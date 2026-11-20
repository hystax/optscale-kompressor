//! The Maintenance page and the backup it takes, over HTTP.
//!
//! Against real files: a statistics database the writer is filling and a
//! SQLite ref store holding a fragment, both in a directory of the test's
//! own. What comes back is unpacked and opened, because an archive that
//! downloads is not yet one that restores.

use std::io::Read;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use kompressor_core::AppCore;
use kompressor_core::config::{ApiKey, AuthMode, Config, RefsBackend};
use kompressor_stats::StatsConfig;
use serde_json::{Value, json};
use tower::ServiceExt;

struct Deployment {
    router: Router,
    dir: tempfile::TempDir,
    sink: kompressor_stats::StatsSink,
    _stats: kompressor_stats::StatsHandle,
}

/// A server over on-disk databases, with `tweak` applied to its
/// configuration and `make_router` choosing the listener.
fn deployment(
    tweak: impl FnOnce(&mut Config),
    make_router: fn(Arc<kompressor_server::ServerState>) -> Router,
) -> Deployment {
    let dir = tempfile::tempdir().expect("a directory");
    let mut config = Config::default();
    config.stats.enabled = true;
    config.stats.path = dir.path().join("stats.db");
    config.refs.backend = RefsBackend::Sqlite;
    config.refs.path = dir.path().join("refs.db");
    tweak(&mut config);
    let (sink, stats) =
        kompressor_stats::spawn(&StatsConfig::at(&config.stats.path)).expect("the writer starts");
    let refs = kompressor_core::build_ref_store(&config).expect("the store opens");
    let core = Arc::new(AppCore::new(config, refs, sink.clone()));
    Deployment {
        router: make_router(kompressor_server::ServerState::new(core)),
        dir,
        sink,
        _stats: stats,
    }
}

fn backing_up(config: &mut Config) {
    config.maintenance.backup = true;
}

struct Answer {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
    }
}

async fn send(
    deployment: &Deployment,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> Answer {
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let body = if method == "POST" && path.ends_with("/compress") {
        request = request.header("content-type", "application/json");
        let log: String = (0..400)
            .map(|i| {
                format!(
                    "WARN worker-{i}: connection refused from 10.0.0.{} after {}ms\n",
                    i % 251,
                    i * 7
                )
            })
            .collect();
        Body::from(json!({"messages": [{"role": "tool", "content": log}]}).to_string())
    } else {
        Body::empty()
    };
    let response = deployment
        .router
        .clone()
        .oneshot(request.body(body).expect("a request"))
        .await
        .expect("an answer");
    Answer {
        status: response.status(),
        headers: response.headers().clone(),
        body: response
            .into_body()
            .collect()
            .await
            .expect("a whole body")
            .to_bytes()
            .to_vec(),
    }
}

/// Every entry of the archive, by name.
fn unpack(archive: &[u8]) -> Vec<(String, Vec<u8>)> {
    tar::Archive::new(flate2::read::GzDecoder::new(archive))
        .entries()
        .expect("a tar")
        .map(|entry| {
            let mut entry = entry.expect("an entry");
            let name = entry.path().unwrap().to_string_lossy().into_owned();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            (name, bytes)
        })
        .collect()
}

/// What is in the data directory besides the databases and their logs.
fn strays(deployment: &Deployment) -> Vec<String> {
    std::fs::read_dir(deployment.dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with("stats.db") && !name.starts_with("refs.db"))
        .collect()
}

#[tokio::test]
async fn a_backup_is_a_tar_of_databases_that_open_and_hold_what_was_there() {
    let deployment = deployment(backing_up, kompressor_server::router);
    // Something in each database: a compression leaves a statistics row and
    // a stored fragment.
    let compressed = send(&deployment, "POST", "/v1/compress", &[]).await;
    assert_eq!(compressed.status, StatusCode::OK, "{}", compressed.json());
    let handle = String::from_utf8_lossy(&compressed.body)
        .split("ref:")
        .nth(1)
        .map(|rest| rest[..14].to_string())
        .expect("the compression stored a fragment");

    let status = send(&deployment, "GET", "/v1/maintenance?format=json", &[]).await;
    assert_eq!(status.status, StatusCode::OK);
    let status = status.json();
    assert_eq!(status["backup"]["allowed"], true, "{status}");
    assert_eq!(status["databases"][0]["name"], "stats.db");
    assert_eq!(status["databases"][1]["included"], true);
    assert!(status["databases"][1]["bytes"].as_u64().unwrap() > 0);

    let answer = send(&deployment, "POST", "/v1/maintenance/backup", &[]).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    assert_eq!(answer.header("content-type"), "application/gzip");
    assert_eq!(answer.header("cache-control"), "no-store");
    assert_eq!(
        answer.header("content-length"),
        answer.body.len().to_string()
    );
    let disposition = answer.header("content-disposition");
    assert!(
        disposition.starts_with("attachment; filename=\"kompressor-backup-")
            && disposition.ends_with(".tar.gz\""),
        "{disposition}"
    );

    let entries = unpack(&answer.body);
    let names: Vec<&str> = entries.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["manifest.json", "stats.db", "refs.db"]);
    let manifest: Value = serde_json::from_slice(&entries[0].1).unwrap();
    assert_eq!(manifest["format"], 1);
    assert_eq!(manifest["kompressor"], env!("CARGO_PKG_VERSION"));
    assert_eq!(manifest["skipped"], json!([]));

    // Restored somewhere else, both open and hold what was written: the
    // statistics row that was still queued when the backup was asked for,
    // and the fragment behind the handle.
    let restored = tempfile::tempdir().unwrap();
    for (name, bytes) in &entries[1..] {
        assert_eq!(
            manifest["databases"]
                .as_array()
                .unwrap()
                .iter()
                .find(|database| database["name"] == *name)
                .map(|database| database["bytes"].as_u64().unwrap()),
            Some(bytes.len() as u64)
        );
        std::fs::write(restored.path().join(name), bytes).unwrap();
    }
    let stats = rusqlite::Connection::open(restored.path().join("stats.db")).unwrap();
    let events: i64 = stats
        .query_row("SELECT COUNT(*) FROM optimization_events", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(events, 1, "flushed into the snapshot");
    let refs = rusqlite::Connection::open(restored.path().join("refs.db")).unwrap();
    let held: i64 = refs
        .query_row(
            "SELECT COUNT(*) FROM refs WHERE ref = ?1",
            // The store keys a fragment by its handle without the `k_`.
            [handle.trim_start_matches("k_")],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(held, 1, "{handle} is in the restored store");

    // Sent, the archive is gone from beside the databases, and the service
    // carries on: a second backup works.
    for _ in 0..50 {
        if strays(&deployment).is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(strays(&deployment), Vec::<String>::new());
    let again = send(&deployment, "POST", "/v1/maintenance/backup", &[]).await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(unpack(&again.body).len(), 3);
    let _ = deployment.sink.flush();
}

#[tokio::test]
async fn backup_is_off_until_it_is_turned_on() {
    let deployment = deployment(|_| {}, kompressor_server::router);
    let answer = send(&deployment, "POST", "/v1/maintenance/backup", &[]).await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    let message = answer.json()["error"]["message"].to_string();
    assert!(message.contains("maintenance.backup = true"), "{message}");
    assert!(
        message.contains("kompressor backup"),
        "the way that needs no setting: {message}"
    );

    let status = send(&deployment, "GET", "/v1/maintenance?format=json", &[])
        .await
        .json();
    assert_eq!(status["backup"]["enabled"], false);
    assert_eq!(status["backup"]["allowed"], false);
    assert!(
        status["backup"]["reason"]
            .as_str()
            .unwrap()
            .contains("maintenance.backup")
    );
    assert_eq!(
        status["databases"].as_array().unwrap().len(),
        2,
        "still listed"
    );
    assert_eq!(
        strays(&deployment),
        Vec::<String>::new(),
        "nothing was written"
    );
    // A backup is taken, not fetched.
    let get = send(&deployment, "GET", "/v1/maintenance/backup", &[]).await;
    assert_eq!(get.status, StatusCode::METHOD_NOT_ALLOWED);
}

/// The archive is every tenant's data. A tenant's key reads that tenant's
/// reports; it does not take everyone's database home.
#[tokio::test]
async fn with_tenants_only_the_shared_secret_takes_a_backup() {
    let deployment = deployment(
        |config| {
            backing_up(config);
            config.auth.mode = AuthMode::ApiKeys;
            config.auth.secret = "the-shared-secret".to_string();
            config.auth.keys = vec![ApiKey {
                key: "sk-alpha".to_string(),
                tenant: "alpha".to_string(),
                ..Default::default()
            }];
        },
        kompressor_server::router,
    );
    let path = "/v1/maintenance/backup";
    assert_eq!(
        send(&deployment, "POST", path, &[]).await.status,
        StatusCode::UNAUTHORIZED
    );
    let key = [("x-kompressor-key", "sk-alpha")];
    assert_eq!(
        send(&deployment, "POST", path, &key).await.status,
        StatusCode::FORBIDDEN
    );
    let status = send(&deployment, "GET", "/v1/maintenance?format=json", &key)
        .await
        .json();
    assert_eq!(status["backup"]["enabled"], true);
    assert_eq!(status["backup"]["allowed"], false);
    assert!(
        status["backup"]["reason"]
            .as_str()
            .unwrap()
            .contains("x-kompressor-secret")
    );
    assert_eq!(strays(&deployment), Vec::<String>::new());

    let secret = [("x-kompressor-secret", "the-shared-secret")];
    let status = send(&deployment, "GET", "/v1/maintenance?format=json", &secret)
        .await
        .json();
    assert_eq!(status["backup"]["allowed"], true);
    let answer = send(&deployment, "POST", path, &secret).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(unpack(&answer.body)[0].0, "manifest.json");
    let wrong = [("x-kompressor-secret", "not-it")];
    assert_ne!(
        send(&deployment, "POST", path, &wrong).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_deployment_with_no_database_on_disk_says_so() {
    let deployment = deployment(
        |config| {
            backing_up(config);
            config.refs.backend = RefsBackend::Memory;
            config.stats.enabled = false;
        },
        kompressor_server::router,
    );
    let answer = send(&deployment, "POST", "/v1/maintenance/backup", &[]).await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    let message = answer.json()["error"]["message"].to_string();
    assert!(message.contains("no database to back up"), "{message}");
    let status = send(&deployment, "GET", "/v1/maintenance?format=json", &[])
        .await
        .json();
    assert_eq!(status["databases"][0]["included"], false);
    assert_eq!(
        status["databases"][1]["reason"],
        "the ref store is in memory"
    );
}

/// The page is at `/maintenance` on the statistics listener and at the data
/// route under a version prefix, and every other page links to it.
#[tokio::test]
async fn the_page_is_served_on_both_listeners_and_linked_from_the_others() {
    let html = [("accept", "text/html")];
    let listener = deployment(backing_up, kompressor_server::stats_listener_router);
    let page = send(&listener, "GET", "/maintenance", &[]).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.header("content-type").starts_with("text/html"));
    let text = String::from_utf8(page.body).unwrap();
    assert!(text.contains("Kompressor maintenance") && text.contains("Download backup"));
    assert!(
        text.contains("\"listener\":true"),
        "it knows where its data is"
    );
    for other in ["/", "/cache", "/analysis", "/refs", "/health"] {
        let page = send(&listener, "GET", other, &html).await;
        let text = String::from_utf8(page.body).unwrap();
        assert!(
            text.contains("id=\"maintenance-link\"") && text.contains("maintenance-link\").href"),
            "{other} links to Maintenance"
        );
    }
    // The data and the action are under /v1 and /v2 there too.
    for version in ["v1", "v2"] {
        let status = send(&listener, "GET", &format!("/{version}/maintenance"), &[]).await;
        assert_eq!(status.json()["backup"]["allowed"], true);
    }
    let answer = send(&listener, "POST", "/v1/maintenance/backup", &[]).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(unpack(&answer.body).len(), 3);

    let api = deployment(|_| {}, kompressor_server::router);
    let page = send(&api, "GET", "/v1/maintenance", &html).await;
    let text = String::from_utf8(page.body).unwrap();
    assert!(text.contains("\"listener\":false"));
    let bad = send(&api, "GET", "/v1/maintenance?format=xml", &[]).await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
}

fn acting(config: &mut Config) {
    config.maintenance.actions = true;
}

/// The status describes each database as SQLite sees it, to anyone who can
/// read the dashboards, whether or not any action is turned on.
#[tokio::test]
async fn the_status_describes_each_database_with_nothing_turned_on() {
    let deployment = deployment(|_| {}, kompressor_server::router);
    send(&deployment, "POST", "/v1/compress", &[]).await;
    let _ = deployment.sink.flush();
    let status = send(&deployment, "GET", "/v1/maintenance?format=json", &[])
        .await
        .json();
    assert_eq!(status["actions"]["enabled"], false);
    assert_eq!(status["actions"]["allowed"], false);
    assert!(
        status["actions"]["reason"]
            .as_str()
            .unwrap()
            .contains("maintenance.actions = true")
    );
    assert_eq!(status["schedule"], Value::Null);
    assert!(status["now_ms"].as_i64().unwrap() > 0);

    let stats = &status["databases"][0];
    assert_eq!(stats["name"], "stats.db");
    assert_eq!(stats["schema_version"], stats["expected_schema_version"]);
    assert_eq!(
        stats["expected_schema_version"].as_u64(),
        Some(u64::from(kompressor_stats::schema::latest_version()))
    );
    assert!(stats["bytes"].as_u64().unwrap() >= stats["wal_bytes"].as_u64().unwrap());
    assert!(stats["reclaimable_bytes"].is_u64());
    assert_eq!(
        stats["checked"],
        Value::Null,
        "nothing has been checked yet"
    );
    assert_eq!(stats["error"], Value::Null);
    let refs = &status["databases"][1];
    assert_eq!(refs["schema_version"], 1);
    assert_eq!(
        refs["oldest_ms"],
        Value::Null,
        "a span is the statistics' to have"
    );

    // The actions themselves are refused, and say how to turn them on.
    for action in ["check", "checkpoint"] {
        let answer = send(
            &deployment,
            "POST",
            &format!("/v1/maintenance/databases/stats.db/{action}"),
            &[],
        )
        .await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{action}");
        let message = answer.json()["error"]["message"].to_string();
        assert!(
            message.contains("kompressor db"),
            "the way that needs no setting: {message}"
        );
    }
}

#[tokio::test]
async fn a_database_is_checked_and_checkpointed_while_the_service_runs() {
    let deployment = deployment(acting, kompressor_server::router);
    send(&deployment, "POST", "/v1/compress", &[]).await;
    let _ = deployment.sink.flush();

    let wal = deployment.dir.path().join("refs.db-wal");
    assert!(
        std::fs::metadata(&wal).unwrap().len() > 0,
        "the fragment is in the log"
    );
    let answer = send(
        &deployment,
        "POST",
        "/v1/maintenance/databases/refs.db/checkpoint",
        &[],
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    let done = answer.json();
    assert_eq!(done["database"], "refs.db");
    assert_eq!(done["complete"], true, "{done}");
    assert!(done["wal_bytes_before"].as_u64().unwrap() > 0);
    assert_eq!(done["wal_bytes_after"], 0);
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);

    for name in ["stats.db", "refs.db"] {
        let answer = send(
            &deployment,
            "POST",
            &format!("/v1/maintenance/databases/{name}/check"),
            &[],
        )
        .await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
        let checked = &answer.json()["checked"];
        assert_eq!(checked["ok"], true, "{checked}");
        assert_eq!(checked["problems"], json!([]));
    }
    // The result is on the status until the service restarts.
    let status = send(&deployment, "GET", "/v1/maintenance?format=json", &[])
        .await
        .json();
    assert_eq!(status["actions"]["allowed"], true);
    for database in status["databases"].as_array().unwrap() {
        assert_eq!(database["checked"]["ok"], true, "{database}");
        assert!(database["checked"]["at_ms"].as_i64().unwrap() > 0);
    }
    // And the service carries on writing.
    assert_eq!(
        send(&deployment, "POST", "/v1/compress", &[]).await.status,
        StatusCode::OK
    );

    let unknown = send(
        &deployment,
        "POST",
        "/v1/maintenance/databases/config.toml/check",
        &[],
    )
    .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert!(
        unknown.json()["error"]["message"]
            .to_string()
            .contains("stats.db and refs.db")
    );
    // Turning actions on does not turn backup on.
    assert_eq!(
        send(&deployment, "POST", "/v1/maintenance/backup", &[])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

/// A damaged file is a failed check with SQLite's account of it, reported
/// as an answer rather than as a server error, and kept on the status.
#[tokio::test]
async fn a_damaged_database_is_reported_as_a_failed_check() {
    let deployment = deployment(acting, kompressor_server::router);
    for _ in 0..3 {
        send(&deployment, "POST", "/v1/compress", &[]).await;
    }
    send(
        &deployment,
        "POST",
        "/v1/maintenance/databases/refs.db/checkpoint",
        &[],
    )
    .await;
    let path = deployment.dir.path().join("refs.db");
    let mut bytes = std::fs::read(&path).unwrap();
    assert!(bytes.len() > 4_096 * 3, "{} bytes", bytes.len());
    for byte in &mut bytes[4_096..4_096 * 2] {
        *byte = 0xAB;
    }
    std::fs::write(&path, bytes).unwrap();

    let answer = send(
        &deployment,
        "POST",
        "/v1/maintenance/databases/refs.db/check",
        &[],
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK);
    let checked = &answer.json()["checked"];
    assert_eq!(checked["ok"], false, "{checked}");
    assert!(!checked["problems"].as_array().unwrap().is_empty());
    let status = send(&deployment, "GET", "/v1/maintenance?format=json", &[])
        .await
        .json();
    assert_eq!(status["databases"][1]["checked"]["ok"], false);
    assert_eq!(
        status["databases"][0]["checked"],
        Value::Null,
        "the other was not checked"
    );
}

#[tokio::test]
async fn with_tenants_only_the_shared_secret_runs_an_action() {
    let deployment = deployment(
        |config| {
            acting(config);
            config.auth.mode = AuthMode::ApiKeys;
            config.auth.secret = "the-shared-secret".to_string();
            config.auth.keys = vec![ApiKey {
                key: "sk-alpha".to_string(),
                tenant: "alpha".to_string(),
                ..Default::default()
            }];
        },
        kompressor_server::router,
    );
    let path = "/v1/maintenance/databases/refs.db/checkpoint";
    let key = [("x-kompressor-key", "sk-alpha")];
    assert_eq!(
        send(&deployment, "POST", path, &[]).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&deployment, "POST", path, &key).await.status,
        StatusCode::FORBIDDEN
    );
    // A tenant still sees what the databases are.
    let status = send(&deployment, "GET", "/v1/maintenance?format=json", &key)
        .await
        .json();
    assert_eq!(status["actions"]["enabled"], true);
    assert_eq!(status["actions"]["allowed"], false);
    assert!(status["databases"][1]["schema_version"].is_u64());
    let secret = [("x-kompressor-secret", "the-shared-secret")];
    assert_eq!(
        send(&deployment, "POST", path, &secret).await.status,
        StatusCode::OK
    );
}

/// The archives the schedule wrote are listed newest first and can be
/// downloaded by name — by that name only.
#[tokio::test]
async fn scheduled_archives_are_listed_and_downloaded_by_name() {
    use kompressor_core::backup::archive_name;
    let backups = tempfile::tempdir().unwrap();
    let dir = backups.path().to_path_buf();
    let hour = 3_600_000;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let (older, newer) = (archive_name(now - 30 * hour), archive_name(now - 2 * hour));
    std::fs::write(dir.join(&older), b"older archive").unwrap();
    std::fs::write(dir.join(&newer), b"the newer archive").unwrap();
    std::fs::write(dir.join("notes.txt"), b"not an archive").unwrap();
    std::fs::write(dir.join("kompressor-backup-latest.tar.gz"), b"nor this").unwrap();

    let schedule_dir = dir.clone();
    let deployment = deployment(
        move |config| {
            backing_up(config);
            config.maintenance.backup_dir = schedule_dir;
            config.maintenance.backup_every = std::time::Duration::from_secs(24 * 3_600);
            config.maintenance.backup_keep = 5;
        },
        kompressor_server::router,
    );
    let status = send(&deployment, "GET", "/v1/maintenance?format=json", &[])
        .await
        .json();
    let schedule = &status["schedule"];
    assert_eq!(schedule["dir"], dir.display().to_string());
    assert_eq!(
        (&schedule["every_s"], &schedule["keep"]),
        (&json!(86_400), &json!(5))
    );
    let names: Vec<&str> = schedule["archives"]
        .as_array()
        .unwrap()
        .iter()
        .map(|archive| archive["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, [newer.as_str(), older.as_str()]);
    assert_eq!(schedule["archives"][0]["bytes"], 17);
    let due = schedule["next_due_ms"].as_i64().unwrap();
    assert!(
        (due - (now + 22 * hour)).abs() < 5_000,
        "a day after the newest: {due}"
    );

    let answer = send(
        &deployment,
        "GET",
        &format!("/v1/maintenance/backups/{newer}"),
        &[],
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.body, b"the newer archive");
    assert_eq!(answer.header("content-type"), "application/gzip");
    assert_eq!(
        answer.header("content-disposition"),
        format!("attachment; filename=\"{newer}\"")
    );
    assert!(
        dir.join(&newer).exists(),
        "a stored archive is sent, not consumed"
    );

    for not_served in [
        "notes.txt",
        "kompressor-backup-latest.tar.gz",
        "..%2Fstats.db",
        "%2Fetc%2Fpasswd",
        &archive_name(now),
    ] {
        let answer = send(
            &deployment,
            "GET",
            &format!("/v1/maintenance/backups/{not_served}"),
            &[],
        )
        .await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{not_served}");
    }
}

/// Listing is for anyone who can read the dashboards; an archive is every
/// tenant's data and needs what taking one needs.
#[tokio::test]
async fn a_stored_archive_needs_what_taking_one_needs() {
    use kompressor_core::backup::archive_name;
    let backups = tempfile::tempdir().unwrap();
    let name = archive_name(1_795_158_930_000);
    std::fs::write(backups.path().join(&name), b"archive").unwrap();
    let dir = backups.path().to_path_buf();
    // A directory with backup over HTTP off: listed, not served.
    let off = deployment(
        {
            let dir = dir.clone();
            move |config| config.maintenance.backup_dir = dir
        },
        kompressor_server::router,
    );
    let status = send(&off, "GET", "/v1/maintenance?format=json", &[])
        .await
        .json();
    assert_eq!(status["schedule"]["archives"][0]["name"], name);
    let path = format!("/v1/maintenance/backups/{name}");
    assert_eq!(
        send(&off, "GET", &path, &[]).await.status,
        StatusCode::NOT_FOUND
    );

    let keyed = deployment(
        move |config| {
            backing_up(config);
            config.maintenance.backup_dir = dir;
            config.auth.mode = AuthMode::ApiKeys;
            config.auth.secret = "the-shared-secret".to_string();
            config.auth.keys = vec![ApiKey {
                key: "sk-alpha".to_string(),
                tenant: "alpha".to_string(),
                ..Default::default()
            }];
        },
        kompressor_server::router,
    );
    let key = [("x-kompressor-key", "sk-alpha")];
    assert_eq!(
        send(&keyed, "GET", &path, &key).await.status,
        StatusCode::FORBIDDEN
    );
    let secret = [("x-kompressor-secret", "the-shared-secret")];
    assert_eq!(
        send(&keyed, "GET", &path, &secret).await.status,
        StatusCode::OK
    );
}

/// Every backup is on record, with what asked for it, and the status says
/// when the last one was.
#[tokio::test]
async fn a_backup_is_recorded_and_the_status_says_when_the_last_one_was() {
    let deployment = deployment(backing_up, kompressor_server::router);
    let status = send(&deployment, "GET", "/v1/maintenance?format=json", &[])
        .await
        .json();
    let last = &status["last_backup"];
    assert_eq!(last["ok_ms"], Value::Null);
    assert_eq!(last["history"], json!([]));
    assert_eq!(last["max_age_s"], Value::Null, "no backups asked for");
    assert_eq!(last["stale"], false, "and so none is missed");

    let answer = send(&deployment, "POST", "/v1/maintenance/backup", &[]).await;
    assert_eq!(answer.status, StatusCode::OK);
    let _ = deployment.sink.flush();
    let status = send(&deployment, "GET", "/v1/maintenance?format=json", &[])
        .await
        .json();
    let last = &status["last_backup"];
    let taken = last["ok_ms"].as_i64().expect("on record");
    assert!((status["now_ms"].as_i64().unwrap() - taken).abs() < 60_000);
    assert_eq!(last["failed_ms"], Value::Null);
    let entry = &last["history"][0];
    assert_eq!(
        (
            &entry["origin"],
            &entry["destination"],
            &entry["ok"],
            &entry["databases"]
        ),
        (&json!("http"), &json!("download"), &json!(true), &json!(2))
    );
    assert_eq!(entry["bytes"].as_u64(), Some(answer.body.len() as u64));

    // The Health page has the same answer, not judged: nothing was asked for.
    let health = send(&deployment, "GET", "/v1/stats/health?format=json", &[])
        .await
        .json();
    let check = health["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "last_backup")
        .unwrap_or_else(|| panic!("no last_backup check in {health}"));
    assert_eq!(check["level"], "info");
    assert_eq!(check["unit"], "age");
    assert!(
        check["detail"]
            .as_str()
            .unwrap()
            .starts_with("taken 0 min ago"),
        "{check}"
    );
}

/// With backups expected, an old one — or none — is stale on the page and a
/// warning on Health; a fresh archive in the directory clears it.
#[tokio::test]
async fn a_backup_older_than_expected_is_stale_and_a_health_warning() {
    use kompressor_core::backup::archive_name;
    let backups = tempfile::tempdir().unwrap();
    let dir = backups.path().to_path_buf();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let hour = 3_600_000;
    let scheduled = {
        let dir = dir.clone();
        move |config: &mut Config| {
            config.maintenance.backup_dir = dir;
            config.maintenance.backup_every = std::time::Duration::from_secs(24 * 3_600);
        }
    };
    let deployment = deployment(scheduled, kompressor_server::router);
    let health_level = |health: Value| {
        health["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["id"] == "last_backup")
            .map(|check| (check["level"].clone(), check["detail"].clone()))
            .unwrap()
    };

    // None yet, and a daily one is expected.
    let status = send(&deployment, "GET", "/v1/maintenance?format=json", &[])
        .await
        .json();
    assert_eq!(
        status["last_backup"]["max_age_s"],
        2 * 86_400,
        "one missed is a warning"
    );
    assert_eq!(status["last_backup"]["stale"], true);
    let (level, detail) = health_level(
        send(&deployment, "GET", "/v1/stats/health?format=json", &[])
            .await
            .json(),
    );
    assert_eq!(level, "warn");
    assert_eq!(detail, "none on record; one is expected");

    // Three days old.
    std::fs::write(dir.join(archive_name(now - 72 * hour)), b"archive").unwrap();
    let status = send(&deployment, "GET", "/v1/maintenance?format=json", &[])
        .await
        .json();
    assert_eq!(status["last_backup"]["stale"], true);
    assert_eq!(
        status["last_backup"]["ok_ms"].as_i64(),
        Some((now - 72 * hour) / 1_000 * 1_000)
    );
    let (level, detail) = health_level(
        send(&deployment, "GET", "/v1/stats/health?format=json", &[])
            .await
            .json(),
    );
    assert_eq!(level, "warn");
    assert_eq!(detail, "taken 3 d ago");

    // An hour old.
    std::fs::write(dir.join(archive_name(now - hour)), b"archive").unwrap();
    let status = send(&deployment, "GET", "/v1/maintenance?format=json", &[])
        .await
        .json();
    assert_eq!(status["last_backup"]["stale"], false);
    let (level, _) = health_level(
        send(&deployment, "GET", "/v1/stats/health?format=json", &[])
            .await
            .json(),
    );
    assert_eq!(level, "ok");
}

/// The status carries what the restore instructions need, and the page
/// spells them out for the shell rather than offering a button.
#[tokio::test]
async fn the_page_gives_restore_instructions_and_no_restore_route() {
    let deployment = deployment(backing_up, kompressor_server::stats_listener_router);
    let status = send(&deployment, "GET", "/v1/maintenance", &[])
        .await
        .json();
    assert_eq!(
        status["restore"]["config"],
        Value::Null,
        "built in a test, read from no file"
    );
    assert!(status["restore"]["systemd"].is_boolean());
    let page = String::from_utf8(send(&deployment, "GET", "/maintenance", &[]).await.body).unwrap();
    assert!(page.contains("kompressor restore ARCHIVE.tar.gz"));
    assert!(page.contains("id=\"history\"") && page.contains("id=\"last-backup\""));
    for path in ["/v1/maintenance/restore", "/v1/maintenance/backups/restore"] {
        let answer = send(&deployment, "POST", path, &[]).await;
        assert!(
            answer.status == StatusCode::NOT_FOUND
                || answer.status == StatusCode::METHOD_NOT_ALLOWED,
            "{path}: {}",
            answer.status
        );
    }
}
