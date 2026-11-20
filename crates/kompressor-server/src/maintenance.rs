//! Maintenance: what an operator does to the deployment rather than reads
//! from it. For now, one thing — a backup.
//!
//! `GET /v1/maintenance` says which databases there are and whether this
//! caller may back them up; opened in a browser it is the Maintenance page,
//! which is also at `/maintenance` on the statistics listener.
//! `POST /v1/maintenance/backup` answers with the archive
//! ([`kompressor_core::backup`]): a gzipped tar of a consistent snapshot of
//! every database, as an attachment.
//!
//! `GET /v1/maintenance/backups/{name}` sends one of the archives the
//! schedule wrote (`maintenance.backup_dir`), under the same rule as taking
//! one.
//!
//! The status also describes each database as SQLite sees it — size, the
//! write-ahead log, the schema it holds against the one this build expects,
//! the space a vacuum would reclaim, the span of what it records — and two
//! actions work on one: `POST /v1/maintenance/databases/{name}/check` runs a
//! full integrity check, and `…/checkpoint` folds the write-ahead log into
//! the file. Describing needs no setting; the actions need
//! `maintenance.actions` and the caller a backup needs.
//!
//! # Who may take one
//!
//! The archive is every tenant's statistics and every stored fragment, so it
//! is not scoped to a tenant and a tenant's key does not unlock it:
//!
//! - `maintenance.backup` must be on. It is off by default.
//! - Under `auth.mode = "none"` there is one tenant and anyone who reaches
//!   the listener is it. Otherwise the caller must present the shared secret
//!   (`x-kompressor-secret`), the credential that may already act for any
//!   tenant.
//!
//! # How it is served
//!
//! The archive is written to a file beside the databases and streamed from
//! there, so a backup of a database far larger than memory holds a few
//! hundred kilobytes of it at a time; the file is removed when the last byte
//! has gone or the client has. One backup runs at a time: a second request
//! while one is being written is refused with 409 rather than queued behind
//! a job that reads every page of every database.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use kompressor_core::backup;
use kompressor_core::config::{AuthMode, Config, RefsBackend};
use serde::{Deserialize, Serialize};

use crate::api::authenticate;
use crate::error::ApiError;
use crate::middleware::json_response;
use crate::state::ServerState;

/// The media type of the archive.
pub const CONTENT_TYPE: &str = "application/gzip";

/// How much of the archive is read from disk at a time while it is sent.
const CHUNK: usize = 256 * 1024;

/// The one backup this server may be writing, released however it ends.
struct Slot(Arc<ServerState>);

impl Slot {
    fn take(state: &Arc<ServerState>) -> Option<Slot> {
        state
            .backup_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Slot(state.clone()))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.backup_running.store(false, Ordering::Release);
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct MaintenanceQuery {
    format: Option<String>,
}

/// What an integrity check of one database found.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Checked {
    at_ms: i64,
    took_ms: u64,
    ok: bool,
    /// SQLite's own account of what is wrong; empty when nothing is.
    problems: Vec<String>,
}

/// One database, as the status lists it.
#[derive(Debug, Default, Serialize)]
struct DatabaseStatus {
    name: &'static str,
    /// Where it is kept; absent for one that has no file.
    path: Option<String>,
    /// Its size on disk, write-ahead log included; absent when there is none.
    bytes: Option<u64>,
    /// The part of `bytes` that is the write-ahead log: written, and not yet
    /// folded into the file.
    wal_bytes: Option<u64>,
    /// Whether a backup taken now would hold it.
    included: bool,
    /// Why not, when it would not.
    reason: Option<String>,
    /// The schema the file holds, and the one this build brings a database
    /// to. They differ only on a file a newer build has written.
    schema_version: Option<u32>,
    expected_schema_version: u32,
    /// Bytes a `VACUUM` would hand back to the filesystem.
    reclaimable_bytes: Option<u64>,
    /// The times of the oldest and newest request it records. Statistics
    /// only.
    oldest_ms: Option<i64>,
    newest_ms: Option<i64>,
    /// What the last integrity check since this process started found.
    checked: Option<Checked>,
    /// Why the file could not be described, when it could not.
    error: Option<String>,
}

/// Whether something is on, and whether this caller may use it.
#[derive(Debug, Serialize)]
struct Grant {
    enabled: bool,
    allowed: bool,
    /// Why not, when it may not.
    reason: Option<String>,
}

/// The backup schedule and what it has written.
#[derive(Debug, Serialize)]
struct ScheduleStatus {
    dir: String,
    /// Seconds between backups; zero when the directory has no schedule.
    every_s: u64,
    keep: u32,
    /// When the next one is due; absent with no schedule.
    next_due_ms: Option<i64>,
    /// Newest first.
    archives: Vec<backup::Archive>,
}

/// When a backup was last taken, and whether that is recent enough.
#[derive(Debug, Default, Serialize)]
struct LastBackup {
    /// When the last one that worked was taken: the newest on record or in
    /// the backup directory.
    ok_ms: Option<i64>,
    /// When one last failed, if that was after the last that worked.
    failed_ms: Option<i64>,
    /// Seconds the last backup may reach before it is stale; absent when the
    /// deployment has not asked for backups.
    max_age_s: Option<u64>,
    /// `ok_ms` is older than `max_age_s`, or there is none and one is
    /// expected.
    stale: bool,
    /// The newest backups on record, newest first. Empty with statistics off.
    history: Vec<kompressor_stats::events::BackupEvent>,
}

/// What `kompressor restore` needs to be told on this deployment.
#[derive(Debug, Serialize)]
struct RestoreHint {
    /// The configuration file the service was started with, when it has one.
    config: Option<String>,
    /// Whether the service is running under systemd, which is how to stop it.
    systemd: bool,
}

/// The JSON body of `GET /v1/maintenance`.
#[derive(Debug, Serialize)]
struct MaintenanceStatus {
    version: &'static str,
    now_ms: i64,
    last_backup: LastBackup,
    restore: RestoreHint,
    backup: Grant,
    actions: Grant,
    /// Absent when `maintenance.backup_dir` is not set.
    schedule: Option<ScheduleStatus>,
    databases: Vec<DatabaseStatus>,
}

/// The maintenance routes, mounted under each version prefix by the API
/// router and by the statistics listener.
pub(crate) fn maintenance_routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/maintenance", get(status))
        .route("/maintenance/backup", post(take_backup))
        .route("/maintenance/backups/{name}", get(stored_backup))
        .route("/maintenance/databases/{name}/check", post(check))
        .route("/maintenance/databases/{name}/checkpoint", post(checkpoint))
}

/// The page at `/maintenance` on the statistics listener.
pub(crate) async fn page_on_stats_listener() -> Response {
    page(true)
}

fn page(listener: bool) -> Response {
    // As on the other pages: the settings reach the script as one JSON
    // object, so nothing from the request is spliced in as markup.
    let settings = serde_json::json!({ "listener": listener })
        .to_string()
        .replace('<', "\\u003c");
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        PAGE.replace("/*SETTINGS*/{}", &settings),
    )
        .into_response()
}

const PAGE: &str = include_str!("maintenance.html");

/// Something the page can do that is not every caller's to do.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wanted {
    /// Take or fetch a backup: `maintenance.backup`.
    Backup,
    /// Check or checkpoint a database: `maintenance.actions`.
    Action,
}

impl Wanted {
    fn enabled(self, config: &Config) -> bool {
        match self {
            Wanted::Backup => config.maintenance.backup,
            Wanted::Action => config.maintenance.actions,
        }
    }
}

/// Why this caller may not do `wanted`, or `None` when it may.
///
/// `trusted` is whether it presented the shared secret.
fn refusal(wanted: Wanted, config: &Config, trusted: bool) -> Option<String> {
    if !wanted.enabled(config) {
        return Some(match wanted {
            Wanted::Backup => "backup over HTTP is off on this deployment; set \
                 maintenance.backup = true (or KOMPRESSOR__MAINTENANCE__BACKUP=true) to turn \
                 it on, or run `kompressor backup` on the host"
                .to_string(),
            Wanted::Action => "maintenance actions over HTTP are off on this deployment; set \
                 maintenance.actions = true (or KOMPRESSOR__MAINTENANCE__ACTIONS=true) to turn \
                 them on, or run `kompressor db check` and `kompressor db checkpoint` on the host"
                .to_string(),
        });
    }
    if config.auth.mode != AuthMode::None && !trusted {
        return Some(match wanted {
            Wanted::Backup => "a backup holds every tenant's data, so a tenant's key does not \
                 unlock it: present the shared secret (x-kompressor-secret)"
                .to_string(),
            Wanted::Action => "this works on every tenant's database, so a tenant's key does \
                 not unlock it: present the shared secret (x-kompressor-secret)"
                .to_string(),
        });
    }
    None
}

fn grant(wanted: Wanted, config: &Config, trusted: bool) -> Grant {
    let reason = refusal(wanted, config, trusted);
    Grant {
        enabled: wanted.enabled(config),
        allowed: reason.is_none(),
        reason,
    }
}

/// Authenticate, and refuse a caller who may not do `wanted`: 404 with how
/// to turn it on when it is off, 403 when it is on and not this caller's.
fn permit(state: &ServerState, headers: &HeaderMap, wanted: Wanted) -> Result<(), ApiError> {
    let identity = authenticate(state, headers)?;
    let config = state.core.config();
    match refusal(wanted, config, identity.trusted) {
        None => Ok(()),
        Some(reason) if !wanted.enabled(config) => Err(ApiError::NotFound(reason)),
        Some(_) => {
            tracing::warn!(tenant = %identity.tenant.as_str(), "a maintenance request was refused");
            Err(ApiError::Forbidden)
        }
    }
}

/// A file's size, and its write-ahead log's, which holds the newest rows.
fn sizes_on_disk(path: &Path) -> Option<(u64, u64)> {
    let main = std::fs::metadata(path).ok()?.len();
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    Some((
        main,
        std::fs::metadata(PathBuf::from(wal)).map_or(0, |meta| meta.len()),
    ))
}

/// The databases this deployment keeps on disk, by the name each has on the
/// page and in an archive, with the schema version this build expects.
const STATS_DB: &str = "stats.db";
const REFS_DB: &str = "refs.db";

/// Where the database called `name` is, or why it has no file.
fn located(config: &Config, name: &str) -> Option<Result<(&'static str, PathBuf), &'static str>> {
    match name {
        STATS_DB if config.stats.enabled => Some(Ok((STATS_DB, config.stats.path.clone()))),
        STATS_DB => Some(Err("statistics are off")),
        REFS_DB if config.refs.backend == RefsBackend::Sqlite => {
            Some(Ok((REFS_DB, config.refs.path.clone())))
        }
        REFS_DB => Some(Err("the ref store is in memory")),
        _ => None,
    }
}

fn expected_schema(name: &str) -> u32 {
    if name == STATS_DB {
        kompressor_stats::schema::latest_version()
    } else {
        kompressor_store::sqlite::latest_version()
    }
}

/// Describe one database. Reads the file's header, and for the statistics
/// its first and last row: nothing is scanned.
fn database(config: &Config, name: &'static str) -> DatabaseStatus {
    let mut status = DatabaseStatus {
        name,
        expected_schema_version: expected_schema(name),
        ..DatabaseStatus::default()
    };
    let path = match located(config, name) {
        Some(Ok((_, path))) => path,
        Some(Err(reason)) => {
            status.reason = Some(reason.to_string());
            return status;
        }
        None => return status,
    };
    status.path = Some(path.display().to_string());
    let Some((main, wal)) = sizes_on_disk(&path) else {
        status.reason = Some(format!("{} does not exist yet", path.display()));
        return status;
    };
    status.bytes = Some(main + wal);
    status.wal_bytes = Some(wal);
    status.included = true;
    match kompressor_store::sqlite::facts(&path) {
        Ok(facts) => {
            status.schema_version = Some(facts.schema_version);
            status.reclaimable_bytes = Some(facts.reclaimable_bytes());
        }
        Err(error) => status.error = Some(error.to_string()),
    }
    if name == STATS_DB
        && let Ok(Some((oldest, newest))) = kompressor_stats::schema::time_span(&path)
    {
        status.oldest_ms = Some(oldest);
        status.newest_ms = Some(newest);
    }
    status
}

fn databases(config: &Config) -> Vec<DatabaseStatus> {
    vec![database(config, STATS_DB), database(config, REFS_DB)]
}

fn schedule(config: &Config, now_ms: i64) -> Option<ScheduleStatus> {
    let maintenance = &config.maintenance;
    if maintenance.backup_dir.as_os_str().is_empty() {
        return None;
    }
    let dir = &maintenance.backup_dir;
    Some(ScheduleStatus {
        dir: dir.display().to_string(),
        every_s: maintenance
            .schedule()
            .map_or(0, |(_, every)| every.as_secs()),
        keep: maintenance.backup_keep,
        next_due_ms: maintenance
            .schedule()
            .map(|(dir, every)| backup::next_due_ms(dir, every, now_ms)),
        archives: backup::list_archives(dir),
    })
}

fn wants_html(query: &MaintenanceQuery, headers: &HeaderMap) -> Result<bool, ApiError> {
    match query.format.as_deref() {
        Some("json") => Ok(false),
        Some("html") => Ok(true),
        Some(other) => Err(ApiError::Invalid(format!(
            "format must be json or html, not {other}"
        ))),
        None => Ok(crate::live::prefers_html(headers)),
    }
}

async fn status(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<MaintenanceQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let Query(query) = query
        .map_err(|rejection| ApiError::Invalid(format!("query: {}", rejection.body_text())))?;
    // The shell goes to anyone, as every page's does: a navigation cannot
    // carry a credential, and the page says nothing until its fetch has.
    if wants_html(&query, &headers)? {
        return Ok(page(false));
    }
    let identity = authenticate(&state, &headers)?;
    let core = state.core.clone();
    // Off the runtime: it opens each database and lists a directory.
    let mut status = tokio::task::spawn_blocking(move || {
        let config = core.config();
        let now_ms = now_ms();
        MaintenanceStatus {
            version: env!("CARGO_PKG_VERSION"),
            now_ms,
            last_backup: LastBackup::default(),
            restore: RestoreHint {
                config: config
                    .source
                    .as_ref()
                    .map(|path| path.display().to_string()),
                // systemd sets this for every unit it starts.
                systemd: std::env::var_os("INVOCATION_ID").is_some(),
            },
            backup: grant(Wanted::Backup, config, identity.trusted),
            actions: grant(Wanted::Action, config, identity.trusted),
            schedule: schedule(config, now_ms),
            databases: databases(config),
        }
    })
    .await
    .map_err(|error| ApiError::Internal(format!("maintenance status: {error}")))?;
    // What is on record: none with statistics off, and then the directory
    // alone says when the last one was.
    let history = state
        .with_stats(|reader| reader.backups(10))
        .await
        .unwrap_or_default();
    status.last_backup = last_backup(
        state.core.config(),
        history,
        status.schedule.as_ref(),
        status.now_ms,
    );
    let checks = state
        .integrity
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for database in &mut status.databases {
        database.checked = checks.get(database.name).cloned();
    }
    drop(checks);
    Ok(json_response(&status))
}

fn last_backup(
    config: &Config,
    history: Vec<kompressor_stats::events::BackupEvent>,
    schedule: Option<&ScheduleStatus>,
    now_ms: i64,
) -> LastBackup {
    let recorded = history.iter().find(|backup| backup.ok).map(|b| b.time_ms);
    let on_disk = schedule
        .and_then(|schedule| schedule.archives.first())
        .map(|archive| archive.created_at_ms);
    let ok_ms = recorded.max(on_disk);
    let failed_ms = history
        .first()
        .filter(|newest| !newest.ok && Some(newest.time_ms) > ok_ms)
        .map(|newest| newest.time_ms);
    let max_age = config.maintenance.max_age();
    let limit_ms = max_age.map(|age| i64::try_from(age.as_millis()).unwrap_or(i64::MAX));
    LastBackup {
        ok_ms,
        failed_ms,
        max_age_s: max_age.map(|age| age.as_secs()),
        stale: match (ok_ms, limit_ms) {
            (Some(at), Some(limit)) => now_ms - at > limit,
            (None, Some(_)) => true,
            (_, None) => false,
        },
        history,
    }
}

/// The on-disk database a request names, or the 404 that says why there is
/// none.
fn named(config: &Config, name: &str) -> Result<(&'static str, PathBuf), ApiError> {
    match located(config, name) {
        Some(Ok((name, path))) if path.is_file() => Ok((name, path)),
        Some(Ok((_, path))) => Err(ApiError::NotFound(format!(
            "{} does not exist yet",
            path.display()
        ))),
        Some(Err(reason)) => Err(ApiError::NotFound(format!("{name}: {reason}"))),
        None => Err(ApiError::NotFound(format!(
            "no database called {name:?}; there are {STATS_DB} and {REFS_DB}"
        ))),
    }
}

/// `POST /v1/maintenance/databases/{name}/check`: a full integrity check.
async fn check(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    UrlPath(name): UrlPath<String>,
) -> Result<Response, ApiError> {
    permit(&state, &headers, Wanted::Action)?;
    let (name, path) = named(state.core.config(), &name)?;
    let started = std::time::Instant::now();
    let problems =
        tokio::task::spawn_blocking(move || kompressor_store::sqlite::integrity_check(&path))
            .await
            .map_err(|error| ApiError::Internal(format!("integrity check: {error}")))?;
    // A file SQLite cannot read at all is the worst answer a check can give,
    // and it is an answer: reported as a failed check, not as a server error.
    let problems = problems.unwrap_or_else(|error| vec![error.to_string()]);
    let checked = Checked {
        at_ms: now_ms(),
        took_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        ok: problems.is_empty(),
        problems,
    };
    if checked.ok {
        tracing::info!(
            database = name,
            took_ms = checked.took_ms,
            "integrity check passed"
        );
    } else {
        tracing::error!(
            database = name,
            problems = checked.problems.len(),
            first = checked.problems.first().map(String::as_str),
            "integrity check FAILED"
        );
    }
    state
        .integrity
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(name, checked.clone());
    Ok(json_response(
        &serde_json::json!({"database": name, "checked": checked}),
    ))
}

/// `POST /v1/maintenance/databases/{name}/checkpoint`: fold the write-ahead
/// log into the file.
async fn checkpoint(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    UrlPath(name): UrlPath<String>,
) -> Result<Response, ApiError> {
    permit(&state, &headers, Wanted::Action)?;
    let (name, path) = named(state.core.config(), &name)?;
    let before = sizes_on_disk(&path).map_or(0, |(_, wal)| wal);
    let for_task = path.clone();
    let done = tokio::task::spawn_blocking(move || kompressor_store::sqlite::checkpoint(&for_task))
        .await
        .map_err(|error| ApiError::Internal(format!("checkpoint: {error}")))?
        .map_err(|error| ApiError::Internal(format!("checkpoint of {name}: {error}")))?;
    let after = sizes_on_disk(&path).map_or(0, |(_, wal)| wal);
    tracing::info!(
        database = name,
        complete = done.complete,
        wal_before = before,
        wal_after = after,
        "checkpoint"
    );
    Ok(json_response(&serde_json::json!({
        "database": name,
        "complete": done.complete,
        "log_frames": done.log_frames,
        "moved_frames": done.moved_frames,
        "wal_bytes_before": before,
        "wal_bytes_after": after,
    })))
}

/// `GET /v1/maintenance/backups/{name}`: one of the scheduled archives.
async fn stored_backup(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    UrlPath(name): UrlPath<String>,
) -> Result<Response, ApiError> {
    permit(&state, &headers, Wanted::Backup)?;
    let config = state.core.config();
    // Only a name `archive_name` writes is ever opened: no separator, no
    // `..`, nothing else that happens to be in the directory.
    let missing = || ApiError::NotFound(format!("no backup called {name:?}"));
    if config.maintenance.backup_dir.as_os_str().is_empty() || backup::archive_time(&name).is_none()
    {
        return Err(missing());
    }
    let path = config.maintenance.backup_dir.join(&name);
    let opened = tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&path)?;
        let meta = file.metadata()?;
        Ok::<_, std::io::Error>((file, meta.is_file(), meta.len()))
    })
    .await
    .map_err(|error| ApiError::Internal(format!("backup download: {error}")))?;
    match opened {
        Ok((file, true, bytes)) => Ok(attachment(name, None, file, bytes)),
        _ => Err(missing()),
    }
}

/// Where the archive waits between being written and being sent: beside the
/// first database there is, on its filesystem and inside the one directory
/// the service is sure to be able to write to.
fn spool_dir(config: &Config) -> PathBuf {
    let beside = |path: &Path| {
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
    };
    let stats = config
        .stats
        .enabled
        .then(|| beside(&config.stats.path))
        .flatten();
    let refs = (config.refs.backend == RefsBackend::Sqlite)
        .then(|| beside(&config.refs.path))
        .flatten();
    stats.or(refs).unwrap_or_else(std::env::temp_dir)
}

/// Removes the spooled archive when it has been sent, or will not be.
struct Spooled(PathBuf);

impl Drop for Spooled {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

async fn take_backup(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    permit(&state, &headers, Wanted::Backup)?;
    let Some(slot) = Slot::take(&state) else {
        return Ok((
            StatusCode::CONFLICT,
            json_response(&serde_json::json!({"error": {
                "code": "backup_running",
                "message": "a backup is already being written; wait for it to finish",
            }})),
        )
            .into_response());
    };

    let core = state.core.clone();
    let written = tokio::task::spawn_blocking(move || {
        // Held to here, and no further: sending the file needs no database.
        let _slot = slot;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let config = core.config();
        let dir = spool_dir(config);
        backup::remove_stale_scratch(&dir);
        // What is still queued belongs in the snapshot. Best effort: a sink
        // that cannot flush leaves the last moments out, not the backup.
        let _ = core.stats().flush();
        let spooled = Spooled(dir.join(format!(
            ".kompressor-backup-{}-http{}.tar.gz",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        let manifest = backup::write_archive(config, &spooled.0)?;
        let file = std::fs::File::open(&spooled.0)?;
        let bytes = file.metadata()?.len();
        Ok::<_, kompressor_core::CoreError>((manifest, spooled, file, bytes))
    })
    .await
    .map_err(|error| ApiError::Internal(format!("backup task: {error}")))?;
    // On record either way: "when was the last backup" is answered from
    // these, and a failure is the more useful half of that answer.
    let record = |bytes: u64, databases: usize, error: Option<String>| {
        state.core.stats().send(kompressor_stats::Event::Backup(
            kompressor_stats::events::BackupEvent {
                time_ms: now_ms(),
                origin: "http".to_string(),
                destination: "download".to_string(),
                bytes: i64::try_from(bytes).unwrap_or(i64::MAX),
                databases: databases as i64,
                ok: error.is_none(),
                error,
            },
        ));
    };
    let (manifest, spooled, file, bytes) = match written {
        Ok(written) => written,
        Err(kompressor_core::CoreError::BadRequest(reason)) => {
            return Err(ApiError::NotFound(reason));
        }
        Err(error) => {
            record(0, 0, Some(error.to_string()));
            return Err(ApiError::Internal(format!("backup: {error}")));
        }
    };
    record(bytes, manifest.databases.len(), None);
    tracing::info!(
        bytes,
        databases = manifest.databases.len(),
        "backup written; sending it"
    );
    let name = backup::archive_name(if manifest.created_at_ms > 0 {
        manifest.created_at_ms
    } else {
        now_ms()
    });
    Ok(attachment(name, Some(spooled), file, bytes))
}

/// An archive as a streamed attachment called `name`. `spooled` is the file
/// to remove once it has been sent, for one written only to be sent.
fn attachment(name: String, spooled: Option<Spooled>, file: std::fs::File, bytes: u64) -> Response {
    // Read on a thread of its own and handed over a short queue: the file is
    // read no faster than the client takes it, and never whole.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    std::thread::spawn(move || {
        // Dropped with the thread, whichever way it ends: sent in full, or
        // the client gone and the queue closed.
        let _spooled = spooled;
        let mut file = file;
        loop {
            let mut chunk = vec![0; CHUNK];
            let read = match file.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) => {
                    let _ = tx.blocking_send(Err(error));
                    break;
                }
            };
            chunk.truncate(read);
            if tx.blocking_send(Ok(Bytes::from(chunk))).is_err() {
                break;
            }
        }
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|chunk| (chunk, rx))
    });

    let mut response = Body::from_stream(stream).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(bytes));
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backup_needs_the_setting_and_a_caller_who_acts_for_everyone() {
        let mut config = Config::default();
        for (wanted, setting) in [
            (Wanted::Backup, "maintenance.backup"),
            (Wanted::Action, "maintenance.actions"),
        ] {
            let mut config = config.clone();
            assert!(refusal(wanted, &config, true).unwrap().contains(setting));
            assert!(!grant(wanted, &config, true).enabled);
            config.maintenance.backup = wanted == Wanted::Backup;
            config.maintenance.actions = wanted == Wanted::Action;
            assert_eq!(
                refusal(wanted, &config, false),
                None,
                "one tenant: anyone is it"
            );
            config.auth.mode = AuthMode::ApiKeys;
            let said = refusal(wanted, &config, false).unwrap();
            assert!(said.contains("shared secret"), "{said}");
            assert!(grant(wanted, &config, false).enabled);
            assert_eq!(refusal(wanted, &config, true), None);
        }
        // One does not grant the other.
        config.maintenance.backup = true;
        assert!(refusal(Wanted::Action, &config, true).is_some());
    }

    #[test]
    fn one_backup_at_a_time() {
        let mut config = Config::default();
        config.refs.backend = RefsBackend::Memory;
        config.stats.enabled = false;
        let refs = kompressor_core::build_ref_store(&config).unwrap();
        let core = Arc::new(kompressor_core::AppCore::new(
            config,
            refs,
            kompressor_stats::null_sink(),
        ));
        let state = ServerState::new(core);
        let first = Slot::take(&state).expect("nothing is running");
        assert!(Slot::take(&state).is_none());
        drop(first);
        assert!(Slot::take(&state).is_some(), "and the slot comes back");
    }

    #[test]
    fn the_status_says_what_a_backup_would_hold() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.stats.path = dir.path().join("stats.db");
        config.refs.backend = RefsBackend::Memory;
        std::fs::write(&config.stats.path, [0; 100]).unwrap();
        std::fs::write(dir.path().join("stats.db-wal"), [0; 30]).unwrap();
        let listed = databases(&config);
        assert_eq!(
            (listed[0].name, listed[0].bytes, listed[0].included),
            ("stats.db", Some(130), true)
        );
        assert_eq!(listed[0].wal_bytes, Some(30));
        assert!(
            listed[0].error.is_some() && listed[0].schema_version.is_none(),
            "a file SQLite cannot read is listed, with what it said"
        );
        assert_eq!(
            listed[0].expected_schema_version,
            kompressor_stats::schema::latest_version()
        );
        assert_eq!(
            (listed[1].path.as_deref(), listed[1].included),
            (None, false)
        );
        assert!(
            schedule(&config, 0).is_none(),
            "no directory, nothing to list"
        );
        assert!(located(&config, "other.db").is_none());
        assert_eq!(
            listed[1].reason.as_deref(),
            Some("the ref store is in memory")
        );

        config.refs.backend = RefsBackend::Sqlite;
        config.refs.path = dir.path().join("refs.db");
        let listed = databases(&config);
        assert!(!listed[1].included);
        assert!(
            listed[1]
                .reason
                .as_deref()
                .unwrap()
                .ends_with("does not exist yet")
        );
        assert_eq!(spool_dir(&config), dir.path());
        config.stats.enabled = false;
        assert_eq!(spool_dir(&config), dir.path(), "beside the ref store then");
    }
}
