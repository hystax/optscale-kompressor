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
