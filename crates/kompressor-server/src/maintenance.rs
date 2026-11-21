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
//! `maintenance.actions` and the caller a backup needs. So do the two that
//! write: `…/vacuum` rebuilds the file without its free pages, after checking
//! the disk has room for the rebuild, and `…/optimize` gathers the statistics
//! the query planner chooses indexes by. `GET …/space` says where a
//! database's bytes are, table by table; it reads every page, so the page
//! asks for it only when someone opens it.
//!
//! The status also says what keeps the databases from growing without bound
//! — the retention pass over the statistics and the sweep of expired
//! fragments: when each last ran in this process and what it removed — and
//! how much room the disks under the databases and the backups have left.
//!
//! # The danger zone
//!
//! `POST /v1/maintenance/statistics/erase` deletes every statistic, for
//! every tenant, daily rollups included, and keeps only the record of
//! backups. It needs `maintenance.erase`, the caller a backup needs, and a
//! body that spells the request out — `{"confirm": "nuke all stats"}` — so
//! that no stray or replayed POST is one. The page asks twice before it
//! sends that.
//!
//! A backup, a vacuum and an erase each read or rewrite a whole database:
//! one of them runs at a time, and a second is refused with 409. The two
//! that rewrite `stats.db` run while the statistics writer stands aside
//! (`StatsSink::exclusive`), so what arrives meanwhile waits in its queue
//! rather than failing against a locked file.
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
use kompressor_core::AppCore;
use kompressor_core::auth::Identity;
use kompressor_core::backup;
use kompressor_core::config::{AuthMode, Config, RefsBackend};
use kompressor_stats::MaintenanceEvent;
use kompressor_stats::purge::{PurgeKind, PurgeScope};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::api::authenticate;
use crate::error::ApiError;
use crate::middleware::json_response;
use crate::state::ServerState;

/// The media type of the archive.
pub const CONTENT_TYPE: &str = "application/gzip";

/// How much of the archive is read from disk at a time while it is sent.
const CHUNK: usize = 256 * 1024;

/// The one heavy job this server may be running — a backup, a vacuum, an
/// erase — released however it ends.
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
    /// Whether the query planner has statistics to choose indexes by: false
    /// until an optimize has run on the file.
    analyzed: Option<bool>,
    /// `PRAGMA auto_vacuum`: 0 none, 1 full, 2 incremental. Incremental lets
    /// freed pages go back to the disk without a full vacuum.
    auto_vacuum: Option<u8>,
    /// The last of each kind of work on record for it — `check`,
    /// `checkpoint`, `vacuum`, `optimize`, `erase`, `delete`, `trim` — by
    /// action. Across restarts; empty with statistics off.
    last: BTreeMap<String, MaintenanceEvent>,
}

/// A filesystem this deployment writes to.
#[derive(Debug, Serialize)]
struct Volume {
    /// `data` for the one under the databases, `backups` for the one under
    /// `maintenance.backup_dir`.
    role: &'static str,
    path: String,
    /// Absent where the platform would not say.
    free_bytes: Option<u64>,
    total_bytes: Option<u64>,
}

/// How much room is left, and how fast it is going.
#[derive(Debug, Default, Serialize)]
struct Storage {
    volumes: Vec<Volume>,
    /// Whether the backups are kept on the filesystem they back up. Absent
    /// with no backup directory, or where the platform would not say.
    backups_share_disk: Option<bool>,
    /// What `stats.db` has grown by per day over the span it records: an
    /// estimate, absent until it holds a day of requests.
    growth_bytes_per_day: Option<u64>,
    /// Where that levels off: the size of a full retention window. Absent
    /// with retention off, when nothing levels off.
    projected_bytes: Option<u64>,
    /// Days until the data filesystem is full at that rate. Absent when the
    /// window is already full, or the estimate cannot be made.
    days_left: Option<u64>,
}

/// The retention pass over the statistics.
#[derive(Debug, Serialize)]
struct StatsRetention {
    /// `stats.retention_days`; zero keeps everything.
    retention_days: u32,
    /// Requests older than this are archived into daily rollups and removed.
    cutoff_ms: Option<i64>,
    /// Seconds between passes.
    every_s: u64,
    /// Whole days of requests older than the cutoff still in the file. A
    /// pass archives a bounded number of days, so a long backlog takes
    /// several.
    backlog_days: Option<u64>,
    /// The last pass since this process started.
    last: Option<kompressor_stats::Upkeep>,
    /// `stats.context_retention_days`: requests older than this keep their
    /// row and lose their copy of the settings. Zero is off.
    context_retention_days: u32,
    /// Absent when that is off, or no shorter than the requests' own window.
    context_cutoff_ms: Option<i64>,
    /// Seconds between scheduled optimizes (`maintenance.optimize_every`);
    /// absent when there are none.
    optimize_every_s: Option<u64>,
}

/// The sweep of expired fragments out of the ref store.
#[derive(Debug, Serialize)]
struct RefsRetention {
    every_s: u64,
    /// `refs.max_bytes`; zero is no cap.
    max_bytes: u64,
    live_refs: Option<u64>,
    live_bytes: Option<u64>,
    /// The last sweep since this process started.
    last: Option<kompressor_core::RefsSweep>,
}

/// What keeps the databases from growing without bound.
#[derive(Debug, Serialize)]
struct Retention {
    /// Absent with statistics off.
    stats: Option<StatsRetention>,
    refs: RefsRetention,
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
    /// The last verification on record of each archive, by its name.
    verified: BTreeMap<String, MaintenanceEvent>,
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

/// What is running: the build, since when, and whether the binary on disk is
/// still the one that is.
#[derive(Debug, Serialize)]
struct BuildInfo {
    version: &'static str,
    /// The commit it was built from, when the build could tell.
    commit: Option<&'static str>,
    started_at_ms: i64,
    uptime_s: u64,
    /// The executable this process runs.
    binary: Option<String>,
    /// The file at `binary` has been replaced since this process started —
    /// an upgrade was installed and the service not restarted, so what runs
    /// is not what is installed.
    restart_pending: bool,
    /// The configuration file it was started with, when it has one.
    config: Option<String>,
    systemd: bool,
}

/// The JSON body of `GET /v1/maintenance`.
#[derive(Debug, Serialize)]
struct MaintenanceStatus {
    version: &'static str,
    build: BuildInfo,
    /// Where it listens, how it is set, the process and its load.
    running: crate::running::Running,
    now_ms: i64,
    last_backup: LastBackup,
    restore: RestoreHint,
    backup: Grant,
    actions: Grant,
    erase: Grant,
    /// Whether this caller may ask where a database's bytes are.
    space: Grant,
    retention: Retention,
    storage: Storage,
    /// Absent when `maintenance.backup_dir` is not set.
    schedule: Option<ScheduleStatus>,
    databases: Vec<DatabaseStatus>,
    /// The newest maintenance work on record, newest first.
    history: Vec<MaintenanceEvent>,
}

/// The maintenance routes, mounted under each version prefix by the API
/// router and by the statistics listener.
pub(crate) fn maintenance_routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/maintenance", get(status))
        .route("/maintenance/backup", post(take_backup))
        .route("/maintenance/backups/{name}", get(stored_backup))
        .route("/maintenance/backups/{name}/verify", post(verify))
        .route("/maintenance/config", get(effective_config))
        .route("/maintenance/databases/{name}/check", post(check))
        .route("/maintenance/databases/{name}/checkpoint", post(checkpoint))
        .route("/maintenance/databases/{name}/vacuum", post(vacuum))
        .route("/maintenance/databases/{name}/optimize", post(optimize))
        .route("/maintenance/databases/{name}/space", get(space))
        .route("/maintenance/statistics/erase", post(erase))
        .route("/maintenance/statistics/delete", post(delete))
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
    /// Check, checkpoint, vacuum or optimize a database:
    /// `maintenance.actions`.
    Action,
    /// Delete every statistic: `maintenance.erase`.
    Erase,
    /// Read where a database's bytes are. No setting: it reads nothing out,
    /// but it reads every page, so not for any tenant to ask.
    Space,
}

impl Wanted {
    fn enabled(self, config: &Config) -> bool {
        match self {
            Wanted::Backup => config.maintenance.backup,
            Wanted::Action => config.maintenance.actions,
            Wanted::Erase => config.maintenance.erase,
            Wanted::Space => true,
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
                 them on; `kompressor db check` and `kompressor db checkpoint` on the host need \
                 no setting"
                .to_string(),
            Wanted::Erase => "erasing the statistics over HTTP is off on this deployment; set \
                 maintenance.erase = true (or KOMPRESSOR__MAINTENANCE__ERASE=true) to turn it on"
                .to_string(),
            Wanted::Space => unreachable!("reading the space breakdown has no setting"),
        });
    }
    if config.auth.mode != AuthMode::None && !trusted {
        return Some(match wanted {
            Wanted::Backup => "a backup holds every tenant's data, so a tenant's key does not \
                 unlock it: present the shared secret (x-kompressor-secret)"
                .to_string(),
            Wanted::Action | Wanted::Space => "this works on every tenant's database, so a \
                 tenant's key does not unlock it: present the shared secret (x-kompressor-secret)"
                .to_string(),
            Wanted::Erase => "this deletes every tenant's statistics, so a tenant's key does \
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
fn permit(state: &ServerState, headers: &HeaderMap, wanted: Wanted) -> Result<Identity, ApiError> {
    let identity = authenticate(state, headers)?;
    let config = state.core.config();
    match refusal(wanted, config, identity.trusted) {
        None => Ok(identity),
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
            status.analyzed = Some(facts.analyzed);
            status.auto_vacuum = Some(facts.auto_vacuum);
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
        verified: BTreeMap::new(),
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
    let info = crate::live::server_info(&state);
    let under_way = state.core.runtime().in_flight();
    let load = crate::running::Load {
        requests: under_way.requests,
        streams: under_way.streams,
        engine_busy: info.inflight_limit.saturating_sub(info.inflight_free),
        engine_limit: info.inflight_limit,
        backup_running: state.backup_running.load(Ordering::Acquire),
    };
    // Off the runtime: it opens each database and lists a directory.
    let mut status = tokio::task::spawn_blocking(move || {
        let binary = running_binary(info.started_at_ms);
        let config = core.config();
        let now_ms = now_ms();
        let databases = databases(config);
        MaintenanceStatus {
            version: env!("CARGO_PKG_VERSION"),
            build: BuildInfo {
                version: env!("CARGO_PKG_VERSION"),
                commit: option_env!("KOMPRESSOR_GIT_COMMIT").filter(|commit| !commit.is_empty()),
                started_at_ms: info.started_at_ms,
                uptime_s: info.uptime_s,
                binary: binary.0,
                restart_pending: binary.1,
                config: config
                    .source
                    .as_ref()
                    .map(|path| path.display().to_string()),
                systemd: std::env::var_os("INVOCATION_ID").is_some(),
            },
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
            running: crate::running::running(&core, load),
            backup: grant(Wanted::Backup, config, identity.trusted),
            actions: grant(Wanted::Action, config, identity.trusted),
            erase: grant(Wanted::Erase, config, identity.trusted),
            space: grant(Wanted::Space, config, identity.trusted),
            retention: retention(&core, &databases, now_ms),
            storage: storage(config, &databases),
            schedule: schedule(config, now_ms),
            databases,
            history: Vec::new(),
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
    let (latest, history) = state
        .with_stats(|reader| Ok((reader.latest_maintenance()?, reader.maintenance(20)?)))
        .await
        .unwrap_or_default();
    status.history = history;
    for work in latest {
        if work.action == "verify" {
            if let Some(schedule) = status.schedule.as_mut() {
                schedule.verified.insert(work.target.clone(), work);
            }
        } else if let Some(database) = status
            .databases
            .iter_mut()
            .find(|database| database.name == work.target)
        {
            database.last.insert(work.action.clone(), work);
        }
    }
    {
        let checks = state
            .integrity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for database in &mut status.databases {
            // This process's own check, or failing that the last on record.
            database.checked = checks
                .get(database.name)
                .cloned()
                .or_else(|| database.last.get("check").map(checked_from_record));
        }
    }
    if let Some(sample) = state.store_sample().await {
        status.retention.refs.live_refs = Some(sample.live_refs);
        status.retention.refs.live_bytes = Some(sample.live_bytes);
    }
    Ok(json_response(&status))
}

const DAY_MS: i64 = 86_400_000;

/// The statistics writer's view of the configuration: the windows it prunes
/// and trims by.
fn stats_config(config: &Config) -> kompressor_stats::StatsConfig {
    kompressor_stats::StatsConfig {
        retention_days: config.stats.retention_days,
        context_retention_days: config.stats.context_retention_days,
        ..kompressor_stats::StatsConfig::at(&config.stats.path)
    }
}

/// The executable this process runs, and whether it has been replaced since
/// `started_at_ms`. Linux names a replaced one `… (deleted)`; elsewhere a file
/// newer than the process says the same.
fn running_binary(started_at_ms: i64) -> (Option<String>, bool) {
    let Ok(exe) = std::env::current_exe() else {
        return (None, false);
    };
    let shown = exe.display().to_string();
    if let Some(path) = shown.strip_suffix(" (deleted)") {
        return (Some(path.to_string()), true);
    }
    let newer = std::fs::metadata(&exe)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .is_some_and(|at| i64::try_from(at.as_millis()).unwrap_or(i64::MAX) > started_at_ms);
    (Some(shown), newer)
}

/// An integrity check as it was put on record.
fn checked_from_record(work: &MaintenanceEvent) -> Checked {
    let problems = work
        .detail
        .as_deref()
        .and_then(|detail| serde_json::from_str::<serde_json::Value>(detail).ok())
        .and_then(|detail| serde_json::from_value(detail["problems"].clone()).ok())
        .unwrap_or_else(|| work.error.iter().cloned().collect());
    Checked {
        at_ms: work.time_ms,
        took_ms: u64::try_from(work.took_ms).unwrap_or(0),
        ok: work.ok,
        problems,
    }
}

/// A piece of maintenance work done for `identity`, to be filled in and
/// [`record`]ed.
fn work(
    action: &str,
    target: &str,
    identity: &Identity,
    started: std::time::Instant,
) -> MaintenanceEvent {
    MaintenanceEvent {
        time_ms: now_ms(),
        action: action.to_string(),
        target: target.to_string(),
        origin: "http".to_string(),
        // No one in particular under `auth.mode = "none"`: the one tenant
        // there has no name.
        actor: Some(identity.tenant.as_str().to_string()).filter(|tenant| !tenant.is_empty()),
        took_ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
        ok: true,
        bytes_before: None,
        bytes_after: None,
        detail: None,
        error: None,
    }
}

/// Put `work` on record. With statistics off there is no record to keep.
fn record(state: &ServerState, work: MaintenanceEvent) {
    state
        .core
        .stats()
        .send(kompressor_stats::Event::Maintenance(work));
}

fn bytes(n: u64) -> Option<i64> {
    Some(i64::try_from(n).unwrap_or(i64::MAX))
}

fn retention(core: &AppCore, databases: &[DatabaseStatus], now_ms: i64) -> Retention {
    let config = core.config();
    let stats = config.stats.enabled.then(|| {
        let days = config.stats.retention_days;
        // Whole UTC days, as the pass itself cuts.
        let cutoff_ms =
            (days > 0).then(|| (now_ms - i64::from(days) * DAY_MS).div_euclid(DAY_MS) * DAY_MS);
        let oldest_ms = databases
            .iter()
            .find(|database| database.name == STATS_DB)
            .and_then(|database| database.oldest_ms);
        StatsRetention {
            retention_days: days,
            cutoff_ms,
            every_s: kompressor_stats::writer::UPKEEP_INTERVAL_S,
            // No request on record is no backlog.
            backlog_days: cutoff_ms.map(|cutoff| {
                oldest_ms.map_or(0, |oldest| {
                    u64::try_from((cutoff - oldest + DAY_MS - 1).div_euclid(DAY_MS)).unwrap_or(0)
                })
            }),
            last: core.stats().upkeep(),
            context_retention_days: config.stats.context_retention_days,
            context_cutoff_ms: stats_config(config).context_cutoff_ms(now_ms),
            optimize_every_s: (!config.maintenance.optimize_every.is_zero())
                .then_some(config.maintenance.optimize_every.as_secs()),
        }
    });
    Retention {
        stats,
        refs: RefsRetention {
            every_s: config.refs.purge_interval.as_secs(),
            max_bytes: config.refs.max_bytes,
            live_refs: None,
            live_bytes: None,
            last: core.refs_sweep(),
        },
    }
}

/// The directory a file is in, as something a filesystem can be asked about.
fn directory_of(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

fn volume(role: &'static str, dir: &Path) -> Volume {
    Volume {
        role,
        path: dir.display().to_string(),
        free_bytes: fs4::available_space(dir).ok(),
        total_bytes: fs4::total_space(dir).ok(),
    }
}

/// Whether two directories are on one filesystem, where that can be told.
fn same_filesystem(a: &Path, b: &Path) -> Option<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(std::fs::metadata(a).ok()?.dev() == std::fs::metadata(b).ok()?.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        None
    }
}

/// The directory the databases are in: the statistics' when there are any,
/// since that is the file that grows.
fn data_dir(config: &Config) -> Option<PathBuf> {
    if config.stats.enabled {
        Some(directory_of(&config.stats.path))
    } else if config.refs.backend == RefsBackend::Sqlite {
        Some(directory_of(&config.refs.path))
    } else {
        None
    }
}

fn storage(config: &Config, databases: &[DatabaseStatus]) -> Storage {
    let mut storage = Storage::default();
    let Some(data) = data_dir(config) else {
        return storage;
    };
    storage.volumes.push(volume("data", &data));
    let backups = &config.maintenance.backup_dir;
    if !backups.as_os_str().is_empty() && backups.is_dir() {
        storage.backups_share_disk = same_filesystem(&data, backups);
        if storage.backups_share_disk != Some(true) {
            storage.volumes.push(volume("backups", backups));
        }
    }
    let stats = databases.iter().find(|database| database.name == STATS_DB);
    let figures = stats.and_then(|stats| Some((stats.bytes?, stats.oldest_ms?, stats.newest_ms?)));
    if let Some((bytes, oldest, newest)) = figures
        && newest - oldest >= DAY_MS
    {
        let span_days = (newest - oldest) as f64 / DAY_MS as f64;
        let per_day = (bytes as f64 / span_days) as u64;
        storage.growth_bytes_per_day = Some(per_day);
        let window = u64::from(config.stats.retention_days);
        let projected = (window > 0).then(|| per_day.saturating_mul(window));
        storage.projected_bytes = projected;
        // Past the window the file stops growing; before it, the disk has
        // to hold what is still to come.
        let still_growing = projected.is_none_or(|projected| bytes < projected);
        if let Some(free) = storage.volumes[0].free_bytes
            && still_growing
            && per_day > 0
        {
            storage.days_left = Some(free / per_day);
        }
    }
    storage
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
    let identity = permit(&state, &headers, Wanted::Action)?;
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
    record(
        &state,
        MaintenanceEvent {
            ok: checked.ok,
            detail: Some(serde_json::json!({"problems": checked.problems}).to_string()),
            error: checked.problems.first().cloned(),
            ..work("check", name, &identity, started)
        },
    );
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
    let identity = permit(&state, &headers, Wanted::Action)?;
    let (name, path) = named(state.core.config(), &name)?;
    let before = sizes_on_disk(&path).map_or(0, |(_, wal)| wal);
    let for_task = path.clone();
    let started = std::time::Instant::now();
    let done = tokio::task::spawn_blocking(move || kompressor_store::sqlite::checkpoint(&for_task))
        .await
        .map_err(|error| ApiError::Internal(format!("checkpoint: {error}")))?
        .map_err(|error| {
            record(
                &state,
                MaintenanceEvent {
                    ok: false,
                    error: Some(error.to_string()),
                    ..work("checkpoint", name, &identity, started)
                },
            );
            ApiError::Internal(format!("checkpoint of {name}: {error}"))
        })?;
    let after = sizes_on_disk(&path).map_or(0, |(_, wal)| wal);
    record(
        &state,
        MaintenanceEvent {
            ok: done.complete,
            bytes_before: bytes(before),
            bytes_after: bytes(after),
            detail: Some(serde_json::json!({"complete": done.complete}).to_string()),
            ..work("checkpoint", name, &identity, started)
        },
    );
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

/// The 409 a heavy job gets while another is running.
fn busy() -> Response {
    (
        StatusCode::CONFLICT,
        json_response(&serde_json::json!({"error": {
            "code": "maintenance_running",
            "message": "a backup, vacuum, delete, erase or verification is already running; \
                        wait for it to finish",
        }})),
    )
        .into_response()
}

/// Run `job` on the database called `name` as its only writer: for the
/// statistics, while the writer thread stands aside.
fn alone<T, F>(core: &AppCore, name: &str, job: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    if name == STATS_DB {
        core.stats()
            .exclusive(job)
            .map_err(|error| error.to_string())
    } else {
        Ok(job())
    }
}

/// How much free space a vacuum is given before it is started: the rebuilt
/// copy, and the same again while it waits in the write-ahead log.
fn vacuum_needs(database_bytes: u64) -> u64 {
    database_bytes.saturating_mul(2)
}

/// `POST /v1/maintenance/databases/{name}/vacuum`: rebuild the file without
/// its free pages.
async fn vacuum(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    UrlPath(name): UrlPath<String>,
) -> Result<Response, ApiError> {
    let identity = permit(&state, &headers, Wanted::Action)?;
    let (name, path) = named(state.core.config(), &name)?;
    let Some(slot) = Slot::take(&state) else {
        return Ok(busy());
    };
    let size = sizes_on_disk(&path).map_or(0, |(main, _)| main);
    let needs = vacuum_needs(size);
    // Where the platform will not say how much is free, the vacuum is tried:
    // SQLite fails it cleanly, and leaves the file as it was, if it runs out.
    if let Ok(free) = fs4::available_space(directory_of(&path))
        && free < needs
    {
        return Ok((
            StatusCode::INSUFFICIENT_STORAGE,
            json_response(&serde_json::json!({"error": {
                "code": "disk_full",
                "message": format!(
                    "a vacuum of {name} needs about {needs} bytes free beside it and there are {free}"
                ),
            }})),
        )
            .into_response());
    }
    let core = state.core.clone();
    let started = std::time::Instant::now();
    let done = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        alone(&core, name, move || {
            // The ref store is converted to incremental auto-vacuum on the
            // way, so its sweep hands pages back by itself from then on.
            if name == REFS_DB {
                kompressor_store::sqlite::vacuum_incremental(&path)
            } else {
                kompressor_store::sqlite::vacuum(&path)
            }
        })
    })
    .await
    .map_err(|error| ApiError::Internal(format!("vacuum: {error}")))?
    .and_then(|done| done.map_err(|error| error.to_string()))
    .map_err(|error| {
        record(
            &state,
            MaintenanceEvent {
                ok: false,
                bytes_before: bytes(size),
                error: Some(error.clone()),
                ..work("vacuum", name, &identity, started)
            },
        );
        ApiError::Internal(format!("vacuum of {name}: {error}"))
    })?;
    let took_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    record(
        &state,
        MaintenanceEvent {
            bytes_before: bytes(done.bytes_before),
            bytes_after: bytes(done.bytes_after),
            detail: Some(serde_json::json!({"folded": done.folded}).to_string()),
            ..work("vacuum", name, &identity, started)
        },
    );
    tracing::info!(
        database = name,
        bytes_before = done.bytes_before,
        bytes_after = done.bytes_after,
        folded = done.folded,
        took_ms,
        "vacuum"
    );
    Ok(json_response(&serde_json::json!({
        "database": name,
        "bytes_before": done.bytes_before,
        "bytes_after": done.bytes_after,
        "folded": done.folded,
        "took_ms": took_ms,
    })))
}

/// `POST /v1/maintenance/databases/{name}/optimize`: gather the statistics
/// the query planner chooses indexes by.
async fn optimize(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    UrlPath(name): UrlPath<String>,
) -> Result<Response, ApiError> {
    let identity = permit(&state, &headers, Wanted::Action)?;
    let (name, path) = named(state.core.config(), &name)?;
    let core = state.core.clone();
    let started = std::time::Instant::now();
    let described = tokio::task::spawn_blocking(move || {
        alone(&core, name, move || {
            kompressor_store::sqlite::optimize(&path)
        })
    })
    .await
    .map_err(|error| ApiError::Internal(format!("optimize: {error}")))?
    .and_then(|done| done.map_err(|error| error.to_string()))
    .map_err(|error| {
        record(
            &state,
            MaintenanceEvent {
                ok: false,
                error: Some(error.clone()),
                ..work("optimize", name, &identity, started)
            },
        );
        ApiError::Internal(format!("optimize of {name}: {error}"))
    })?;
    let took_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    record(
        &state,
        MaintenanceEvent {
            detail: Some(serde_json::json!({"described": described}).to_string()),
            ..work("optimize", name, &identity, started)
        },
    );
    tracing::info!(database = name, described, took_ms, "optimize");
    Ok(json_response(&serde_json::json!({
        "database": name,
        "described": described,
        "took_ms": took_ms,
    })))
}

/// `GET /v1/maintenance/databases/{name}/space`: where the bytes are.
async fn space(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    UrlPath(name): UrlPath<String>,
) -> Result<Response, ApiError> {
    permit(&state, &headers, Wanted::Space)?;
    let (name, path) = named(state.core.config(), &name)?;
    let started = std::time::Instant::now();
    let found = tokio::task::spawn_blocking(move || kompressor_store::sqlite::space(&path))
        .await
        .map_err(|error| ApiError::Internal(format!("space: {error}")))?
        .map_err(|error| ApiError::Internal(format!("space of {name}: {error}")))?;
    let objects: Vec<serde_json::Value> = found
        .iter()
        .map(|object| {
            serde_json::json!({
                "name": object.name,
                "kind": object.kind,
                "table": object.table,
                "bytes": object.bytes,
                "unused_bytes": object.unused_bytes,
                "rows": object.rows,
            })
        })
        .collect();
    Ok(json_response(&serde_json::json!({
        "database": name,
        "bytes": found.iter().map(|object| object.bytes).sum::<u64>(),
        "took_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "objects": objects,
    })))
}

/// What an erase must be asked for with, word for word.
pub(crate) const ERASE_PHRASE: &str = "nuke all stats";

#[derive(Debug, Deserialize)]
struct EraseRequest {
    confirm: String,
}

/// `POST /v1/maintenance/statistics/erase`: delete every statistic.
async fn erase(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let identity = permit(&state, &headers, Wanted::Erase)?;
    let asked = serde_json::from_slice::<EraseRequest>(&body)
        .ok()
        .is_some_and(|request| request.confirm == ERASE_PHRASE);
    if !asked {
        return Err(ApiError::Invalid(format!(
            "this deletes every statistic for every tenant and cannot be undone; to go ahead, \
             send {{\"confirm\": \"{ERASE_PHRASE}\"}}"
        )));
    }
    let (name, path) = named(state.core.config(), STATS_DB)?;
    let Some(slot) = Slot::take(&state) else {
        return Ok(busy());
    };
    let core = state.core.clone();
    let started = std::time::Instant::now();
    let erased = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        alone(&core, name, move || kompressor_stats::schema::erase(&path))
    })
    .await
    .map_err(|error| ApiError::Internal(format!("erase: {error}")))?
    .and_then(|done| done.map_err(|error| error.to_string()))
    .map_err(|error| {
        record(
            &state,
            MaintenanceEvent {
                ok: false,
                error: Some(error.clone()),
                ..work("erase", name, &identity, started)
            },
        );
        ApiError::Internal(format!("erase: {error}"))
    })?;
    let took_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    record(
        &state,
        MaintenanceEvent {
            bytes_before: bytes(erased.bytes_before),
            bytes_after: bytes(erased.bytes_after),
            detail: Some(
                serde_json::json!({"rows": erased.rows, "tables": erased.tables}).to_string(),
            ),
            ..work("erase", name, &identity, started)
        },
    );
    // Loud, and on record in the one place the erase cannot reach.
    tracing::warn!(
        rows = erased.rows,
        tables = erased.tables,
        bytes_before = erased.bytes_before,
        bytes_after = erased.bytes_after,
        took_ms,
        "every statistic was erased on request"
    );
    Ok(json_response(&serde_json::json!({
        "erased": erased,
        "took_ms": took_ms,
    })))
}

/// What a scoped delete must be asked for with, word for word.
pub(crate) const DELETE_PHRASE: &str = "delete statistics";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteRequest {
    #[serde(default)]
    kind: PurgeKind,
    scope: PurgeScope,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    confirm: Option<String>,
}

/// `POST /v1/maintenance/statistics/delete`: delete part of the statistics
/// (`"kind": "rows"`) — before a day, for a tenant, a run or an experiment —
/// or empty the copy of the settings those requests carry
/// (`"kind": "context"`). `"dry_run": true` says what it would do.
///
/// Deleting rows is the danger zone's (`maintenance.erase`) and a real one
/// must say `{"confirm": "delete statistics"}`; trimming loses no request
/// and nothing the analysis history reports, and is an action's
/// (`maintenance.actions`).
async fn delete(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let request: DeleteRequest = serde_json::from_slice(&body).map_err(|error| {
        ApiError::Invalid(format!(
            "expected {{\"kind\": \"rows\" | \"context\", \"scope\": {{\"before_ms\", \
             \"tenant\", \"run_id\", \"experiment_id\"}}, \"dry_run\": bool}}: {error}"
        ))
    })?;
    let wanted = match request.kind {
        PurgeKind::Rows => Wanted::Erase,
        PurgeKind::Context => Wanted::Action,
    };
    let identity = permit(&state, &headers, wanted)?;
    if request.kind == PurgeKind::Rows
        && !request.dry_run
        && request.confirm.as_deref() != Some(DELETE_PHRASE)
    {
        return Err(ApiError::Invalid(format!(
            "this deletes statistics and cannot be undone; rehearse it with \"dry_run\": true, \
             and to go ahead send \"confirm\": \"{DELETE_PHRASE}\""
        )));
    }
    let (name, path) = named(state.core.config(), STATS_DB)?;
    let Some(slot) = Slot::take(&state) else {
        return Ok(busy());
    };
    let core = state.core.clone();
    let started = std::time::Instant::now();
    let (kind, dry_run) = (request.kind, request.dry_run);
    let scope = request.scope;
    let done = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        alone(&core, name, move || {
            kompressor_stats::purge::purge(&path, scope, kind, dry_run)
        })
    })
    .await
    .map_err(|error| ApiError::Internal(format!("delete: {error}")))?
    .map_err(|error| ApiError::Internal(format!("delete: {error}")))?;
    let action = match kind {
        PurgeKind::Rows => "delete",
        PurgeKind::Context => "trim",
    };
    let purged = match done {
        Ok(purged) => purged,
        Err(kompressor_stats::StatsError::Invalid(reason)) => {
            return Err(ApiError::Invalid(reason));
        }
        Err(error) => {
            if !dry_run {
                record(
                    &state,
                    MaintenanceEvent {
                        ok: false,
                        error: Some(error.to_string()),
                        ..work(action, name, &identity, started)
                    },
                );
            }
            return Err(ApiError::Internal(format!("delete: {error}")));
        }
    };
    let took_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if !dry_run {
        tracing::warn!(
            kind = action,
            rows = purged.rows,
            tenant = purged.scope.tenant.as_deref(),
            run_id = purged.scope.run_id.as_deref(),
            experiment_id = purged.scope.experiment_id.as_deref(),
            before_ms = purged.scope.before_ms,
            took_ms,
            "statistics deleted on request"
        );
        record(
            &state,
            MaintenanceEvent {
                detail: serde_json::to_string(&purged).ok(),
                ..work(action, name, &identity, started)
            },
        );
    }
    Ok(json_response(&serde_json::json!({
        "purged": purged,
        "took_ms": took_ms,
    })))
}

/// The archive in the backup directory called `name`, when it is one the
/// schedule wrote: no separator, no `..`, nothing else that happens to be in
/// the directory.
fn stored_archive(config: &Config, name: &str) -> Result<PathBuf, ApiError> {
    if config.maintenance.backup_dir.as_os_str().is_empty() || backup::archive_time(name).is_none()
    {
        return Err(ApiError::NotFound(format!("no backup called {name:?}")));
    }
    let path = config.maintenance.backup_dir.join(name);
    if !path.is_file() {
        return Err(ApiError::NotFound(format!("no backup called {name:?}")));
    }
    Ok(path)
}

/// `POST /v1/maintenance/backups/{name}/verify`: check a scheduled archive
/// as a restore would — its checksum, its manifest, a full integrity check
/// of every database in it — without stopping or touching anything.
async fn verify(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    UrlPath(name): UrlPath<String>,
) -> Result<Response, ApiError> {
    let identity = permit(&state, &headers, Wanted::Action)?;
    let path = stored_archive(state.core.config(), &name)?;
    let Some(slot) = Slot::take(&state) else {
        return Ok(busy());
    };
    let started = std::time::Instant::now();
    let found = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        backup::verify_archive(&path)
    })
    .await
    .map_err(|error| ApiError::Internal(format!("verify: {error}")))?;
    let verified = match found {
        Ok(verified) => verified,
        Err(error) => {
            record(
                &state,
                MaintenanceEvent {
                    ok: false,
                    error: Some(error.to_string()),
                    ..work("verify", &name, &identity, started)
                },
            );
            return Err(ApiError::Internal(format!("verify of {name}: {error}")));
        }
    };
    let took_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if verified.ok {
        tracing::info!(archive = %name, took_ms, "archive verified");
    } else {
        tracing::error!(archive = %name, problems = ?verified.problems, "archive FAILED verification");
    }
    record(
        &state,
        MaintenanceEvent {
            ok: verified.ok,
            bytes_before: bytes(verified.bytes),
            detail: Some(
                serde_json::json!({
                    "checksum": verified.checksum,
                    "sha256": verified.sha256,
                    "databases": verified.databases.len(),
                    "problems": verified.problems,
                })
                .to_string(),
            ),
            error: verified.problems.first().cloned(),
            ..work("verify", &name, &identity, started)
        },
    );
    Ok(json_response(&serde_json::json!({
        "archive": name,
        "verified": verified,
        "took_ms": took_ms,
    })))
}

/// `GET /v1/maintenance/config`: the effective configuration with every
/// secret replaced, for the page's build panel. It names upstreams, paths
/// and tenants, so it is for the callers that may act for every tenant.
async fn effective_config(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    permit(&state, &headers, Wanted::Space)?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        state.core.config().redacted_toml(),
    )
        .into_response())
}

/// `GET /v1/maintenance/backups/{name}`: one of the scheduled archives.
async fn stored_backup(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    UrlPath(name): UrlPath<String>,
) -> Result<Response, ApiError> {
    permit(&state, &headers, Wanted::Backup)?;
    let missing = || ApiError::NotFound(format!("no backup called {name:?}"));
    let path = stored_archive(state.core.config(), &name)?;
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
        return Ok(busy());
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

    /// After a restart the last check on record stands in for one this
    /// process ran, problems and all.
    #[test]
    fn a_check_on_record_reads_back_as_a_check() {
        let recorded = MaintenanceEvent {
            time_ms: 42,
            action: "check".into(),
            target: STATS_DB.into(),
            origin: "http".into(),
            actor: None,
            took_ms: 7,
            ok: false,
            bytes_before: None,
            bytes_after: None,
            detail: Some(r#"{"problems":["row 3 missing from index"]}"#.into()),
            error: Some("row 3 missing from index".into()),
        };
        let checked = checked_from_record(&recorded);
        assert_eq!((checked.at_ms, checked.took_ms, checked.ok), (42, 7, false));
        assert_eq!(checked.problems, ["row 3 missing from index"]);
        // A record with no detail still says what was wrong.
        let bare = checked_from_record(&MaintenanceEvent {
            detail: None,
            ..recorded
        });
        assert_eq!(bare.problems, ["row 3 missing from index"]);
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
