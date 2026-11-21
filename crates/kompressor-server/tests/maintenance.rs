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
    dispatch(deployment, request.body(body).expect("a request")).await
}

/// A POST carrying `body` as JSON.
async fn post_json(deployment: &Deployment, path: &str, body: Value) -> Answer {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("a request");
    dispatch(deployment, request).await
}

async fn dispatch(deployment: &Deployment, request: Request<Body>) -> Answer {
    let response = deployment
        .router
        .clone()
        .oneshot(request)
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

fn rows(deployment: &Deployment, table: &str) -> i64 {
    rusqlite::Connection::open(deployment.dir.path().join("stats.db"))
        .unwrap()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

/// A vacuum hands free pages back and an optimize gives the planner its
/// figures, on both databases, with the service writing before and after.
#[tokio::test]
async fn a_database_is_vacuumed_and_optimized_while_the_service_runs() {
    let deployment = deployment(acting, kompressor_server::router);
    for _ in 0..3 {
        send(&deployment, "POST", "/v1/compress", &[]).await;
    }
    let _ = deployment.sink.flush();
    // Free pages to reclaim: rows deleted behind the service's back.
    {
        let conn = rusqlite::Connection::open(deployment.dir.path().join("refs.db")).unwrap();
        conn.execute("DELETE FROM refs", []).unwrap();
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .unwrap();
    }
    let before = send(&deployment, "GET", "/v1/maintenance?format=json", &[])
        .await
        .json();
    assert!(
        before["databases"][1]["reclaimable_bytes"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(before["databases"][0]["analyzed"], false);

    for name in ["stats.db", "refs.db"] {
        let answer = send(
            &deployment,
            "POST",
            &format!("/v1/maintenance/databases/{name}/vacuum"),
            &[],
        )
        .await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
        let done = answer.json();
        assert_eq!(done["database"], name);
        assert_eq!(done["folded"], true, "{done}");
        assert!(done["bytes_after"].as_u64().unwrap() <= done["bytes_before"].as_u64().unwrap());

        let answer = send(
            &deployment,
            "POST",
            &format!("/v1/maintenance/databases/{name}/optimize"),
            &[],
        )
        .await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
        // An emptied table has nothing to describe; the statistics do.
        let described = answer.json()["described"].as_u64().unwrap();
        assert!(described > 0 || name == "refs.db", "{name}: {described}");
    }
    let after = send(&deployment, "GET", "/v1/maintenance?format=json", &[])
        .await
        .json();
    for database in after["databases"].as_array().unwrap() {
        assert_eq!(database["reclaimable_bytes"], 0, "{database}");
        assert_eq!(database["analyzed"], true, "{database}");
    }
    assert!(
        after["databases"][1]["bytes"].as_u64().unwrap()
            < before["databases"][1]["bytes"].as_u64().unwrap()
    );
    // The slot a vacuum held is free again, and the service carries on.
    let recorded = rows(&deployment, "optimization_events");
    assert_eq!(
        send(&deployment, "POST", "/v1/compress", &[]).await.status,
        StatusCode::OK
    );
    let _ = deployment.sink.flush();
    assert!(rows(&deployment, "optimization_events") > recorded);
    assert_eq!(
        send(
            &deployment,
            "POST",
            "/v1/maintenance/databases/stats.db/vacuum",
            &[]
        )
        .await
        .status,
        StatusCode::OK
    );
}

/// The breakdown accounts for the file table by table, and the status says
/// what keeps it bounded and how much room there is.
#[tokio::test]
async fn the_status_says_where_the_space_is_and_what_keeps_it_bounded() {
    let deployment = deployment(
        |config| config.maintenance.backup_dir = config.stats.path.with_file_name("backups"),
        kompressor_server::router,
    );
    std::fs::create_dir_all(deployment.dir.path().join("backups")).unwrap();
    send(&deployment, "POST", "/v1/compress", &[]).await;
    let _ = deployment.sink.flush();

    // With nothing turned on, as the rest of the description is.
    let answer = send(
        &deployment,
        "GET",
        "/v1/maintenance/databases/stats.db/space",
        &[],
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    let space = answer.json();
    let objects = space["objects"].as_array().unwrap();
    let table = objects
        .iter()
        .find(|object| object["name"] == "optimization_events")
        .expect("the table is listed");
    assert_eq!(table["kind"], "table");
    assert!(table["rows"].as_u64().unwrap() >= 1);
    assert!(
        objects
            .iter()
            .any(|object| object["kind"] == "index" && object["table"] == "optimization_events")
    );
    assert_eq!(
        space["bytes"].as_u64().unwrap(),
        objects
            .iter()
            .map(|object| object["bytes"].as_u64().unwrap())
            .sum::<u64>()
    );
    let sizes: Vec<u64> = objects
        .iter()
        .map(|object| object["bytes"].as_u64().unwrap())
        .collect();
    assert!(
        sizes.windows(2).all(|pair| pair[0] >= pair[1]),
        "largest first"
    );

    let status = send(&deployment, "GET", "/v1/maintenance?format=json", &[])
        .await
        .json();
    assert_eq!(status["space"]["allowed"], true);
    let retention = &status["retention"];
    assert_eq!(retention["stats"]["retention_days"], 90);
    assert!(retention["stats"]["cutoff_ms"].as_i64().unwrap() < status["now_ms"].as_i64().unwrap());
    assert_eq!(retention["stats"]["every_s"], 3_600);
    assert_eq!(retention["stats"]["backlog_days"], 0, "nothing is old yet");
    // The writer's first pass runs as it starts.
    assert_eq!(retention["stats"]["last"]["error"], Value::Null);
    assert_eq!(retention["stats"]["last"]["removed"]["proxy_requests"], 0);
    assert!(retention["refs"]["every_s"].as_u64().unwrap() > 0);
    assert_eq!(retention["refs"]["live_refs"], 1);
    assert_eq!(
        retention["refs"]["last"],
        Value::Null,
        "the sweep is `serve`'s to run, and this is not `serve`"
    );

    let storage = &status["storage"];
    assert_eq!(storage["volumes"][0]["role"], "data");
    assert!(storage["volumes"][0]["free_bytes"].as_u64().unwrap() > 0);
    #[cfg(unix)]
    {
        assert_eq!(storage["backups_share_disk"], true);
        assert_eq!(storage["volumes"].as_array().unwrap().len(), 1);
    }
    assert_eq!(
        storage["growth_bytes_per_day"],
        Value::Null,
        "not from a minute of requests"
    );
}

fn erasing(config: &mut Config) {
    config.maintenance.erase = true;
}

/// Erasing is its own setting, has to be asked for word for word, and
/// leaves an empty database the service goes on writing to.
#[tokio::test]
async fn the_statistics_are_erased_only_when_asked_for_in_so_many_words() {
    let path = "/v1/maintenance/statistics/erase";
    let asked = json!({"confirm": "nuke all stats"});

    // Off by default, and the housekeeping setting does not turn it on.
    let guarded = deployment(acting, kompressor_server::router);
    let refused = post_json(&guarded, path, asked.clone()).await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND);
    assert!(
        refused.json()["error"]["message"]
            .to_string()
            .contains("maintenance.erase = true")
    );
    let status = send(&guarded, "GET", "/v1/maintenance?format=json", &[])
        .await
        .json();
    assert_eq!(status["erase"]["enabled"], false);
    assert_eq!(status["erase"]["allowed"], false);

    let deployment = deployment(erasing, kompressor_server::router);
    for _ in 0..3 {
        send(&deployment, "POST", "/v1/compress", &[]).await;
    }
    let _ = deployment.sink.flush();
    let recorded = rows(&deployment, "optimization_events");
    assert!(recorded >= 3);

    // Not without the words: no body, another phrase, a near miss.
    for body in [
        Value::Null,
        json!({}),
        json!({"confirm": true}),
        json!({"confirm": "yes"}),
        json!({"confirm": "Nuke all stats"}),
    ] {
        let answer = post_json(&deployment, path, body.clone()).await;
        assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(
            answer.json()["error"]["message"]
                .to_string()
                .contains("nuke all stats"),
            "the refusal says what to send"
        );
    }
    assert_eq!(
        send(&deployment, "POST", path, &[]).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(rows(&deployment, "optimization_events"), recorded);

    let answer = post_json(&deployment, path, asked.clone()).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    let erased = &answer.json()["erased"];
    assert!(erased["rows"].as_i64().unwrap() >= recorded, "{erased}");
    assert!(erased["bytes_after"].as_u64().unwrap() <= erased["bytes_before"].as_u64().unwrap());
    assert_eq!(rows(&deployment, "optimization_events"), 0);

    // The fragments are not statistics, and the schema is where it was.
    let status = send(&deployment, "GET", "/v1/maintenance?format=json", &[])
        .await
        .json();
    assert_eq!(status["erase"]["allowed"], true);
    assert_eq!(status["retention"]["refs"]["live_refs"], 1);
    let stats = &status["databases"][0];
    assert_eq!(stats["schema_version"], stats["expected_schema_version"]);
    assert_eq!(stats["oldest_ms"], Value::Null);
    // The same writer carries on into the same file.
    send(&deployment, "POST", "/v1/compress", &[]).await;
    let _ = deployment.sink.flush();
    assert!(rows(&deployment, "optimization_events") >= 1);
    // Erasing does not turn the housekeeping on.
    assert_eq!(
        send(
            &deployment,
            "POST",
            "/v1/maintenance/databases/stats.db/vacuum",
            &[]
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

/// What works on every tenant's data is not a tenant's to ask for.
#[tokio::test]
async fn with_tenants_only_the_shared_secret_erases_or_reads_the_breakdown() {
    let deployment = deployment(
        |config| {
            erasing(config);
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
    let secret = [("x-kompressor-secret", "the-shared-secret")];
    let space = "/v1/maintenance/databases/stats.db/space";
    assert_eq!(
        send(&deployment, "GET", space, &[]).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&deployment, "GET", space, &key).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&deployment, "GET", space, &secret).await.status,
        StatusCode::OK
    );
    let status = send(&deployment, "GET", "/v1/maintenance?format=json", &key)
        .await
        .json();
    assert_eq!(status["space"]["allowed"], false);
    assert_eq!(status["erase"]["enabled"], true);
    assert_eq!(status["erase"]["allowed"], false);

    let erase = |headers: [(&'static str, &'static str); 1]| {
        Request::builder()
            .method("POST")
            .uri("/v1/maintenance/statistics/erase")
            .header("content-type", "application/json")
            .header(headers[0].0, headers[0].1)
            .body(Body::from(json!({"confirm": "nuke all stats"}).to_string()))
            .unwrap()
    };
    assert_eq!(
        dispatch(&deployment, erase(key)).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        dispatch(&deployment, erase(secret)).await.status,
        StatusCode::OK
    );
}

/// Commit what the writer holds, so the status reads it back.
fn flushed(deployment: &Deployment) {
    deployment.sink.flush().expect("the writer commits");
}

/// The configuration the deployment's files are under, for writing an
/// archive of them from outside the server.
fn files_of(deployment: &Deployment) -> Config {
    let mut config = Config::default();
    config.stats.path = deployment.dir.path().join("stats.db");
    config.refs.backend = RefsBackend::Sqlite;
    config.refs.path = deployment.dir.path().join("refs.db");
    config
}

/// The page says what is running — version, since when, from which binary
/// and configuration — and, to a caller who may act for every tenant, the
/// effective configuration with its secrets taken out.
#[tokio::test]
async fn the_status_says_what_is_running_and_shows_the_configuration() {
    let deployment = deployment(
        |config| {
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
    let secret = [("x-kompressor-secret", "the-shared-secret")];
    let status = send(&deployment, "GET", "/v1/maintenance?format=json", &key)
        .await
        .json();
    let build = &status["build"];
    assert_eq!(build["version"], env!("CARGO_PKG_VERSION"));
    assert!(build["started_at_ms"].as_i64().unwrap() > 0);
    assert!(build["uptime_s"].is_u64());
    assert_eq!(
        build["restart_pending"], false,
        "the test binary is what runs"
    );
    assert!(
        build["binary"]
            .as_str()
            .is_some_and(|path| !path.is_empty())
    );
    assert!(build["commit"].is_null() || build["commit"].as_str().is_some_and(|c| !c.is_empty()));

    // How it is set and what it is doing, with no key or secret in it.
    let running = &status["running"];
    assert_eq!(running["mode"], "recoverable");
    assert_eq!(running["auth"]["mode"], "api_keys");
    assert_eq!(running["auth"]["keys"], 1);
    assert_eq!(running["auth"]["secret"], true);
    assert_eq!(running["process"]["pid"], std::process::id());
    assert_eq!(running["load"]["requests"], 0);
    assert_eq!(running["load"]["backup_running"], false);
    assert!(running["load"]["engine_limit"].as_u64().unwrap() > 0);
    assert!(running["listeners"].is_array() && running["features"].is_array());
    let text = running.to_string();
    assert!(!text.contains("sk-alpha") && !text.contains("the-shared-secret"));

    let path = "/v1/maintenance/config";
    assert_eq!(
        send(&deployment, "GET", path, &key).await.status,
        StatusCode::FORBIDDEN
    );
    let shown = send(&deployment, "GET", path, &secret).await;
    assert_eq!(shown.status, StatusCode::OK);
    let text = String::from_utf8(shown.body).unwrap();
    assert!(text.contains("[maintenance]"), "{text}");
    assert!(!text.contains("the-shared-secret"), "secrets are taken out");
    assert!(!text.contains("sk-alpha"), "and so are keys");
}

/// Each action is put on record — what, on what, by whom, what came of it —
/// and the status shows the newest of each per database and the history.
#[tokio::test]
async fn maintenance_work_is_on_record_and_shown_per_database() {
    let deployment = deployment(acting, kompressor_server::router);
    for (database, action) in [
        ("stats.db", "check"),
        ("refs.db", "checkpoint"),
        ("refs.db", "vacuum"),
        ("stats.db", "optimize"),
    ] {
        let answer = send(
            &deployment,
            "POST",
            &format!("/v1/maintenance/databases/{database}/{action}"),
            &[],
        )
        .await;
        assert_eq!(answer.status, StatusCode::OK, "{database} {action}");
    }
    flushed(&deployment);
    let status = send(&deployment, "GET", "/v1/maintenance?format=json", &[])
        .await
        .json();
    let history = status["history"].as_array().unwrap();
    let actions: Vec<&str> = history
        .iter()
        .map(|w| w["action"].as_str().unwrap())
        .collect();
    assert_eq!(
        actions,
        ["optimize", "vacuum", "checkpoint", "check"],
        "newest first"
    );
    assert!(
        history
            .iter()
            .all(|w| w["origin"] == "http" && w["ok"] == true)
    );
    assert!(
        history[0]["actor"].is_null(),
        "no one in particular without auth"
    );
    assert!(history[1]["bytes_before"].is_i64() && history[1]["bytes_after"].is_i64());

    let database = |name: &str| {
        status["databases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["name"] == name)
            .unwrap()
            .clone()
    };
    let (stats, refs) = (database("stats.db"), database("refs.db"));
    assert_eq!(stats["last"]["check"]["ok"], true);
    assert_eq!(stats["last"]["optimize"]["target"], "stats.db");
    assert!(stats["last"]["vacuum"].is_null());
    assert!(refs["last"]["vacuum"]["time_ms"].is_i64());
    assert_eq!(stats["checked"]["ok"], true);
    assert_eq!(
        refs["auto_vacuum"], 2,
        "a new ref store is incremental, and a vacuum keeps it so"
    );
}

/// Midnight UTC ten days ago: recent enough that retention leaves it alone.
fn base_day_ms() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    now.div_euclid(86_400_000) * 86_400_000 - 10 * 86_400_000
}

/// Seed `n` requests of `tenant`, each with its copy of the settings, on the
/// UTC day `day` days after [`base_day_ms`].
fn requests(deployment: &Deployment, tenant: &str, run: &str, day: i64, n: usize) {
    let conn = rusqlite::Connection::open(deployment.dir.path().join("stats.db")).unwrap();
    let time = base_day_ms() + day * 86_400_000;
    for i in 0..n {
        let id = format!("{tenant}-{run}-{day}-{i}");
        conn.execute(
            "INSERT INTO proxy_requests (time_ms, tenant, request_id, api_family, upstream,
                status, stream, prompt_tokens)
             VALUES (?1, ?2, ?3, 'anthropic', 'u', 200, 0, 1200)",
            rusqlite::params![time + i as i64, tenant, id],
        )
        .unwrap();
        let context = json!({
            "build_version": "t", "config_fingerprint": "f", "experiment_id": null,
            "run_id": run, "settings": {"padding": "x".repeat(2_000)},
        });
        conn.execute(
            "INSERT INTO request_context (tenant, request_id, time_ms, model, mode, context)
             VALUES (?1, ?2, ?3, 'm', 'recoverable', ?4)",
            rusqlite::params![tenant, id, time + i as i64, context.to_string()],
        )
        .unwrap();
    }
}

/// Trimming is an action; deleting rows is the danger zone's and must be
/// spelled out. Both rehearse exactly, and both go on record.
#[tokio::test]
async fn statistics_are_trimmed_and_deleted_by_scope() {
    let path = "/v1/maintenance/statistics/delete";
    let off = deployment(|_| {}, kompressor_server::router);
    let trim = json!({"kind": "context", "scope": {"run_id": "bench"}, "dry_run": true});
    assert_eq!(
        post_json(&off, path, trim.clone()).await.status,
        StatusCode::NOT_FOUND
    );

    let deployment = deployment(acting, kompressor_server::router);
    requests(&deployment, "default", "bench", 0, 6);
    requests(&deployment, "default", "prod", 0, 4);
    requests(&deployment, "default", "prod", 3, 5);

    let preview = post_json(&deployment, path, trim).await;
    assert_eq!(preview.status, StatusCode::OK, "{:?}", preview.json());
    let purged = &preview.json()["purged"];
    assert_eq!(purged["dry_run"], true);
    assert_eq!(purged["tables"]["request_context"], 6);
    assert!(purged["content_bytes"].as_u64().unwrap() > 6 * 2_000);
    assert_eq!(
        rows(&deployment, "request_context WHERE context IS NULL"),
        0,
        "a rehearsal"
    );

    let done = post_json(
        &deployment,
        path,
        json!({"kind": "context", "scope": {"run_id": "bench"}}),
    )
    .await;
    assert_eq!(done.status, StatusCode::OK);
    assert_eq!(
        rows(&deployment, "request_context WHERE context IS NULL"),
        6
    );
    assert_eq!(
        rows(&deployment, "proxy_requests"),
        15,
        "every request stays"
    );

    let rows_by_day = json!({"kind": "rows", "scope": {"before_ms": base_day_ms() + 86_400_000}});
    assert_eq!(
        post_json(&deployment, path, rows_by_day.clone())
            .await
            .status,
        StatusCode::NOT_FOUND,
        "deleting rows needs maintenance.erase"
    );

    let dangerous = deployment_with_erase();
    requests(&dangerous, "default", "prod", 0, 4);
    requests(&dangerous, "default", "prod", 3, 5);
    let unconfirmed = post_json(&dangerous, path, rows_by_day.clone()).await;
    assert_eq!(unconfirmed.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(String::from_utf8_lossy(&unconfirmed.body).contains("delete statistics"));
    let mut rehearsal = rows_by_day.clone();
    rehearsal["dry_run"] = json!(true);
    let rehearsed = post_json(&dangerous, path, rehearsal).await.json();
    assert_eq!(rehearsed["purged"]["tables"]["proxy_requests"], 4);
    assert_eq!(rows(&dangerous, "proxy_requests"), 9);
    let mut confirmed = rows_by_day;
    confirmed["confirm"] = json!("delete statistics");
    let deleted = post_json(&dangerous, path, confirmed).await;
    assert_eq!(deleted.status, StatusCode::OK);
    assert_eq!(deleted.json()["purged"]["rolled_up"], true);
    assert_eq!(rows(&dangerous, "proxy_requests"), 5);
    assert_eq!(
        rows(&dangerous, "daily_requests"),
        1,
        "the day was rolled up first"
    );

    let empty = post_json(
        &dangerous,
        path,
        json!({"kind": "rows", "scope": {}, "dry_run": true}),
    )
    .await;
    assert_eq!(empty.status, StatusCode::UNPROCESSABLE_ENTITY);
    let unknown = post_json(
        &dangerous,
        path,
        json!({"kind": "rows", "scope": {"day": 1}}),
    )
    .await;
    assert_eq!(
        unknown.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an unknown filter is no filter"
    );

    flushed(&deployment);
    flushed(&dangerous);
    let recorded = |d: &Deployment| -> Vec<String> {
        rusqlite::Connection::open(d.dir.path().join("stats.db"))
            .unwrap()
            .prepare("SELECT action FROM maintenance_events ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        recorded(&deployment),
        ["trim"],
        "a rehearsal is not on record"
    );
    assert_eq!(recorded(&dangerous), ["delete"]);
}

fn deployment_with_erase() -> Deployment {
    deployment(
        |config| {
            acting(config);
            erasing(config);
        },
        kompressor_server::router,
    )
}

/// A scheduled archive is verified as a restore would check it; the verdict
/// is kept and shown beside the archive, a damaged one fails, and only names
/// the schedule writes are opened.
#[tokio::test]
async fn a_scheduled_archive_is_verified_and_the_verdict_kept() {
    use kompressor_core::backup::{archive_name, write_archive, write_checksum};
    let backups = tempfile::tempdir().unwrap();
    let dir = backups.path().to_path_buf();
    let schedule_dir = dir.clone();
    let deployment = deployment(
        move |config| {
            acting(config);
            config.maintenance.backup_dir = schedule_dir;
        },
        kompressor_server::router,
    );
    flushed(&deployment);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let good = archive_name(now - 3_600_000);
    write_archive(&files_of(&deployment), &dir.join(&good)).unwrap();
    let sum = write_checksum(&dir.join(&good)).unwrap();
    let bad = archive_name(now - 7_200_000);
    std::fs::write(dir.join(&bad), b"not a gzip at all").unwrap();

    let answer = send(
        &deployment,
        "POST",
        &format!("/v1/maintenance/backups/{good}/verify"),
        &[],
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK, "{:?}", answer.json());
    let verified = &answer.json()["verified"];
    assert_eq!(verified["ok"], true, "{verified}");
    assert_eq!(verified["checksum"], "matches");
    assert_eq!(verified["sha256"], sum);
    assert_eq!(verified["databases"].as_array().unwrap().len(), 2);

    let answer = send(
        &deployment,
        "POST",
        &format!("/v1/maintenance/backups/{bad}/verify"),
        &[],
    )
    .await;
    assert_eq!(
        answer.status,
        StatusCode::OK,
        "a damaged archive is a verdict"
    );
    assert_eq!(answer.json()["verified"]["ok"], false);

    for not_ours in ["notes.txt", "..%2Fstats.db", &archive_name(now)] {
        let answer = send(
            &deployment,
            "POST",
            &format!("/v1/maintenance/backups/{not_ours}/verify"),
            &[],
        )
        .await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{not_ours}");
    }

    flushed(&deployment);
    let status = send(&deployment, "GET", "/v1/maintenance?format=json", &[])
        .await
        .json();
    let schedule = &status["schedule"];
    assert_eq!(schedule["verified"][&good]["ok"], true);
    assert_eq!(schedule["verified"][&bad]["ok"], false);
    assert_eq!(schedule["archives"][0]["name"], good.as_str());
    assert_eq!(schedule["archives"][0]["sha256"], sum);
    assert!(schedule["archives"][1]["sha256"].is_null());
    assert!(
        std::fs::read_dir(&dir).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".kompressor")),
        "nothing unpacked is left behind"
    );

    let refused = crate::deployment(
        |config: &mut Config| config.maintenance.backup_dir = dir.clone(),
        kompressor_server::router,
    );
    assert_eq!(
        send(
            &refused,
            "POST",
            &format!("/v1/maintenance/backups/{good}/verify"),
            &[]
        )
        .await
        .status,
        StatusCode::NOT_FOUND,
        "verifying is an action"
    );
}
