//! The HTTP surface, driven the way a client drives it.
//!
//! Every test here goes through the real [`axum::Router`] — middleware, body
//! limit, error envelope and all — because most of what this crate owns lives
//! in the layers rather than in the handlers, and a test that called a handler
//! directly would assert none of it.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use kompressor_core::config::{ApiKey, AuthMode, RefsBackend};
use kompressor_core::{AppCore, Config};
use kompressor_engine::RefId;
use kompressor_stats::{StatsConfig, StatsHandle, StatsSink};
use kompressor_store::types::{Lookup, PutMeta, RefStore, StoreError, StoreStats, Tenant};
use serde_json::{Value, json};
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

struct Harness {
    router: Router,
    sink: StatsSink,
    stats: Option<StatsHandle>,
    _dir: Option<tempfile::TempDir>,
}

impl Harness {
    /// A server with in-memory refs and no statistics: what most assertions
    /// here need, and nothing that touches the disk.
    fn memory() -> Harness {
        Harness::build(|_| {}, None, None)
    }

    /// A server whose configuration is tweaked before anything is built.
    fn with_config(tweak: impl FnOnce(&mut Config)) -> Harness {
        Harness::build(tweak, None, None)
    }

    /// A server writing to a real statistics database, for the reporting
    /// endpoints: the queries under test are SQL over a real schema, and an
    /// in-memory fake would test none of it.
    fn with_stats() -> Harness {
        Harness::with_stats_and(|_| {}, kompressor_server::router)
    }

    /// Statistics on, a further config tweak, and the router to serve.
    fn with_stats_and(
        tweak: impl FnOnce(&mut Config),
        make_router: fn(Arc<kompressor_server::ServerState>) -> Router,
    ) -> Harness {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("stats.db");
        let (sink, handle) =
            kompressor_stats::spawn(&StatsConfig::at(&path)).expect("the writer starts");
        let sink_for_core = sink.clone();
        let mut harness = Harness::build_with(
            move |config| {
                config.stats.enabled = true;
                config.stats.path = path.clone();
                tweak(config);
            },
            Some(sink_for_core),
            Some(dir),
            make_router,
        );
        harness.sink = sink;
        harness.stats = Some(handle);
        harness
    }

    fn build(
        tweak: impl FnOnce(&mut Config),
        sink: Option<StatsSink>,
        dir: Option<tempfile::TempDir>,
    ) -> Harness {
        Harness::build_with(tweak, sink, dir, kompressor_server::router)
    }

    fn build_with(
        tweak: impl FnOnce(&mut Config),
        sink: Option<StatsSink>,
        dir: Option<tempfile::TempDir>,
        make_router: fn(Arc<kompressor_server::ServerState>) -> Router,
    ) -> Harness {
        let mut config = Config::default();
        config.refs.backend = RefsBackend::Memory;
        config.stats.enabled = false;
        tweak(&mut config);

        let sink = sink.unwrap_or_else(kompressor_stats::null_sink);
        let refs = kompressor_core::build_ref_store(&config).expect("a memory store opens");
        let core = Arc::new(AppCore::new(config, refs, sink.clone()));
        let state = kompressor_server::ServerState::new(core);
        Harness {
            router: make_router(state),
            sink,
            stats: None,
            _dir: dir,
        }
    }

    /// A server whose ref store answers every call with an error.
    fn broken_store() -> Harness {
        let mut config = Config::default();
        config.refs.backend = RefsBackend::Memory;
        config.stats.enabled = false;
        let core = Arc::new(AppCore::new(
            config,
            Arc::new(BrokenStore),
            kompressor_stats::null_sink(),
        ));
        let state = kompressor_server::ServerState::new(core);
        Harness {
            router: kompressor_server::router(state),
            sink: kompressor_stats::null_sink(),
            stats: None,
            _dir: None,
        }
    }

    async fn send(&self, request: Request<Body>) -> (StatusCode, Value, String) {
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("the router answers");
        let status = response.status();
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("a body")
            .to_bytes();
        let value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        (status, value, request_id)
    }

    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let (status, value, _) = self.post_with(path, body, &[]).await;
        (status, value)
    }

    async fn post_with(
        &self,
        path: &str,
        body: Value,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value, String) {
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let request = request
            .body(Body::from(serde_json::to_vec(&body).expect("a body")))
            .expect("a request");
        self.send(request).await
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        let (status, value, _) = self.get_with(path, &[]).await;
        (status, value)
    }

    async fn delete_with(&self, path: &str, headers: &[(&str, &str)]) -> (StatusCode, Value) {
        let mut request = Request::builder().method("DELETE").uri(path);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let request = request.body(Body::empty()).expect("a request");
        let (status, value, _) = self.send(request).await;
        (status, value)
    }

    async fn get_with(&self, path: &str, headers: &[(&str, &str)]) -> (StatusCode, Value, String) {
        let mut request = Request::builder().method("GET").uri(path);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let request = request.body(Body::empty()).expect("a request");
        self.send(request).await
    }
}

/// A store that will not answer, for the probes.
struct BrokenStore;

impl RefStore for BrokenStore {
    fn put_many(
        &self,
        _tenant: &Tenant,
        _refs: &[(RefId, String)],
        _meta: &PutMeta,
    ) -> Result<(), StoreError> {
        Err(broken())
    }

    fn get(&self, _tenant: &Tenant, _id: &RefId, _touch: bool) -> Result<Lookup, StoreError> {
        Err(broken())
    }

    fn touch_many(&self, _tenant: &Tenant, _ids: &[RefId]) -> Result<usize, StoreError> {
        Err(broken())
    }

    fn purge_expired(&self, _now: u64) -> Result<u64, StoreError> {
        Err(broken())
    }

    fn stats(&self) -> Result<StoreStats, StoreError> {
        Err(broken())
    }

    fn stats_for(
        &self,
        _tenant: &kompressor_store::types::Tenant,
    ) -> Result<StoreStats, StoreError> {
        Err(broken())
    }
}

fn broken() -> StoreError {
    StoreError::CorruptRow {
        id: "k_000000000000".to_string(),
        detail: "this store is broken on purpose",
    }
}

/// A payload big and repetitive enough that every compressor is interested.
fn log_payload(lines: usize) -> String {
    (0..lines)
        .map(|i| {
            format!(
                "2026-01-02T03:04:{:02}Z INFO worker heartbeat seq={i} queue=7 latency_ms=12",
                i % 60
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn keyed_config(config: &mut Config) {
    config.auth.mode = AuthMode::ApiKeys;
    config.auth.secret = "cluster-secret".to_string();
    config.auth.keys = vec![
        ApiKey {
            key: "sk-alpha".to_string(),
            tenant: "team-a".to_string(),
            enabled: true,
            mode: None,
        },
        ApiKey {
            key: "sk-bravo".to_string(),
            tenant: "team-b".to_string(),
            enabled: true,
            mode: None,
        },
        ApiKey {
            key: "sk-retired".to_string(),
            tenant: "team-c".to_string(),
            enabled: false,
            mode: None,
        },
    ];
}

/// The first handle a compressed conversation offers the model.
fn handle_in(value: &Value) -> String {
    let text = value.to_string();
    let found = kompressor_engine::refs_in(&text);
    assert!(
        !found.is_empty(),
        "the response carries a handle: {text:.400}"
    );
    found[0].to_string()
}

// ---------------------------------------------------------------------------
// compression
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_conversation_compresses_and_the_response_carries_both_messages_and_stats() {
    let harness = Harness::memory();
    let payload = log_payload(400);
    let (status, body) = harness
        .post(
            "/v1/compress",
            json!({
                "messages": [
                    {"role": "user", "content": "what happened in the build?"},
                    {"role": "user", "content": payload}
                ],
                "model": "claude-x",
                "provider": "anthropic"
            }),
        )
        .await;

    assert_eq!(status, StatusCode::OK);
    let messages = body["messages"].as_array().expect("messages come back");
    assert_eq!(messages.len(), 2);
    let compressed = messages[1]["content"].as_str().unwrap();
    assert!(
        compressed.chars().count() < payload.chars().count(),
        "the payload shrank"
    );
    assert!(body["stats"]["tokens_in"].as_u64().unwrap() > 0);
    assert!(
        body["stats"]["tokens_out"].as_u64().unwrap()
            < body["stats"]["tokens_in"].as_u64().unwrap()
    );
    assert_eq!(body["stats"]["compressed_messages"], 1);
    assert!(body["stats"]["by_type"].is_object());
}

#[tokio::test]
async fn unknown_message_fields_come_back_byte_identical() {
    let harness = Harness::memory();
    let original = json!({
        "role": "tool",
        "content": "short enough to be left alone",
        "tool_call_id": "call_1",
        "name": "Bash",
        "annotations": [{"kind": "citation", "index": 3}],
        "weird": {"nested": {"deep": [1, 2, {"x": null}]}}
    });
    let (status, body) = harness
        .post("/v1/compress", json!({ "messages": [original.clone()] }))
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::to_string(&body["messages"][0]).unwrap(),
        serde_json::to_string(&original).unwrap(),
        "a message the engine did not touch is returned exactly as it arrived"
    );
}

#[tokio::test]
async fn unknown_fields_survive_a_message_that_was_compressed() {
    let harness = Harness::memory();
    let (status, body) = harness
        .post(
            "/v1/compress",
            json!({
                "messages": [{
                    "role": "user",
                    "content": log_payload(400),
                    "tool_call_id": "call_9",
                    "provenance": {"tool": "Bash", "exit": 0}
                }]
            }),
        )
        .await;

    assert_eq!(status, StatusCode::OK);
    let message = &body["messages"][0];
    assert_eq!(message["tool_call_id"], "call_9");
    assert_eq!(message["provenance"]["exit"], 0);
    assert!(
        message["content"].as_str().unwrap().chars().count() < log_payload(400).chars().count()
    );
}

#[tokio::test]
async fn block_content_compresses_per_slot_and_a_protected_slot_is_untouched() {
    let harness = Harness::memory();
    let free = log_payload(400);
    let pinned = log_payload(400);
    let (status, body) = harness
        .post(
            "/v1/compress",
            json!({
                "messages": [{
                    "role": "user",
                    "content": [
                        {"type": "text", "text": free},
                        {"type": "text", "text": pinned, "cache_control": {"type": "ephemeral"}},
                        {"type": "image", "source": {"type": "base64", "data": "AAAA"}}
                    ]
                }]
            }),
        )
        .await;

    assert_eq!(status, StatusCode::OK);
    let blocks = body["messages"][0]["content"].as_array().unwrap();
    assert!(
        blocks[0]["text"].as_str().unwrap().chars().count() < free.chars().count(),
        "the free slot compressed"
    );
    assert_eq!(
        blocks[1]["text"].as_str().unwrap(),
        pinned,
        "a cache breakpoint's bytes are load-bearing and must survive"
    );
    assert_eq!(
        blocks[2]["type"], "image",
        "an unreadable block is untouched"
    );
}

#[tokio::test]
async fn the_assistant_and_system_turns_come_back_unchanged_and_protected_by_reason_says_why() {
    let harness = Harness::memory();
    let payload = log_payload(400);
    let (status, body) = harness
        .post(
            "/v1/compress",
            json!({
                "messages": [
                    {"role": "system", "content": payload},
                    {"role": "assistant", "content": payload},
                    {"role": "user", "content": payload}
                ]
            }),
        )
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["messages"][0]["content"].as_str().unwrap(), payload);
    assert_eq!(body["messages"][1]["content"].as_str().unwrap(), payload);
    assert!(
        body["messages"][2]["content"]
            .as_str()
            .unwrap()
            .chars()
            .count()
            < payload.chars().count()
    );

    let reasons = &body["stats"]["protected_by_reason"];
    assert_eq!(reasons["role:system"], 1);
    assert_eq!(reasons["role:assistant"], 1);
}

#[tokio::test]
async fn a_frozen_prefix_is_returned_byte_identical() {
    let harness = Harness::memory();
    let payload = log_payload(400);
    let (status, body) = harness
        .post(
            "/v1/compress",
            json!({
                "messages": [
                    {"role": "user", "content": payload},
                    {"role": "user", "content": payload}
                ],
                "frozen_message_count": 1
            }),
        )
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["messages"][0]["content"].as_str().unwrap(),
        payload,
        "the provider-cached prefix must come back unchanged or the cache is lost"
    );
    assert!(
        body["messages"][1]["content"]
            .as_str()
            .unwrap()
            .chars()
            .count()
            < payload.chars().count()
    );
    assert_eq!(body["stats"]["protected_by_reason"]["frozen"], 1);
}

#[tokio::test]
async fn a_raw_payload_compresses_on_its_own_endpoint() {
    let harness = Harness::memory();
    let payload = log_payload(400);
    let (status, body) = harness
        .post("/v1/compress/raw", json!({ "content": payload }))
        .await;

    assert_eq!(status, StatusCode::OK);
    assert!(body["content"].as_str().unwrap().chars().count() < payload.chars().count());
    assert_eq!(body["stats"]["compressed_messages"], 1);
}

#[tokio::test]
async fn detect_reports_the_type_the_strategy_and_the_kind() {
    let harness = Harness::memory();
    let (status, body) = harness
        .post("/v1/detect", json!({ "content": log_payload(400) }))
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["content_type"], "build");
    assert_eq!(body["strategy"], "log");
    assert_eq!(body["kind"], "log");
    assert!(body["confidence"].as_f64().unwrap() > 0.0);
}

// ---------------------------------------------------------------------------
// retrieval
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_handle_minted_by_compress_resolves_through_retrieve() {
    let harness = Harness::memory();
    let (_, compressed) = harness
        .post(
            "/v1/compress",
            json!({"messages": [{"role": "user", "content": log_payload(400)}]}),
        )
        .await;
    let handle = handle_in(&compressed["messages"]);

    let (status, body) = harness.post("/v1/retrieve", json!({ "ref": handle })).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["found"], true);
    assert_eq!(body["expired"], false);
    assert_eq!(body["ranged"], false);
    assert!(body["total_lines"].as_u64().unwrap() > 0);
    assert!(!body["content"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn a_ranged_retrieval_returns_less_than_the_whole_fragment() {
    let harness = Harness::memory();
    let (_, compressed) = harness
        .post(
            "/v1/compress",
            json!({"messages": [{"role": "user", "content": log_payload(400)}]}),
        )
        .await;
    let handle = handle_in(&compressed["messages"]);

    let (_, whole) = harness.post("/v1/retrieve", json!({ "ref": handle })).await;
    let (status, window) = harness
        .post(
            "/v1/retrieve",
            json!({ "ref": handle, "range": {"lines": {"start": 1, "end": 3}} }),
        )
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(window["ranged"], true);
    assert!(
        window["content"].as_str().unwrap().len() < whole["content"].as_str().unwrap().len(),
        "a range is what keeps a retrieval from costing more than the compression saved"
    );
}

#[tokio::test]
async fn a_handle_does_not_resolve_for_another_api_keys_tenant() {
    let harness = Harness::with_config(keyed_config);
    let (status, compressed, _) = harness
        .post_with(
            "/v1/compress",
            json!({"messages": [{"role": "user", "content": log_payload(400)}]}),
            &[("authorization", "Bearer sk-alpha")],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let handle = handle_in(&compressed["messages"]);

    let (_, mine, _) = harness
        .post_with(
            "/v1/retrieve",
            json!({ "ref": handle }),
            &[("authorization", "Bearer sk-alpha")],
        )
        .await;
    assert_eq!(mine["found"], true);

    let (status, theirs, _) = harness
        .post_with(
            "/v1/retrieve",
            json!({ "ref": handle }),
            &[("x-api-key", "sk-bravo")],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        theirs["found"], false,
        "the tenant is part of the key, not a filter applied afterwards"
    );
}

#[tokio::test]
async fn a_handle_nobody_minted_is_a_miss_that_tells_the_model_to_regenerate() {
    let harness = Harness::memory();
    let (status, body) = harness
        .post("/v1/retrieve", json!({ "ref": "k_AAAAAAAAAAAA" }))
        .await;

    assert_eq!(status, StatusCode::OK, "a miss is an answer, not an error");
    assert_eq!(body["found"], false);
    let hint = body["hint"].as_str().unwrap();
    assert!(hint.contains("Re-run") && hint.contains("rather than reconstructing"));
}

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_option_key_is_refused_rather_than_ignored() {
    let harness = Harness::memory();
    let (status, body) = harness
        .post(
            "/v1/compress",
            json!({
                "messages": [{"role": "user", "content": "hi"}],
                "options": {"enable_smart_crushr": false}
            }),
        )
        .await;

    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a silently ignored typo is a knob that does not do what it says"
    );
    assert_eq!(body["error"]["code"], "invalid_request");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("enable_smart_crushr")
    );
}

#[tokio::test]
async fn an_out_of_range_option_is_refused_rather_than_clamped() {
    let harness = Harness::memory();
    let (status, _) = harness
        .post(
            "/v1/compress",
            json!({
                "messages": [{"role": "user", "content": "hi"}],
                "options": {"max_array_items": 0}
            }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn a_body_that_is_not_json_is_400_and_one_of_the_wrong_shape_is_422() {
    let harness = Harness::memory();
    let request = Request::builder()
        .method("POST")
        .uri("/v1/compress")
        .header("content-type", "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    let (status, body, _) = harness.send(request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "bad_json");

    let (status, _) = harness
        .post(
            "/v1/compress",
            json!({"messages": [{"content": "no role"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, _) = harness
        .post("/v1/retrieve", json!({"handle": "k_AAAAAAAAAAAA"}))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn a_missing_or_unknown_credential_is_401_and_a_disabled_one_is_403() {
    let harness = Harness::with_config(keyed_config);
    let body = json!({"messages": [{"role": "user", "content": "hi"}]});

    let (status, refused, _) = harness.post_with("/v1/compress", body.clone(), &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(refused["error"]["code"], "unauthorized");

    let (status, _, _) = harness
        .post_with(
            "/v1/compress",
            body.clone(),
            &[("authorization", "Bearer sk-guessed")],
        )
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "an unknown key reads the same as none: otherwise a guess is an oracle"
    );

    let (status, disabled, _) = harness
        .post_with("/v1/compress", body, &[("x-api-key", "sk-retired")])
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(disabled["error"]["code"], "forbidden");
}

#[tokio::test]
async fn a_body_over_the_configured_limit_is_413_in_the_same_envelope() {
    let harness = Harness::with_config(|config| config.server.max_body_bytes = 2048);
    let (status, body) = harness
        .post(
            "/v1/compress",
            json!({"messages": [{"role": "user", "content": log_payload(400)}]}),
        )
        .await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        body["error"]["code"], "payload_too_large",
        "a refusal from a layer reads the same as one from a handler"
    );
    assert!(body["error"]["request_id"].is_string());
}

#[tokio::test]
async fn an_error_carries_the_request_id_the_caller_supplied() {
    let harness = Harness::memory();
    let (status, body, echoed) = harness
        .post_with(
            "/v1/compress",
            json!({"messages": "not a list"}),
            &[("x-request-id", "trace-abc-123")],
        )
        .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["request_id"], "trace-abc-123");
    assert_eq!(echoed, "trace-abc-123");
}

#[tokio::test]
async fn a_request_id_that_could_forge_a_log_line_is_replaced() {
    let harness = Harness::memory();
    let (status, _, echoed) = harness
        .post_with(
            "/v1/compress",
            json!({"messages": []}),
            &[("x-request-id", "abc def")],
        )
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_ne!(echoed, "abc def");
    assert_eq!(echoed.len(), 26, "a generated identifier took its place");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_saturated_engine_refuses_with_a_retry_after_rather_than_queueing() {
    let harness = Arc::new(Harness::with_config(|config| {
        config.server.max_inflight_compressions = 1;
        // Nothing waits: a caller that cannot have the engine now is told so
        // now, because its right move is to send the prompt uncompressed.
        config.server.queue_timeout = std::time::Duration::from_millis(0);
    }));
    let payload = log_payload(4000);

    let mut inflight = Vec::new();
    for _ in 0..24 {
        let harness = harness.clone();
        let payload = payload.clone();
        inflight.push(tokio::spawn(async move {
            let request = Request::builder()
                .method("POST")
                .uri("/v1/compress")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "messages": [{"role": "user", "content": payload}]
                    }))
                    .unwrap(),
                ))
                .unwrap();
            let response = harness.router.clone().oneshot(request).await.unwrap();
            let status = response.status();
            let retry_after = response
                .headers()
                .get("retry-after")
                .map(|value| value.to_str().unwrap().to_string());
            (status, retry_after)
        }));
    }

    let mut refused = 0;
    for task in inflight {
        let (status, retry_after) = task.await.unwrap();
        if status == StatusCode::SERVICE_UNAVAILABLE {
            refused += 1;
            assert!(
                retry_after.is_some(),
                "a refusal tells the caller when to come back"
            );
        } else {
            assert_eq!(status, StatusCode::OK);
        }
    }
    assert!(
        refused > 0,
        "one permit and no queue means some of twenty-four callers are turned away"
    );
}

#[tokio::test]
async fn an_unknown_path_is_refused_in_the_same_envelope() {
    let harness = Harness::memory();
    let (status, body) = harness.get("/v1/nonsense").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
}

// ---------------------------------------------------------------------------
// the /v2 aliases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_v2_aliases_behave_identically_to_v1() {
    let harness = Harness::memory();
    let body = json!({"messages": [{"role": "user", "content": log_payload(400)}]});

    let (v1_status, v1) = harness.post("/v1/compress", body.clone()).await;
    let (v2_status, v2) = harness.post("/v2/compress", body).await;

    assert_eq!(v1_status, v2_status);
    assert_eq!(
        v1["messages"], v2["messages"],
        "an existing client pointing at /v2 must not have to be rewritten"
    );

    let handle = handle_in(&v2["messages"]);
    let (status, retrieved) = harness.post("/v2/retrieve", json!({ "ref": handle })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(retrieved["found"], true);

    let (status, _) = harness
        .post("/v2/compress/raw", json!({"content": log_payload(400)}))
        .await;
    assert_eq!(status, StatusCode::OK);
    // the reporting routes are aliased too: this deployment has statistics off,
    // so the answer is "disabled" rather than the router's own "no such path"
    let (_, stats) = harness.get("/v2/stats/summary").await;
    assert!(
        stats["error"]["message"]
            .as_str()
            .unwrap()
            .contains("disabled")
    );
}

// ---------------------------------------------------------------------------
// operational endpoints
// ---------------------------------------------------------------------------

#[tokio::test]
async fn healthz_answers_even_when_the_store_is_broken_and_readyz_does_not() {
    let harness = Harness::broken_store();

    let (status, body) = harness.get("/healthz").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a liveness probe that consults a dependency turns a broken database into a restart loop"
    );
    assert_eq!(body["status"], "ok");

    let (status, body) = harness.get("/readyz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["checks"]["ref_store"], false);
}

#[tokio::test]
async fn readyz_passes_when_the_store_and_the_stats_writer_answer() {
    let harness = Harness::with_stats();
    let (status, body) = harness.get("/readyz").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ready");
    assert_eq!(body["checks"]["stats"], true);
}

#[tokio::test]
async fn the_config_endpoint_never_shows_a_configured_key_or_secret() {
    let harness = Harness::with_config(keyed_config);
    let (status, body, _) = harness
        .get_with("/v1/config", &[("authorization", "Bearer sk-alpha")])
        .await;

    assert_eq!(status, StatusCode::OK);
    let rendered = body.as_str().expect("TOML comes back as text");
    assert!(!rendered.contains("sk-alpha"));
    assert!(!rendered.contains("sk-retired"));
    assert!(!rendered.contains("cluster-secret"));
    assert!(rendered.contains("team-a"), "non-secret fields survive");
}

#[tokio::test]
async fn the_config_endpoint_needs_a_credential_when_auth_is_on() {
    let harness = Harness::with_config(keyed_config);
    let (status, _) = harness.get("/v1/config").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // and is open when the deployment has no keys at all, which is the local
    // single-tenant default
    let open = Harness::memory();
    let (status, _) = open.get("/v2/config").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn metrics_report_counters_that_moved() {
    let harness = Harness::memory();
    let before = counter_total(&harness, "kompressor_tokens_in_total").await;

    let (status, compressed) = harness
        .post(
            "/v1/compress",
            json!({"messages": [{"role": "user", "content": log_payload(400)}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let handle = handle_in(&compressed["messages"]);
    harness.post("/v1/retrieve", json!({ "ref": handle })).await;

    let (status, body) = harness.get("/metrics").await;
    assert_eq!(status, StatusCode::OK);
    let rendered = body.as_str().expect("Prometheus text");
    assert!(rendered.contains("kompressor_compress_requests_total"));
    assert!(rendered.contains("kompressor_compress_duration_seconds"));
    assert!(rendered.contains("kompressor_retrievals_total"));
    assert!(rendered.contains("kompressor_refs_emitted_total"));
    assert!(rendered.contains("kompressor_refs_live"));

    let after = counter_total(&harness, "kompressor_tokens_in_total").await;
    assert!(
        after > before,
        "the compression that just happened is counted: {before} -> {after}"
    );
}

/// Sum every sample of `name` in the exposition.
async fn counter_total(harness: &Harness, name: &str) -> f64 {
    let (_, body) = harness.get("/metrics").await;
    let text = body.as_str().unwrap_or_default().to_string();
    text.lines()
        .filter(|line| line.starts_with(name))
        .filter_map(|line| line.rsplit(' ').next())
        .filter_map(|value| value.parse::<f64>().ok())
        .sum()
}

// ---------------------------------------------------------------------------
// reporting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_stats_endpoints_report_what_the_requests_just_made_recorded() {
    let harness = Harness::with_stats();
    let (status, compressed, _) = harness
        .post_with(
            "/v1/compress",
            json!({
                "messages": [{"role": "user", "content": log_payload(400)}],
                "request_id": "req-report-1"
            }),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let handle = handle_in(&compressed["messages"]);
    harness
        .post(
            "/v1/retrieve",
            json!({ "ref": handle, "request_id": "req-report-1" }),
        )
        .await;
    harness.sink.flush().expect("the batch commits");

    let (status, summary) = harness.get("/v1/stats/summary").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(summary["requests"], 1);
    assert!(summary["tokens_in"].as_i64().unwrap() > summary["tokens_out"].as_i64().unwrap());
    assert!(summary["saved_gross"].as_i64().unwrap() > 0);
    assert_eq!(summary["retrievals"], 1);
    assert_eq!(summary["retrieval_hits"], 1);
    assert!(
        summary["retrieved_tokens"].as_i64().unwrap() > 0,
        "the debit is reported beside the credit"
    );

    let (status, histogram) = harness.get("/v1/stats/histogram?bucket=1h").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(histogram["bucket"], "hour");
    assert_eq!(histogram["rows"].as_array().unwrap().len(), 1);

    let (status, by_type) = harness.get("/v1/stats/by_type").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!by_type["types"].as_array().unwrap().is_empty());

    let (status, retrievals) = harness.get("/v1/stats/retrievals").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(retrievals["hits"], 1);
    assert_eq!(retrievals["top_refs"][0]["reference"], handle);

    let (status, request) = harness
        .get("/v1/stats/requests?request_id=req-report-1")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(request["request_id"], "req-report-1");
    assert_eq!(request["compressions"], 1);
    assert_eq!(request["retrieval_hits"], 1);

    let (status, store) = harness.get("/v1/stats/store").await;
    assert_eq!(status, StatusCode::OK);
    assert!(store["live_refs"].as_u64().unwrap() > 0);

    let (status, missing) = harness
        .get("/v1/stats/requests?request_id=never-happened")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing["error"]["code"], "not_found");
}

#[tokio::test]
async fn a_report_cannot_be_asked_for_on_another_tenant() {
    let harness = Harness::with_stats();

    // with auth off there is one anonymous tenant, so naming another one is the
    // only way to ask across the boundary, and it is refused
    let (status, body) = harness.get("/v1/stats/summary?tenant=team-b").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "forbidden");
}

#[tokio::test]
async fn an_unknown_query_parameter_is_refused_rather_than_ignored() {
    let harness = Harness::with_stats();
    let (status, body) = harness.get("/v1/stats/summary?tenantt=team-a").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "invalid_request");

    let (status, _) = harness.get("/v1/stats/histogram?bucket=1w").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn a_report_from_a_deployment_with_statistics_off_says_so() {
    let harness = Harness::memory();
    let (status, body) = harness.get("/v1/stats/summary").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("disabled")
    );
}

#[tokio::test]
async fn the_stats_writer_is_shut_down_cleanly() {
    // Not a behaviour of the server, but the harness owns a writer thread and a
    // test that leaks one leaks a file handle into every later test.
    let mut harness = Harness::with_stats();
    harness.get("/v1/stats/summary").await;
    if let Some(handle) = harness.stats.take() {
        handle.shutdown().expect("the writer stops cleanly");
    }
}

/// A body as text, whatever `send` made of it.
fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[tokio::test]
async fn the_live_view_reports_a_compression_moments_after_it_happened() {
    let harness = Harness::with_stats();
    let (status, _) = harness
        .post(
            "/v1/compress/raw",
            json!({"content": log_payload(400), "request_id": "live-1"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    harness.sink.flush().expect("the batch commits");

    let (status, live) = harness.get("/v1/stats/live?window=15m&recent=5").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(live["window"]["label"], "15m");
    assert_eq!(live["server"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(live["server"]["stats_dropped"], 0);
    assert_eq!(live["totals"]["compressions"], 1);
    assert!(live["totals"]["saved_gross"].as_i64().unwrap() > 0);
    assert_eq!(live["recent"][0]["request_id"], "live-1");
    assert_eq!(live["recent"][0]["surface"], "api");
    assert_eq!(live["rates"].as_array().unwrap().len(), 3);
    assert!(!live["timeline"].as_array().unwrap().is_empty());
    for section in [
        "upstream",
        "by_provider",
        "by_model",
        "by_type",
        "skipped_by_reason",
        "retrievals",
    ] {
        assert!(
            !live[section].is_null(),
            "{section} is part of the snapshot"
        );
    }
    // the versioned alias, as for every other reporting route
    let (status, _) = harness.get("/v2/stats/live").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_browser_gets_the_dashboard_and_a_client_gets_json() {
    let harness = Harness::with_stats();
    let browser = [("accept", "text/html,application/xhtml+xml,*/*;q=0.8")];
    let (status, page, _) = harness.get_with("/v1/stats/live", &browser).await;
    assert_eq!(status, StatusCode::OK);
    let page = text(&page);
    assert!(page.starts_with("<!doctype html>"));
    assert!(
        page.contains("\"window\":\"1h\""),
        "the settings are filled in"
    );

    let (status, forced, _) = harness
        .get_with("/v1/stats/live?format=json", &browser)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        forced["totals"].is_object(),
        "an explicit format beats the Accept header"
    );

    let (_, page) = harness
        .get("/v1/stats/live?format=html&window=24h&refresh=5")
        .await;
    assert!(text(&page).contains("\"refresh\":5"));
}

#[tokio::test]
async fn a_live_query_it_cannot_honour_is_refused() {
    let harness = Harness::with_stats();
    for query in [
        "window=2w",
        "window=91d",
        "recent=500",
        "format=xml",
        "refresh=0",
        "colour=red",
    ] {
        let (status, body) = harness.get(&format!("/v1/stats/live?{query}")).await;
        assert!(status.is_client_error(), "{query} answered {status}");
        assert!(body["error"]["message"].is_string(), "{query} says why");
    }
}

#[tokio::test]
async fn the_live_data_needs_a_key_where_keys_are_required_but_the_page_does_not() {
    let harness = Harness::with_stats_and(keyed_config, kompressor_server::router);
    let (status, _) = harness.get("/v1/stats/live?format=json").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // the page holds no data; it asks for the key and sends it with its polls
    let (status, page) = harness.get("/v1/stats/live?format=html").await;
    assert_eq!(status, StatusCode::OK);
    assert!(text(&page).contains("x-kompressor-key"));

    let (status, live, _) = harness
        .get_with("/v1/stats/live", &[("x-kompressor-key", "sk-alpha")])
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(live["tenant"], "team-a");

    let (status, _, _) = harness
        .get_with(
            "/v1/stats/live?tenant=team-b",
            &[("x-kompressor-key", "sk-alpha")],
        )
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a key reads only its own tenant"
    );
}

#[tokio::test]
async fn live_statistics_on_a_deployment_with_statistics_off_say_so() {
    let harness = Harness::memory();
    let (status, body) = harness.get("/v1/stats/live").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(text(&body["error"]["message"]).contains("disabled"));
}

#[tokio::test]
async fn the_statistics_listener_serves_the_dashboard_and_nothing_that_compresses() {
    let harness = Harness::with_stats_and(|_| {}, kompressor_server::stats_listener_router);
    let (status, page) = harness.get("/").await;
    assert_eq!(status, StatusCode::OK);
    let page = text(&page);
    assert!(page.contains("Kompressor live"));
    assert!(
        page.contains("\"endpoint\":\"/v1/stats/live\""),
        "the page at / must poll the JSON path, not itself"
    );

    let (status, live) = harness.get("/v1/stats/live").await;
    assert_eq!(status, StatusCode::OK);
    assert!(live["server"].is_object());

    let (status, health) = harness.get("/healthz").await;
    assert_eq!(
        (status, health["status"].as_str()),
        (StatusCode::OK, Some("ok"))
    );

    let (status, _) = harness
        .post("/v1/compress/raw", json!({"content": log_payload(10)}))
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a stats port is not a compression port"
    );
}

// ---------------------------------------------------------------------------
// ref browser
// ---------------------------------------------------------------------------

fn browsable(config: &mut Config) {
    config.refs.browse = true;
}

fn browsable_and_keyed(config: &mut Config) {
    browsable(config);
    keyed_config(config);
}

/// Compress a log big enough to elide, as `key`'s tenant, and return a handle.
async fn mint(harness: &Harness, key: Option<&str>, request_id: &str) -> String {
    let headers: Vec<(&str, &str)> = key.map(|k| ("x-kompressor-key", k)).into_iter().collect();
    let (status, compressed, _) = harness
        .post_with(
            "/v1/compress",
            json!({
                "messages": [{"role": "user", "content": log_payload(400)}],
                "request_id": request_id,
            }),
            &headers,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    handle_in(&compressed["messages"])
}

#[tokio::test]
async fn the_ref_browser_is_off_until_the_deployment_turns_it_on() {
    let harness = Harness::memory();
    mint(&harness, None, "req-off").await;
    for path in ["/v1/refs", "/v1/refs/k_AAAAAAAAAAAA"] {
        let (status, body) = harness.get(path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert!(
            text(&body["error"]["message"]).contains("refs.browse"),
            "{path} says how"
        );
    }
    let (status, _) = harness.delete_with("/v1/refs/k_AAAAAAAAAAAA", &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The page itself is served, and explains from its first fetch.
    let (status, page) = harness.get("/v1/refs?format=html").await;
    assert_eq!(status, StatusCode::OK);
    assert!(text(&page).contains("Kompressor refs"));
}

#[tokio::test]
async fn a_minted_fragment_can_be_listed_searched_inspected_and_deleted() {
    let harness = Harness::with_stats_and(browsable, kompressor_server::router);
    let handle = mint(&harness, None, "req-browse-1").await;
    let (_, retrieved) = harness.post("/v1/retrieve", json!({ "ref": handle })).await;
    assert_eq!(retrieved["found"], true);
    harness.sink.flush().expect("the batch commits");

    let (status, page) = harness.get("/v1/refs").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page["total"].as_u64().unwrap() >= 1);
    assert!(page["held"]["live_refs"].as_u64().unwrap() >= 1);
    let listed: Vec<&str> = page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["handle"].as_str().unwrap())
        .collect();
    assert!(
        listed.contains(&handle.as_str()),
        "{listed:?} lists {handle}"
    );
    assert!(!page["kinds"].as_array().unwrap().is_empty());

    for search in [handle.clone(), format!("ref:{handle}")] {
        let (_, found) = harness.get(&format!("/v1/refs?q={search}")).await;
        assert_eq!(found["total"], 1, "{search}");
        assert_eq!(found["entries"][0]["handle"], handle.as_str());
        assert_eq!(found["entries"][0]["request_id"], "req-browse-1");
    }
    let (_, by_request) = harness.get("/v1/refs?request_id=req-browse-1").await;
    assert!(by_request["total"].as_u64().unwrap() >= 1);
    let (_, nothing) = harness.get("/v1/refs?q=no-such-text-anywhere").await;
    assert_eq!(nothing["total"], 0);

    let (status, detail) = harness.get(&format!("/v1/refs/{handle}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["handle"], handle.as_str());
    assert!(text(&detail["fragment"]).contains("worker heartbeat"));
    assert!(detail["lines"].as_u64().unwrap() > 0);
    assert_eq!(detail["expired"], false);
    assert_eq!(detail["retrievals"]["retrievals"], 1);
    assert_eq!(detail["retrievals"]["hits"], 1);
    assert_eq!(detail["retrievals"]["recent"][0]["found"], true);

    // Looking is not asking: neither the list nor the detail filed a
    // retrieval or moved the fragment's clock.
    let last_accessed = detail["last_accessed"].clone();
    harness.get(&format!("/v1/refs/{handle}")).await;
    harness.sink.flush().expect("the batch commits");
    let (_, again) = harness.get(&format!("/v1/refs/{handle}")).await;
    assert_eq!(again["retrievals"]["retrievals"], 1);
    assert_eq!(again["last_accessed"], last_accessed);

    let (status, deleted) = harness
        .delete_with(&format!("/v1/refs/{handle}"), &[])
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(deleted["removed"], true);

    let (_, after) = harness.post("/v1/retrieve", json!({ "ref": handle })).await;
    assert_eq!(after["found"], false, "a deleted fragment is a miss");
    let (status, _) = harness.get(&format!("/v1/refs/{handle}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = harness
        .delete_with(&format!("/v1/refs/{handle}"), &[])
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "deleting twice finds nothing"
    );
}

#[tokio::test]
async fn the_ref_browser_shows_each_key_only_its_own_tenant() {
    let harness = Harness::with_config(browsable_and_keyed);
    let alpha = mint(&harness, Some("sk-alpha"), "req-a").await;
    let alpha_key = [("x-kompressor-key", "sk-alpha")];
    let bravo_key = [("x-kompressor-key", "sk-bravo")];

    let (status, _) = harness.get("/v1/refs").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, page, _) = harness.get_with("/v1/refs", &alpha_key).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["tenant"], "team-a");
    assert!(page["total"].as_u64().unwrap() >= 1);

    let (status, page, _) = harness.get_with("/v1/refs", &bravo_key).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["total"], 0, "bravo holds nothing");

    let (status, _, _) = harness
        .get_with(&format!("/v1/refs/{alpha}"), &bravo_key)
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "another tenant's handle is not there"
    );
    let (status, _) = harness
        .delete_with(&format!("/v1/refs/{alpha}"), &bravo_key)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = harness.get_with("/v1/refs?tenant=team-a", &bravo_key).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = harness
        .delete_with(&format!("/v1/refs/{alpha}?tenant=team-a"), &bravo_key)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // the cluster secret acts for a named tenant, as on the other reports
    let (status, page, _) = harness
        .get_with(
            "/v1/refs?tenant=team-a",
            &[("x-kompressor-secret", "cluster-secret")],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page["total"].as_u64().unwrap() >= 1);

    let (_, still, _) = harness
        .get_with(&format!("/v1/refs/{alpha}"), &alpha_key)
        .await;
    assert_eq!(
        still["handle"],
        alpha.as_str(),
        "bravo's delete changed nothing"
    );
}

#[tokio::test]
async fn the_ref_browser_refuses_parameters_it_does_not_serve() {
    let harness = Harness::with_config(browsable);
    for query in [
        "limit=0",
        "limit=201",
        "preview=5000",
        "order=random",
        "expired=maybe",
        "format=xml",
        "refresh=0",
        "colour=red",
    ] {
        let (status, body) = harness.get(&format!("/v1/refs?{query}")).await;
        assert!(status.is_client_error(), "{query} answered {status}");
        assert!(body["error"]["message"].is_string(), "{query} says why");
    }
    let (status, _) = harness.get("/v1/refs/too-short").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = harness.get("/v1/refs/k_AAAAAAAAAAAA?history=999").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn the_statistics_listener_serves_the_ref_browser_beside_the_dashboard() {
    let harness = Harness::with_stats_and(browsable, kompressor_server::stats_listener_router);
    let (status, page) = harness.get("/refs").await;
    assert_eq!(status, StatusCode::OK);
    let page = text(&page);
    assert!(page.contains("Kompressor refs"));
    assert!(
        page.contains("\"endpoint\":\"/v1/refs\""),
        "the page at /refs must fetch the JSON path, not itself"
    );
    let (status, list) = harness.get("/v1/refs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["total"], 0);

    let (_, dashboard) = harness.get("/").await;
    assert!(
        text(&dashboard).contains("\"refs\":true"),
        "the dashboard links it"
    );
}
