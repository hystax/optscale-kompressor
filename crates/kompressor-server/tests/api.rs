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

// ---------------------------------------------------------------------------
// cache analysis
// ---------------------------------------------------------------------------

fn proxied_turn(
    time_ms: i64,
    tenant: &str,
    read: u32,
    write: u32,
    keep_warm: bool,
) -> kompressor_stats::Event {
    kompressor_stats::Event::ProxyRequest(kompressor_stats::ProxyRequestEvent {
        prefix: None,
        context: None,
        timing: None,
        time_ms,
        tenant: tenant.into(),
        request_id: format!("req-{time_ms}-{keep_warm}"),
        api_family: "anthropic_messages".into(),
        model: Some("claude-opus-5-5".into()),
        upstream: "https://api.anthropic.com".into(),
        status: Some(200),
        stream: !keep_warm,
        strip_usage: false,
        prompt_tokens: Some(8),
        completion_tokens: Some(if keep_warm { 1 } else { 120 }),
        cache_read_tokens: Some(read),
        cache_write_tokens: Some(write),
        cache_write_1h_tokens: Some(0),
        shadow_prompt_tokens: None,
        conversation: Some("0123456789abcdef".into()),
        retrieve_rounds: 0,
        retrieve_tokens_injected: 0,
        upstream_ms: Some(800.0),
        total_ms: (!keep_warm).then_some(900.0),
        error: None,
        keep_warm,
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[tokio::test]
async fn the_cache_view_classifies_each_turn_and_names_the_cause() {
    let harness = Harness::with_stats();
    let (status, empty) = harness.get("/v1/stats/cache").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty["window"], "6h");
    assert_eq!(empty["rows"], 0);
    let tenant = text(&empty["tenant"]);

    const MIN: i64 = 60_000;
    let t0 = now_ms() - 60 * MIN;
    for event in [
        proxied_turn(t0, &tenant, 0, 40_000, false),
        // a ping holds the cache across a seven-minute pause
        proxied_turn(t0 + 4 * MIN, &tenant, 40_000, 0, true),
        proxied_turn(t0 + 7 * MIN, &tenant, 40_000, 2_000, false),
        // then a twenty-minute pause nobody bridged
        proxied_turn(t0 + 27 * MIN, &tenant, 0, 42_500, false),
    ] {
        harness.sink.send(event);
    }
    harness.sink.flush().expect("the batch commits");

    let (status, report) = harness
        .get("/v1/stats/cache?window=2h&conversation=0123456789abcdef")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["rows"], 4);
    assert_eq!(report["clients"]["requests"], 3);
    assert_eq!(report["keep_warm"]["pings"], 1);
    assert_eq!(report["keep_warm"]["warm"], 1);
    assert_eq!(report["keep_warm"]["bridged"], 1);
    assert_eq!(report["losses"]["ttl_expiry_events"], 1);
    assert_eq!(report["losses"]["ttl_expiry_tokens"], 42_000);
    assert_eq!(report["losses"]["prefix_change_events"], 0);
    assert_eq!(report["conversations_total"], 1);
    assert_eq!(
        report["conversations"][0]["conversation"],
        "0123456789abcdef"
    );
    let kinds: Vec<&str> = report["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["first", "hit", "hit", "miss"]);
    assert_eq!(report["turns"][3]["cause"], "ttl_expiry");
    assert!(report["totals"]["saved_share"].as_f64().unwrap() > 0.0);

    // without a conversation named, no turns are listed
    let (_, report) = harness.get("/v2/stats/cache?window=2h").await;
    assert!(report["turns"].is_null());
}

#[tokio::test]
async fn a_cache_query_it_cannot_honour_is_refused() {
    let harness = Harness::with_stats();
    for query in [
        "window=2w",
        "conversations=501",
        "conversation=XYZ",
        "format=xml",
        "refresh=0",
        "colour=red",
    ] {
        let (status, body) = harness.get(&format!("/v1/stats/cache?{query}")).await;
        assert!(status.is_client_error(), "{query} answered {status}");
        assert!(body["error"]["message"].is_string(), "{query} says why");
    }
    let harness = Harness::memory();
    let (status, body) = harness.get("/v1/stats/cache").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(text(&body["error"]["message"]).contains("disabled"));
}

#[tokio::test]
async fn the_cache_data_needs_a_key_where_keys_are_required_but_the_page_does_not() {
    let harness = Harness::with_stats_and(keyed_config, kompressor_server::router);
    let (status, _) = harness.get("/v1/stats/cache?format=json").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, page) = harness.get("/v1/stats/cache?format=html").await;
    assert_eq!(status, StatusCode::OK);
    assert!(text(&page).contains("x-kompressor-key"));
    let (status, report, _) = harness
        .get_with("/v1/stats/cache", &[("x-kompressor-key", "sk-alpha")])
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["tenant"], "team-a");
    let (status, _, _) = harness
        .get_with(
            "/v1/stats/cache?tenant=team-b",
            &[("x-kompressor-key", "sk-alpha")],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_statistics_listener_serves_the_cache_page_beside_the_dashboard() {
    let harness = Harness::with_stats_and(|_| {}, kompressor_server::stats_listener_router);
    let (status, page) = harness.get("/cache?window=24h").await;
    assert_eq!(status, StatusCode::OK);
    let page = text(&page);
    assert!(page.contains("Kompressor cache"));
    assert!(
        page.contains("\"endpoint\":\"/v1/stats/cache\""),
        "the page at /cache must poll the JSON path, not itself"
    );
    assert!(page.contains("\"window\":\"24h\""));
    assert!(
        page.contains("\"refs\":false"),
        "the ref browser is off here"
    );
    let (status, report) = harness.get("/v1/stats/cache").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["rows"], 0);
    let (_, dashboard) = harness.get("/").await;
    assert!(
        text(&dashboard).contains("cache-link"),
        "the dashboard links it"
    );
}

// Historical analysis must preserve credits/debits without multiplying them
// when multiple events share a request, and must never join another tenant.
#[tokio::test]
async fn analysis_joins_events_and_preserves_fixed_window_accounting() {
    use kompressor_engine::types::Mode;
    use kompressor_stats::{Event, OptimizationEvent, RetrievalEvent, Surface};
    let harness = Harness::with_stats();
    let t = now_ms() - 60_000;
    let req = format!("req-{t}-false");
    harness.sink.send(proxied_turn(t, "", 1000, 2000, false));
    harness.sink.send(proxied_turn(t + 1, "", 3000, 0, true));
    for (tenant, outcome, saved) in [
        ("", "compressed", 600),
        ("", "compressed", 100),
        ("", "discarded", 900),
        ("other", "compressed", 9999),
    ] {
        let mut e = OptimizationEvent::new(t, tenant, &req, Surface::Proxy, Mode::Recoverable);
        e.tokens_in = saved + 100;
        e.tokens_out = 100;
        e.messages_total = 2;
        e.messages_skipped = 1;
        e.outcome = Some(outcome.into());
        e.skipped_by_reason.insert("frozen".into(), 1);
        e.skipped_tokens_by_reason.insert("frozen".into(), 500);
        harness.sink.send(Event::Optimization(e));
    }
    for (request, found, withheld, returned) in [
        (Some(req.clone()), true, false, 800),
        (Some(req.clone()), true, false, 200),
        (Some(req.clone()), false, true, 999),
        (None, true, false, 50),
    ] {
        harness.sink.send(Event::Retrieval(RetrievalEvent {
            context: None,
            time_ms: t,
            tenant: "".into(),
            request_id: request,
            surface: Surface::Proxy,
            reference: "k_AbC123dEf456".into(),
            found,
            expired: false,
            ranged: false,
            fragment_bytes: 4000,
            fragment_chars: 4000,
            fragment_tokens: 1000,
            returned_chars: returned * 4,
            returned_tokens: returned as u32,
            withheld,
            range_status: None,
        }));
    }
    // Exclusive end: this row must not enter this report.
    harness.sink.send(proxied_turn(t + 2, "", 0, 9000, false));
    harness.sink.flush().unwrap();
    let route = format!(
        "/v1/stats/analysis?from={t}&to={}&conversation=0123456789abcdef",
        t + 2
    );
    let (status, data) = harness.get(&route).await;
    assert_eq!(status, StatusCode::OK, "{data}");
    let r = &data["report"];
    let c = &r["conversations"][0];
    assert_eq!(r["conversations"].as_array().unwrap().len(), 1);
    assert_eq!(c["client_turns"], 1);
    assert_eq!(c["pings"], 1);
    assert_eq!(c["saved_gross"], 700);
    assert_eq!(c["retrieved_tokens"], 1000);
    assert_eq!(c["saved_net"], -300);
    assert_eq!(c["latency_samples"], 1);
    assert_eq!(r["unattributed"]["retrieved_tokens"], 50);
    assert_eq!(r["skips"][0]["tokens"], 1500);
    assert_eq!(r["turns_total"], 2);
    assert_eq!(r["turns"][0]["references"][0], "k_AbC123dEf456");
    assert_eq!(r["turns"][0]["cache"]["kind"], "first");
    assert_eq!(r["usage"]["requests"], 2);
    assert_eq!(r["ping_usage"]["requests"], 1);
    assert_eq!(data["stats_dropped"], 0);
    let (status, cache) = harness
        .get(&format!(
            "/v1/stats/cache?from={t}&to={}&conversation=0123456789abcdef",
            t + 2
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cache["from_ms"], t);
    assert_eq!(cache["to_ms"], t + 2);
    assert_eq!(cache["totals"]["requests"], 2);
}

#[tokio::test]
async fn analysis_routes_validate_scope_dates_and_formats() {
    let harness = Harness::with_stats_and(keyed_config, kompressor_server::stats_listener_router);
    for path in [
        "/v1/stats/analysis?format=json",
        "/v2/stats/analysis?format=json",
    ] {
        assert_eq!(harness.get(path).await.0, StatusCode::UNAUTHORIZED);
        let (status, body, _) = harness
            .get_with(path, &[("x-kompressor-key", "sk-alpha")])
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["report"]["tenant"], "team-a");
        assert_eq!(body["report"]["conversations"], json!([]));
    }
    let (status, _, _) = harness
        .get_with(
            "/v1/stats/analysis?tenant=team-b",
            &[("x-kompressor-key", "sk-alpha")],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    for query in [
        "from=20&to=10",
        "from=-1&to=10",
        "from=0&to=999999999999",
        "conversation=bad",
        "format=csv",
        "unexpected=1",
    ] {
        assert_eq!(
            harness.get(&format!("/v1/stats/analysis?{query}")).await.0,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{query}"
        );
    }
    for path in [
        "/analysis",
        "/v1/stats/analysis?format=html",
        "/v2/stats/analysis?format=html",
    ] {
        let (status, page) = harness.get(path).await;
        assert_eq!(status, StatusCode::OK);
        assert!(text(&page).contains("Conversation efficiency"));
        assert!(text(&page).contains("x-kompressor-key"));
    }
    for query in ["from=0", "from=10&to=1", "from=0&to=999999999999"] {
        assert_eq!(
            harness.get(&format!("/v1/stats/cache?{query}")).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
}

#[tokio::test]
async fn analysis_turn_limit_keeps_the_latest_turns_in_time_order() {
    let harness = Harness::with_stats();
    let t = now_ms() - 10_000;
    for i in 0..2002 {
        harness.sink.send(proxied_turn(t + i, "", 3000, 0, false));
    }
    harness.sink.flush().unwrap();
    let (status, body) = harness
        .get(&format!(
            "/v1/stats/analysis?from={t}&to={}&conversation=0123456789abcdef",
            t + 2002
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let r = &body["report"];
    assert_eq!(r["turns_total"], 2002);
    assert_eq!(r["turns_truncated"], true);
    assert_eq!(r["turns"].as_array().unwrap().len(), 2000);
    assert_eq!(r["turns"][0]["time_ms"], t + 2);
    assert_eq!(r["turns"][1999]["time_ms"], t + 2001);
    assert_eq!(r["conversations"][0]["client_turns"], 2002);
}

#[tokio::test]
async fn analysis_diagnostics_require_keys_and_scope_every_new_report() {
    use kompressor_stats::context::{ProxyTiming, RunContext};
    let harness = Harness::with_stats_and(keyed_config, kompressor_server::stats_listener_router);
    let t = now_ms() - 120_000;
    for tenant in ["team-a", "team-b"] {
        let mut e = proxied_turn(t, tenant, 1000, 100, false);
        if let kompressor_stats::Event::ProxyRequest(p) = &mut e {
            p.context = Some(RunContext {
                build_version: "2.0.5".into(),
                config_fingerprint: "cfg-a".into(),
                experiment_id: Some("experiment-a".into()),
                run_id: "run-a".into(),
                settings: json!({"engine":{"mode":"recoverable"},"proxy":{"compress":true}}),
            });
            p.timing = Some(ProxyTiming::default());
        }
        harness.sink.send(e);
    }
    harness.sink.flush().unwrap();
    for suffix in ["retrievals", "latency", "compare"] {
        for version in ["v1", "v2"] {
            let path = format!(
                "/{version}/stats/analysis/{suffix}?from={t}&to={}&format=json",
                t + 1
            );
            assert_eq!(harness.get(&path).await.0, StatusCode::UNAUTHORIZED);
            let (status, data, _) = harness
                .get_with(&path, &[("x-kompressor-key", "sk-alpha")])
                .await;
            assert_eq!(status, StatusCode::OK, "{data}");
            assert_eq!(data["tenant"], "team-a");
            if suffix == "latency" {
                assert_eq!(data["requests"], 1);
            }
            if suffix == "compare" {
                assert_eq!(data["current"]["metrics"]["client_requests"], 1);
                assert_eq!(data["current"]["rows"][0]["dimension"]["run_id"], "run-a");
            }
            let (status, _, _) = harness
                .get_with(
                    &format!("{path}&tenant=team-b"),
                    &[("x-kompressor-key", "sk-alpha")],
                )
                .await;
            assert_eq!(status, StatusCode::FORBIDDEN);
        }
    }
    let (status, data, _) = harness
        .get_with(
            &format!(
                "/v1/stats/analysis/compare?from={t}&to={}&experiment_id=missing",
                t + 1
            ),
            &[("x-kompressor-key", "sk-alpha")],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data["current"]["metrics"]["client_requests"], 0);
    for path in [
        "/v1/stats/analysis/compare?compare_from=10",
        "/v1/stats/analysis/compare?compare_from=20&compare_to=10",
        "/v1/stats/analysis/compare?mode=invalid",
        "/v1/stats/analysis/latency?model=ignored",
        "/v1/stats/analysis/retrievals?conversation=0123456789abcdef",
        "/v1/stats/analysis?compare_from=1&compare_to=2",
    ] {
        assert_eq!(
            harness.get(path).await.0,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{path}"
        );
    }
}

/// A GET whose body is kept as text, with the headers a download is judged by.
async fn get_raw(
    harness: &Harness,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, String, String, String) {
    let mut request = Request::builder().method("GET").uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = harness
        .router
        .clone()
        .oneshot(request.body(Body::empty()).expect("a request"))
        .await
        .expect("the router answers");
    let status = response.status();
    let header = |name| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    let (content_type, disposition) = (header("content-type"), header("content-disposition"));
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    (
        status,
        content_type,
        disposition,
        String::from_utf8(bytes.to_vec()).expect("utf-8"),
    )
}

fn jsonl_lines(body: &str) -> Vec<Value> {
    assert!(body.ends_with('\n'), "every line is terminated");
    body.lines()
        .map(|line| serde_json::from_str(line).expect("every line is one JSON value"))
        .collect()
}

#[tokio::test]
async fn every_dashboard_downloads_its_report_as_json_lines() {
    let harness = Harness::with_stats();
    let (status, _) = harness
        .post(
            "/v1/compress/raw",
            json!({"content": log_payload(400), "request_id": "export-1"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, empty) = harness.get("/v1/stats/cache").await;
    let tenant = text(&empty["tenant"]);
    let t = now_ms() - 60_000;
    harness
        .sink
        .send(proxied_turn(t, &tenant, 0, 40_000, false));
    harness
        .sink
        .send(proxied_turn(t + 1_000, &tenant, 40_000, 0, false));
    harness.sink.flush().expect("the batch commits");

    let from = t - 1;
    let to = now_ms() + 1;
    for (path, name, list) in [
        (
            "/v1/stats/live?format=jsonl&window=15m".to_string(),
            "live",
            "recent",
        ),
        (
            "/v1/stats/cache?format=jsonl&window=2h".to_string(),
            "cache",
            "conversations",
        ),
        (
            format!("/v1/stats/analysis?format=jsonl&from={from}&to={to}"),
            "analysis",
            "report.conversations",
        ),
        (
            format!("/v1/stats/analysis/latency?format=jsonl&from={from}&to={to}"),
            "analysis-latency",
            "",
        ),
        (
            format!("/v2/stats/analysis/retrievals?format=jsonl&from={from}&to={to}"),
            "analysis-retrievals",
            "",
        ),
    ] {
        let (status, content_type, disposition, body) = get_raw(&harness, &path, &[]).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert_eq!(content_type, "application/x-ndjson", "{path}");
        assert!(
            disposition.starts_with(&format!("attachment; filename=\"kompressor-{name}-"))
                && disposition.ends_with(".jsonl\""),
            "{path}: {disposition}"
        );
        let lines = jsonl_lines(&body);
        assert_eq!(lines[0]["record"], "summary", "{path}: the summary leads");
        assert!(
            lines
                .iter()
                .skip(1)
                .all(|line| line["record"].is_string() && line["record"] != "summary"),
            "{path}: every other line names its list"
        );
        if !list.is_empty() {
            assert!(
                lines.iter().any(|line| line["record"] == list),
                "{path}: has {list} rows"
            );
        }
    }

    // The rows match what the JSON form reports.
    let (_, live) = harness.get("/v1/stats/live?window=15m").await;
    let (_, _, _, body) = get_raw(&harness, "/v1/stats/live?format=jsonl&window=15m", &[]).await;
    let lines = jsonl_lines(&body);
    assert_eq!(lines[0]["totals"], live["totals"]);
    assert_eq!(lines[0]["server"]["version"], env!("CARGO_PKG_VERSION"));
    let recent: Vec<_> = lines.iter().filter(|l| l["record"] == "recent").collect();
    assert_eq!(recent.len(), live["recent"].as_array().unwrap().len());
    assert_eq!(recent[0]["request_id"], "export-1");

    let (_, cache) = harness.get("/v1/stats/cache?window=2h").await;
    let (_, _, _, body) = get_raw(&harness, "/v1/stats/cache?format=jsonl&window=2h", &[]).await;
    let lines = jsonl_lines(&body);
    assert_eq!(lines[0]["rows"], 2);
    assert_eq!(lines[0]["totals"], cache["totals"]);
    let timeline = lines.iter().filter(|l| l["record"] == "timeline").count();
    assert_eq!(timeline, cache["timeline"].as_array().unwrap().len());

    // An unknown format is still refused.
    let (status, _) = harness.get("/v1/stats/cache?format=csv").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn a_json_lines_download_needs_the_same_key_as_the_json() {
    let harness = Harness::with_stats_and(keyed_config, kompressor_server::stats_listener_router);
    for path in [
        "/v1/stats/live?format=jsonl",
        "/v1/stats/cache?format=jsonl",
        "/v1/stats/analysis?format=jsonl",
    ] {
        let (status, _, _, _) = get_raw(&harness, path, &[]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
        let (status, content_type, _, body) =
            get_raw(&harness, path, &[("x-kompressor-key", "sk-alpha")]).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert_eq!(content_type, "application/x-ndjson");
        assert!(body.contains("\"team-a\""), "{path}: the caller's tenant");
    }
    let (status, _, _, _) = get_raw(
        &harness,
        "/v1/stats/cache?format=jsonl&tenant=team-b",
        &[("x-kompressor-key", "sk-alpha")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// The prefix view over HTTP: a loss the proxy caused is reported as its own,
/// under both version prefixes, as JSON lines, and as a page.
#[tokio::test]
async fn the_prefix_dashboard_says_which_segment_moved_and_who_moved_it() {
    use kompressor_stats::context::{DRIFT_TOOL_SET, PrefixObservation};

    let harness = Harness::with_stats();
    let (_, empty) = harness.get("/v1/stats/cache").await;
    let tenant = text(&empty["tenant"]);
    let t = now_ms() - 60_000;
    let observed = |time_ms, read, write, seen: PrefixObservation| match proxied_turn(
        time_ms, &tenant, read, write, false,
    ) {
        kompressor_stats::Event::ProxyRequest(mut event) => {
            event.prefix = Some(seen);
            kompressor_stats::Event::ProxyRequest(event)
        }
        other => other,
    };
    let sized = PrefixObservation {
        body_bytes: 1_000,
        tools_bytes: 600,
        tools_count: 20,
        ..PrefixObservation::default()
    };
    let stable = PrefixObservation {
        compared: true,
        ..sized.clone()
    };
    let moved = PrefixObservation {
        drift_out: DRIFT_TOOL_SET,
        ..stable.clone()
    };
    // Written, read back, then not read back two seconds later.
    harness.sink.send(observed(t, 0, 40_000, sized));
    harness.sink.send(observed(t + 1_000, 40_000, 0, stable));
    harness.sink.send(observed(t + 2_000, 0, 40_000, moved));
    harness.sink.flush().expect("the batch commits");

    for version in ["v1", "v2"] {
        let (status, report) = harness
            .get(&format!("/{version}/stats/prefix?window=1h"))
            .await;
        assert_eq!(status, StatusCode::OK, "{report}");
        assert_eq!(report["available"], true);
        assert_eq!(
            (&report["observed"], &report["compared"], &report["stable"]),
            (&json!(3), &json!(2), &json!(1))
        );
        assert_eq!(report["losses"]["total"]["events"], 1, "{report}");
        assert_eq!(report["losses"]["proxy"]["lost_tokens"], 40_000);
        assert_eq!(report["losses"]["client"]["events"], 0);
        let tool_set = report["drift"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["segment"] == "tool_set")
            .unwrap_or_else(|| panic!("no tool_set row in {report}"));
        assert_eq!(
            (&tool_set["proxy_turns"], &tool_set["proxy_losses"]),
            (&json!(1), &json!(1))
        );
        let model = &report["composition"][0];
        assert_eq!(model["tools_share"], 0.6);
        assert_eq!(model["undeferred_requests"], 3);
        // What the page's columns read: tool search beside deferral, and a
        // predicted lapse broken out by what became of the prefix.
        assert_eq!(model["tool_search_requests"], 0);
        for score in report["cold"].as_array().unwrap() {
            for outcome in [
                "confirmed",
                "wrong",
                "partial",
                "drift",
                "unknown",
                "missed",
            ] {
                assert!(score[outcome].is_u64(), "{outcome} in {score}");
            }
        }
    }

    let path = "/v1/stats/prefix?format=jsonl&window=1h";
    let (status, content_type, _, body) = get_raw(&harness, path, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(content_type, "application/x-ndjson");
    let lines = jsonl_lines(&body);
    assert_eq!(lines[0]["record"], "summary");
    assert!(lines.iter().any(|line| line["record"] == "drift"), "{body}");

    let (status, page) = harness.get("/v1/stats/prefix?format=html").await;
    assert_eq!(status, StatusCode::OK);
    let page = text(&page);
    assert!(page.contains("\"view\":\"prefix\""));
    assert!(page.contains("RENDER.prefix = ") && page.contains("api: \"stats/prefix\""));

    let (status, _) = harness.get("/v1/stats/prefix?nonsense=1").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// The advisor over HTTP: a row per opt-in setting, each marked with whether
/// this deployment has it on, under both version prefixes and as a page.
#[tokio::test]
async fn the_advisor_gives_a_verdict_per_setting_and_says_what_is_on() {
    use kompressor_stats::context::PrefixObservation;

    let harness = Harness::with_stats();
    let (_, empty) = harness.get("/v1/stats/cache").await;
    let tenant = text(&empty["tenant"]);
    let t = now_ms() - 3_000_000;
    // A hundred and twenty turns of one conversation, each read back in full,
    // carrying tool definitions a twentieth of which is schema bookkeeping.
    for turn in 0..120u32 {
        let seen = PrefixObservation {
            compared: turn > 0,
            body_bytes: 10_000,
            tools_bytes: 4_000,
            tools_count: 12,
            tools_compactable_bytes: 200,
            ..PrefixObservation::default()
        };
        let read = if turn == 0 {
            0
        } else {
            40_000 + 10 * (turn - 1)
        };
        let write = if turn == 0 { 40_000 } else { 10 };
        let event = match proxied_turn(t + i64::from(turn) * 1_000, &tenant, read, write, false) {
            kompressor_stats::Event::ProxyRequest(mut event) => {
                event.prefix = Some(seen);
                kompressor_stats::Event::ProxyRequest(event)
            }
            other => other,
        };
        harness.sink.send(event);
    }
    harness.sink.flush().expect("the batch commits");

    for version in ["v1", "v2"] {
        let (status, report) = harness
            .get(&format!("/{version}/stats/advisor?window=1h"))
            .await;
        assert_eq!(status, StatusCode::OK, "{report}");
        assert_eq!(report["available"], true);
        assert_eq!(report["compared"], 119);
        let advice = report["advice"].as_array().unwrap();
        assert_eq!(advice.len(), 8);
        let of = |setting: &str| {
            advice
                .iter()
                .find(|row| row["setting"] == setting)
                .unwrap_or_else(|| panic!("no {setting} in {report}"))
        };
        assert_eq!(of("tools_compact")["verdict"], "turn_on");
        assert_eq!(of("tools_compact")["value"], "true");
        assert_eq!(of("tools_compact")["measure"], 0.05);
        assert_eq!(of("tools_order")["verdict"], "not_worth_it");
        assert_eq!(of("cold_recompact")["verdict"], "not_worth_it");
        assert_eq!(of("prompt_cache_key")["verdict"], "not_enough_data");
        // Nothing is on in the default deployment, and it says so rather
        // than leaving the column blank.
        for row in advice {
            assert_eq!(row["enabled"], false, "{}", row["setting"]);
        }
        assert_eq!(of("tool deferral")["built"], false);
        assert_eq!(report["effects"]["recompactions_observed"], 0);
    }

    let path = "/v1/stats/advisor?format=jsonl&window=1h";
    let (status, content_type, _, body) = get_raw(&harness, path, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(content_type, "application/x-ndjson");
    assert!(
        jsonl_lines(&body)
            .iter()
            .any(|line| line["record"] == "advice"),
        "{body}"
    );

    let (status, page) = harness.get("/v1/stats/advisor?format=html").await;
    assert_eq!(status, StatusCode::OK);
    let page = text(&page);
    assert!(page.contains("\"view\":\"advisor\""));
    assert!(page.contains("RENDER.advisor = ") && page.contains("api: \"stats/advisor\""));
}

/// The three scorecard views — models, activity, cohorts — over HTTP: the
/// JSON under both version prefixes, the JSON-lines download, the page shell,
/// and the pages on the statistics listener.
#[tokio::test]
async fn the_scorecard_dashboards_answer_with_what_was_recorded() {
    let harness = Harness::with_stats();
    let (status, _) = harness
        .post(
            "/v1/compress/raw",
            json!({"content": log_payload(400), "request_id": "score-1"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, empty) = harness.get("/v1/stats/cache").await;
    let tenant = text(&empty["tenant"]);
    let t = now_ms() - 60_000;
    harness
        .sink
        .send(proxied_turn(t, &tenant, 0, 40_000, false));
    harness
        .sink
        .send(proxied_turn(t + 1_000, &tenant, 40_000, 0, false));
    harness
        .sink
        .send(proxied_turn(t + 2_000, &tenant, 40_000, 0, true));
    harness.sink.flush().expect("the batch commits");

    let (status, models) = harness.get("/v1/stats/models?window=1h").await;
    assert_eq!(status, StatusCode::OK, "{models}");
    assert_eq!(models["window"], "1h");
    let opus = models["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["model"] == "claude-opus-5-5")
        .unwrap_or_else(|| panic!("no opus card in {models}"));
    assert_eq!(
        (&opus["client_requests"], &opus["pings"]),
        (&json!(2), &json!(1))
    );
    assert_eq!(opus["billed"]["read"], 80_000);
    assert_eq!(opus["billed"]["write_5m"], 40_000);
    // The compression made through the API has no model: a card of its own.
    assert!(
        models["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["engine_runs"].as_u64() == Some(1) && m["saved_gross"].as_i64() > Some(0)),
        "{models}"
    );

    let (status, activity) = harness.get("/v2/stats/activity?window=1h").await;
    assert_eq!(status, StatusCode::OK, "{activity}");
    assert_eq!(activity["requests"], 2);
    let cells = activity["cells"].as_array().unwrap();
    assert!(!cells.is_empty() && cells.len() <= 2, "{activity}");
    assert_eq!(
        cells
            .iter()
            .map(|c| c["pings"].as_u64().unwrap())
            .sum::<u64>(),
        1
    );
    assert!(activity["busiest"]["weekday"].as_u64().unwrap() < 7);
    assert!(activity["peak_window_share"].as_f64().unwrap() > 0.99);

    let (status, cohorts) = harness.get("/v1/stats/cohorts?window=1h").await;
    assert_eq!(status, StatusCode::OK, "{cohorts}");
    assert_eq!(cohorts["conversations"], 1);
    let rows = cohorts["cohorts"].as_array().unwrap();
    assert_eq!(rows.len(), 6, "every cohort is listed, empty or not");
    let two_to_four = rows.iter().find(|r| r["turns"] == "2–4").unwrap();
    assert_eq!(
        (
            &two_to_four["conversations"],
            &two_to_four["client_turns"],
            &two_to_four["pings"]
        ),
        (&json!(1), &json!(2), &json!(1))
    );
    assert_eq!(two_to_four["cost_share"], 1.0);

    for name in ["models", "activity", "cohorts"] {
        let path = format!("/v1/stats/{name}?format=jsonl&window=1h");
        let (status, content_type, disposition, body) = get_raw(&harness, &path, &[]).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert_eq!(content_type, "application/x-ndjson", "{path}");
        assert!(
            disposition.starts_with(&format!("attachment; filename=\"kompressor-{name}-")),
            "{path}: {disposition}"
        );
        assert_eq!(jsonl_lines(&body)[0]["record"], "summary", "{path}");

        let (status, page) = harness.get(&format!("/v1/stats/{name}?format=html")).await;
        assert_eq!(status, StatusCode::OK, "{name}");
        let page = text(&page);
        assert!(
            page.contains(&format!("\"view\":\"{name}\"")),
            "{name}: the shell names its view"
        );
        assert!(
            page.contains(&format!("RENDER.{name} = ")),
            "{name}: the page can draw the view it was opened on"
        );
        assert!(
            page.contains(&format!("api: \"stats/{name}\"")),
            "{name}: and links to it"
        );

        // A parameter no view knows is refused, as on every other dashboard.
        let (status, _) = harness.get(&format!("/v1/stats/{name}?nonsense=1")).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name}");
    }

    let listener = Harness::with_stats_and(|_| {}, kompressor_server::stats_listener_router);
    for name in ["models", "activity", "cohorts"] {
        let (status, page) = listener.get(&format!("/{name}")).await;
        assert_eq!(status, StatusCode::OK, "/{name}");
        assert!(
            text(&page).contains(&format!("\"view\":\"{name}\"")),
            "/{name}"
        );
        let (status, body) = listener.get(&format!("/v1/stats/{name}")).await;
        assert_eq!(status, StatusCode::OK, "/v1/stats/{name}: {body}");
    }
}

/// The scorecards carry what every statistics route carries: a key where keys
/// are configured, and no way to name another tenant without being trusted.
#[tokio::test]
async fn the_scorecard_dashboards_need_a_key_and_keep_to_the_callers_tenant() {
    let harness = Harness::with_stats_and(keyed_config, kompressor_server::stats_listener_router);
    for name in ["models", "activity", "cohorts"] {
        let path = format!("/v1/stats/{name}");
        let (status, _, _, _) = get_raw(&harness, &path, &[]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
        let (status, _, _, body) =
            get_raw(&harness, &path, &[("x-kompressor-key", "sk-alpha")]).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        let (status, _, _, _) = get_raw(
            &harness,
            &format!("{path}?tenant=team-b"),
            &[("x-kompressor-key", "sk-alpha")],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        // The page shell itself is served to anyone; the data is not.
        let (status, _, _, _) = get_raw(&harness, &format!("/{name}"), &[]).await;
        assert_eq!(status, StatusCode::OK, "/{name}");
    }
}

#[tokio::test]
async fn every_dashboard_page_offers_the_download() {
    let harness = Harness::with_stats();
    for path in [
        "/v1/stats/live?format=html",
        "/v1/stats/cache?format=html",
        "/v1/stats/analysis?format=html",
    ] {
        let (status, page) = harness.get(path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        let page = text(&page);
        assert!(page.contains("id=\"download\""), "{path}: has the button");
        assert!(page.contains("\"jsonl\""), "{path}: asks for jsonl");
    }
}

// ---------------------------------------------------------------------------
// the statistics JSON API, as automation uses it
// ---------------------------------------------------------------------------

/// Every read-only statistics route that answers JSON, with a query that is
/// valid on each. `{from}`/`{to}` are filled in by [`stats_routes`].
const STATS_ROUTES: [&str; 12] = [
    "stats/summary",
    "stats/histogram?bucket=1h",
    "stats/by_type",
    "stats/retrievals",
    "stats/requests?request_id=auto-1",
    "stats/store",
    "stats/live?window=15m",
    "stats/cache?window=2h",
    "stats/analysis?from={from}&to={to}",
    "stats/analysis/retrievals?from={from}&to={to}",
    "stats/analysis/latency?from={from}&to={to}",
    "stats/analysis/compare?from={from}&to={to}",
];

fn stats_routes(version: &str, from: i64, to: i64) -> Vec<String> {
    STATS_ROUTES
        .iter()
        .map(|route| {
            format!("/{version}/")
                + &route
                    .replace("{from}", &from.to_string())
                    .replace("{to}", &to.to_string())
        })
        .collect()
}

/// A server holding one compression, one retrieval and two proxied turns for
/// `tenant`, all inside the returned `(from, to)`.
async fn seeded(harness: &Harness, tenant: &str, headers: &[(&str, &str)]) -> (i64, i64) {
    let (status, compressed, _) = harness
        .post_with(
            "/v1/compress",
            json!({
                "messages": [{"role": "user", "content": log_payload(400)}],
                "request_id": "auto-1"
            }),
            headers,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{compressed}");
    let handle = handle_in(&compressed["messages"]);
    let (status, _, _) = harness
        .post_with(
            "/v1/retrieve",
            json!({ "ref": handle, "request_id": "auto-1" }),
            headers,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let t = now_ms() - 60_000;
    harness.sink.send(proxied_turn(t, tenant, 0, 40_000, false));
    harness
        .sink
        .send(proxied_turn(t + 1_000, tenant, 40_000, 2_000, false));
    harness.sink.flush().expect("the batch commits");
    (t - 1, now_ms() + 1)
}

fn keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap_or_else(|| panic!("an object, not {value}"))
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

fn sorted(mut keys: Vec<&str>) -> Vec<&str> {
    keys.sort_unstable();
    keys
}

fn has_keys(value: &Value, expected: &[&str], what: &str) {
    for key in expected {
        assert!(
            value.get(key).is_some(),
            "{what} is missing {key:?}: has {:?}",
            keys(value)
        );
    }
}

#[tokio::test]
async fn every_stats_route_answers_automation_with_json_whatever_it_accepts() {
    let harness = Harness::with_stats();
    let (from, to) = seeded(&harness, "", &[]).await;
    for version in ["v1", "v2"] {
        for path in stats_routes(version, from, to) {
            for accept in [None, Some("*/*"), Some("application/json")] {
                let headers: Vec<(&str, &str)> =
                    accept.map(|a| vec![("accept", a)]).unwrap_or_default();
                let (status, content_type, disposition, body) =
                    get_raw(&harness, &path, &headers).await;
                assert_eq!(status, StatusCode::OK, "{path} {accept:?}: {body}");
                assert_eq!(content_type, "application/json", "{path} {accept:?}");
                assert!(disposition.is_empty(), "{path}: JSON is not a download");
                let value: Value = serde_json::from_str(&body).expect("JSON");
                assert!(value.is_object(), "{path}: one object");
            }
        }
    }
    // And `?format=json` wins over a browser's Accept on every dashboard route.
    let browser = [("accept", "text/html,application/xhtml+xml,*/*;q=0.8")];
    for path in &stats_routes("v1", from, to)[6..] {
        let sep = if path.contains('?') { '&' } else { '?' };
        let (status, content_type, _, body) =
            get_raw(&harness, &format!("{path}{sep}format=json"), &browser).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert_eq!(content_type, "application/json", "{path}");
    }
}

#[tokio::test]
async fn the_live_json_keeps_its_contract() {
    let harness = Harness::with_stats();
    seeded(&harness, "", &[]).await;
    let (status, live) = harness.get("/v1/stats/live?window=15m&recent=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        keys(&live),
        sorted(vec![
            "generated_at_ms",
            "server",
            "window",
            "tenant",
            "from_ms",
            "to_ms",
            "bucket_ms",
            "totals",
            "rates",
            "timeline",
            "upstream",
            "by_provider",
            "by_model",
            "by_type",
            "skipped_by_reason",
            "retrievals",
            "recent",
        ])
    );
    assert_eq!(
        keys(&live["server"]),
        sorted(vec![
            "version",
            "started_at_ms",
            "uptime_s",
            "mode",
            "proxy_enabled",
            "stats_dropped",
            "inflight_limit",
            "inflight_free",
        ])
    );
    assert_eq!(
        keys(&live["window"]),
        sorted(vec!["label", "from_ms", "to_ms", "bucket_ms"])
    );
    let window = &live["window"];
    assert_eq!(window["label"], "15m");
    assert_eq!(
        window["to_ms"].as_i64().unwrap() - window["from_ms"].as_i64().unwrap(),
        15 * 60_000
    );
    assert_eq!(window["bucket_ms"], live["bucket_ms"]);
    assert!(live["generated_at_ms"].as_i64().unwrap() >= window["to_ms"].as_i64().unwrap());
    assert_eq!(
        live["recent"].as_array().unwrap().len(),
        1,
        "recent=1 is honoured"
    );
    assert_eq!(live["totals"]["compressions"], 1);
    for list in [
        "rates",
        "timeline",
        "by_provider",
        "by_model",
        "by_type",
        "skipped_by_reason",
    ] {
        assert!(live[list].is_array(), "{list} is a list");
    }
    let (_, none) = harness.get("/v1/stats/live?recent=0").await;
    assert_eq!(none["recent"], json!([]));
    assert_eq!(none["window"]["label"], "1h", "the default window");
}

#[tokio::test]
async fn the_cache_json_keeps_its_contract() {
    let harness = Harness::with_stats();
    let (_, empty) = harness.get("/v1/stats/cache").await;
    let tenant = text(&empty["tenant"]);
    let (from, to) = seeded(&harness, &tenant, &[]).await;
    let report_keys = [
        "tenant",
        "from_ms",
        "to_ms",
        "bucket_ms",
        "rows",
        "truncated",
        "totals",
        "clients",
        "pings",
        "losses",
        "keep_warm",
        "gaps",
        "timeline",
        "by_model",
        "conversations",
        "conversations_total",
        "unattributed",
        "turns",
    ];
    let (status, report) = harness.get("/v1/stats/cache?window=2h").await;
    assert_eq!(status, StatusCode::OK);
    let mut expected = vec!["generated_at_ms", "window"];
    expected.extend(report_keys);
    assert_eq!(keys(&report), sorted(expected));
    assert_eq!(report["window"], "2h");
    assert_eq!(report["rows"], 2);
    assert_eq!(report["truncated"], false);
    assert!(report["turns"].is_null(), "no conversation named");
    for list in ["gaps", "timeline", "by_model", "conversations"] {
        assert!(report[list].is_array(), "{list} is a list");
    }

    // A fixed range is reported as one, with exactly the bounds asked for.
    let (status, fixed) = harness
        .get(&format!(
            "/v1/stats/cache?from={from}&to={to}&conversation=0123456789abcdef&conversations=0"
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{fixed}");
    assert_eq!(fixed["window"], "fixed range");
    assert_eq!(fixed["from_ms"], from);
    assert_eq!(fixed["to_ms"], to);
    assert_eq!(
        fixed["conversations"],
        json!([]),
        "conversations=0 is honoured"
    );
    assert_eq!(fixed["conversations_total"], 1);
    assert_eq!(fixed["turns"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn the_analysis_json_keeps_its_contract() {
    let harness = Harness::with_stats();
    let (_, empty) = harness.get("/v1/stats/cache").await;
    let tenant = text(&empty["tenant"]);
    let (from, to) = seeded(&harness, &tenant, &[]).await;

    let (status, overview) = harness
        .get(&format!("/v1/stats/analysis?from={from}&to={to}"))
        .await;
    assert_eq!(status, StatusCode::OK, "{overview}");
    assert_eq!(
        keys(&overview),
        sorted(vec!["report", "stats_dropped", "retention_days"])
    );
    let report = &overview["report"];
    assert_eq!(
        keys(report),
        sorted(vec![
            "tenant",
            "from_ms",
            "to_ms",
            "conversations",
            "unattributed",
            "outcomes",
            "skips",
            "breakdowns",
            "turns",
            "turns_total",
            "turns_truncated",
            "cache_truncated",
            "usage",
            "ping_usage",
            "raw_available_from_ms",
        ])
    );
    assert_eq!(report["from_ms"], from);
    assert_eq!(report["to_ms"], to);
    assert!(
        report["turns"].is_null(),
        "turns only for a named conversation"
    );

    let scope = ["tenant", "from_ms", "to_ms", "stats_dropped"];
    let (status, retrievals) = harness
        .get(&format!(
            "/v1/stats/analysis/retrievals?from={from}&to={to}"
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{retrievals}");
    has_keys(&retrievals, &scope, "retrievals");
    has_keys(
        &retrievals,
        &[
            "attempts",
            "hits",
            "missing",
            "expired",
            "withheld",
            "ranged_tokens",
            "full_tokens",
            "saved_gross",
            "retrieved_tokens",
            "debit_to_gross",
            "requests",
            "requests_total",
            "repeated_refs",
            "repeated_refs_total",
            "rounds",
            "raw_only",
        ],
        "retrievals",
    );
    assert_eq!(retrievals["hits"], 1);
    assert_eq!(retrievals["from_ms"], from);

    let (status, latency) = harness
        .get(&format!("/v1/stats/analysis/latency?from={from}&to={to}"))
        .await;
    assert_eq!(status, StatusCode::OK, "{latency}");
    has_keys(&latency, &scope, "latency");
    has_keys(
        &latency,
        &[
            "requests",
            "rows_total",
            "truncated",
            "errors",
            "with_timing",
            "total",
            "groups",
            "slowest",
            "errors_over_time",
            "raw_only",
        ],
        "latency",
    );
    has_keys(
        &latency["total"],
        &["samples", "mean", "p50", "p95", "p99", "max"],
        "a distribution",
    );
    assert_eq!(latency["requests"], 2);

    let (status, compare) = harness
        .get(&format!("/v1/stats/analysis/compare?from={from}&to={to}"))
        .await;
    assert_eq!(status, StatusCode::OK, "{compare}");
    has_keys(&compare, &scope, "compare");
    has_keys(&compare, &["current", "baseline", "changes"], "compare");
    for side in ["current", "baseline"] {
        has_keys(
            &compare[side],
            &[
                "from_ms",
                "to_ms",
                "metrics",
                "conversations",
                "net_reduction",
            ],
            side,
        );
    }
    assert_eq!(compare["current"]["from_ms"], from);
    assert_eq!(compare["baseline"]["to_ms"], from, "the window just before");
    assert_eq!(
        compare["baseline"]["from_ms"].as_i64().unwrap(),
        from - (to - from)
    );
    assert!(compare["changes"].is_object());
}

#[tokio::test]
async fn the_reports_agree_with_one_another_over_the_same_range() {
    let harness = Harness::with_stats();
    let (_, empty) = harness.get("/v1/stats/cache").await;
    let tenant = text(&empty["tenant"]);
    let (from, to) = seeded(&harness, &tenant, &[]).await;
    let (_, cache) = harness
        .get(&format!("/v1/stats/cache?from={from}&to={to}"))
        .await;
    let (_, analysis) = harness
        .get(&format!("/v1/stats/analysis?from={from}&to={to}"))
        .await;
    let (_, live) = harness.get("/v1/stats/live?window=15m").await;
    let (_, summary) = harness.get("/v1/stats/summary").await;
    assert_eq!(cache["totals"]["requests"], 2);
    assert_eq!(
        analysis["report"]["usage"]["requests"],
        cache["totals"]["requests"]
    );
    assert_eq!(live["totals"]["compressions"], summary["requests"]);
    assert_eq!(live["tenant"], cache["tenant"]);
    assert_eq!(analysis["report"]["tenant"], cache["tenant"]);
}

#[tokio::test]
async fn every_stats_route_applies_the_same_credentials() {
    let harness = Harness::with_stats_and(keyed_config, kompressor_server::router);
    let alpha = [("x-kompressor-key", "sk-alpha")];
    let (from, to) = seeded(&harness, "team-a", &alpha).await;
    // Something for team-b too, so a cross-tenant read has rows to leak.
    harness
        .sink
        .send(proxied_turn(from + 2, "team-b", 0, 9_000, false));
    harness.sink.flush().unwrap();

    for path in stats_routes("v1", from, to) {
        let sep = if path.contains('?') { '&' } else { '?' };
        let (status, body) = harness.get(&path).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}: no key");
        assert_eq!(body["error"]["code"], "unauthorized", "{path}");

        let (status, _, _) = harness
            .get_with(&path, &[("x-kompressor-key", "sk-nobody")])
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}: unknown key");

        let (status, _, _) = harness
            .get_with(&path, &[("x-kompressor-key", "sk-retired")])
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: disabled key");

        let (status, body, _) = harness.get_with(&path, &alpha).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");

        let (status, body, _) = harness
            .get_with(&format!("{path}{sep}tenant=team-a"), &alpha)
            .await;
        assert_eq!(status, StatusCode::OK, "{path}: naming its own tenant");
        if let Some(tenant) = body
            .get("tenant")
            .or_else(|| body.get("report").and_then(|r| r.get("tenant")))
        {
            assert_eq!(tenant, "team-a", "{path}");
        }

        let (status, body, _) = harness
            .get_with(&format!("{path}{sep}tenant=team-b"), &alpha)
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: another tenant");
        assert_eq!(body["error"]["code"], "forbidden");

        let (status, _, _) = harness
            .get_with(
                &format!("{path}{sep}tenant=team-b"),
                &[("x-kompressor-secret", "wrong")],
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}: wrong secret");
    }

    // The cluster secret may read any tenant: the trusted path automation
    // inside the deployment uses.
    let trusted = [("x-kompressor-secret", "cluster-secret")];
    for path in [
        "/v1/stats/live?tenant=team-b".to_string(),
        "/v1/stats/cache?window=2h&tenant=team-b".to_string(),
        format!("/v1/stats/analysis?from={from}&to={to}&tenant=team-b"),
        format!("/v1/stats/analysis/latency?from={from}&to={to}&tenant=team-b"),
    ] {
        let (status, body, _) = harness.get_with(&path, &trusted).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        let tenant = body
            .get("tenant")
            .or_else(|| body["report"].get("tenant"))
            .unwrap();
        assert_eq!(tenant, "team-b", "{path}");
    }
    let (_, cache, _) = harness
        .get_with("/v1/stats/cache?window=2h&tenant=team-b", &trusted)
        .await;
    assert_eq!(cache["rows"], 1, "only team-b's row");
    let (_, cache, _) = harness.get_with("/v1/stats/cache?window=2h", &alpha).await;
    assert_eq!(cache["rows"], 2, "only team-a's rows");
}

#[tokio::test]
async fn every_stats_route_says_so_when_statistics_are_off() {
    let harness = Harness::memory();
    for path in stats_routes("v1", 1, 2) {
        if path.starts_with("/v1/stats/store") {
            // The ref store's own figures need no statistics database.
            let (status, store) = harness.get(&path).await;
            assert_eq!(status, StatusCode::OK);
            assert!(store["live_refs"].is_u64());
            continue;
        }
        let (status, body) = harness.get(&path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert!(
            text(&body["error"]["message"]).contains("disabled"),
            "{path}: {body}"
        );
    }
}

#[tokio::test]
async fn every_stats_route_refuses_what_it_cannot_honour_in_the_error_envelope() {
    let harness = Harness::with_stats();
    for path in stats_routes("v1", 10, 20) {
        let sep = if path.contains('?') { '&' } else { '?' };
        let (status, body) = harness.get(&format!("{path}{sep}nonsense=1")).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{path}");
        assert_eq!(body["error"]["code"], "invalid_request", "{path}");
        assert!(body["error"]["message"].is_string(), "{path} says why");
    }
    for path in [
        "/v1/stats/analysis/retrievals?format=html",
        "/v1/stats/analysis/latency?format=html",
        "/v1/stats/analysis/compare?format=html",
    ] {
        let (status, body) = harness.get(path).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{path}");
        assert!(text(&body["error"]["message"]).contains("diagnostics serve JSON"));
    }
    for path in [
        "/v1/stats/live?format=csv",
        "/v1/stats/cache?format=csv",
        "/v1/stats/analysis?format=csv",
    ] {
        let (status, body) = harness.get(path).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{path}");
        assert!(text(&body["error"]["message"]).contains("json, jsonl or html"));
    }
    // Statistics listener: the same JSON routes, the same answers.
    let listener = Harness::with_stats_and(|_| {}, kompressor_server::stats_listener_router);
    for path in [
        "/v1/stats/live",
        "/v1/stats/cache",
        "/v1/stats/analysis",
        "/v2/stats/analysis/latency",
    ] {
        let (status, content_type, _, body) = get_raw(&listener, path, &[]).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert_eq!(content_type, "application/json", "{path}");
    }
}
