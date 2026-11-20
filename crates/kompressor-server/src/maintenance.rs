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
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use kompressor_core::backup::{self, Manifest};
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

/// One database, as the status lists it.
#[derive(Debug, Serialize)]
struct DatabaseStatus {
    name: &'static str,
    /// Where it is kept; absent for one that has no file.
    path: Option<String>,
    /// Its size on disk, write-ahead log included; absent when there is none.
    bytes: Option<u64>,
    /// Whether a backup taken now would hold it.
    included: bool,
    /// Why not, when it would not.
    reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct BackupStatus {
    /// `maintenance.backup`.
    enabled: bool,
    /// Whether this caller may take one.
    allowed: bool,
    /// Why not, when it may not.
    reason: Option<String>,
}

/// The JSON body of `GET /v1/maintenance`.
#[derive(Debug, Serialize)]
struct MaintenanceStatus {
    version: &'static str,
    backup: BackupStatus,
    databases: Vec<DatabaseStatus>,
}

/// The maintenance routes, mounted under each version prefix by the API
/// router and by the statistics listener.
pub(crate) fn maintenance_routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/maintenance", get(status))
        .route("/maintenance/backup", post(take_backup))
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

/// Why this caller may not take a backup, or `None` when it may.
///
/// `trusted` is whether it presented the shared secret.
fn refusal(config: &Config, trusted: bool) -> Option<String> {
    if !config.maintenance.backup {
        return Some(
            "backup over HTTP is off on this deployment; set maintenance.backup = true \
             (or KOMPRESSOR__MAINTENANCE__BACKUP=true) to turn it on, or run \
             `kompressor backup` on the host"
                .to_string(),
        );
    }
    if config.auth.mode != AuthMode::None && !trusted {
        return Some(
            "a backup holds every tenant's data, so a tenant's key does not unlock it: \
             present the shared secret (x-kompressor-secret)"
                .to_string(),
        );
    }
    None
}

/// A file's size with its write-ahead log, which holds the newest rows.
fn size_on_disk(path: &Path) -> Option<u64> {
    let main = std::fs::metadata(path).ok()?.len();
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    Some(main + std::fs::metadata(PathBuf::from(wal)).map_or(0, |meta| meta.len()))
}

fn database(name: &'static str, path: Option<&Path>, absent: Option<&str>) -> DatabaseStatus {
    let bytes = path.and_then(size_on_disk);
    let reason = match (absent, path, bytes) {
        (Some(reason), _, _) => Some(reason.to_string()),
        (None, Some(path), None) => Some(format!("{} does not exist yet", path.display())),
        _ => None,
    };
    DatabaseStatus {
        name,
        path: path.map(|path| path.display().to_string()),
        bytes,
        included: reason.is_none(),
        reason,
    }
}

fn databases(config: &Config) -> Vec<DatabaseStatus> {
    vec![
        if config.stats.enabled {
            database("stats.db", Some(&config.stats.path), None)
        } else {
            database("stats.db", None, Some("statistics are off"))
        },
        match config.refs.backend {
            RefsBackend::Sqlite => database("refs.db", Some(&config.refs.path), None),
            RefsBackend::Memory => database("refs.db", None, Some("the ref store is in memory")),
        },
    ]
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
    let config = state.core.config();
    let reason = refusal(config, identity.trusted);
    Ok(json_response(&MaintenanceStatus {
        version: env!("CARGO_PKG_VERSION"),
        backup: BackupStatus {
            enabled: config.maintenance.backup,
            allowed: reason.is_none(),
            reason,
        },
        databases: databases(config),
    }))
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
    let identity = authenticate(&state, &headers)?;
    if let Some(reason) = refusal(state.core.config(), identity.trusted) {
        return Err(if state.core.config().maintenance.backup {
            tracing::warn!(tenant = %identity.tenant.as_str(), "a backup was refused");
            ApiError::Forbidden
        } else {
            ApiError::NotFound(reason)
        });
    }
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
    let (manifest, spooled, file, bytes) = match written {
        Ok(written) => written,
        Err(kompressor_core::CoreError::BadRequest(reason)) => {
            return Err(ApiError::NotFound(reason));
        }
        Err(error) => return Err(ApiError::Internal(format!("backup: {error}"))),
    };
    tracing::info!(
        tenant = %identity.tenant.as_str(),
        bytes,
        databases = manifest.databases.len(),
        "backup written; sending it"
    );
    Ok(attachment(&manifest, spooled, file, bytes))
}

/// The archive as a streamed attachment.
fn attachment(manifest: &Manifest, spooled: Spooled, file: std::fs::File, bytes: u64) -> Response {
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

    let name = backup::archive_name(if manifest.created_at_ms > 0 {
        manifest.created_at_ms
    } else {
        now_ms()
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
        assert!(
            refusal(&config, true)
                .unwrap()
                .contains("maintenance.backup")
        );
        config.maintenance.backup = true;
        assert_eq!(refusal(&config, false), None, "one tenant: anyone is it");
        config.auth.mode = AuthMode::ApiKeys;
        assert!(refusal(&config, false).unwrap().contains("shared secret"));
        assert_eq!(refusal(&config, true), None);
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
        assert_eq!(
            (listed[1].path.as_deref(), listed[1].included),
            (None, false)
        );
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
