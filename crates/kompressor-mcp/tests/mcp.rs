//! The MCP surface driven the way a client drives it: real JSON-RPC bodies over
//! the mounted router, and the stdio transport over an in-memory pipe.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use kompressor_core::auth::Identity;
use kompressor_core::config::{ApiKey, AuthConfig, AuthMode, Config};
use kompressor_core::{AppCore, build_ref_store};
use kompressor_mcp::mcp_router;
use kompressor_store::clock::ManualClock;
use kompressor_store::memory::MemoryRefStore;
use kompressor_store::types::Tenant;
use serde_json::{Value, json};
use tower::ServiceExt;

const HOST: &str = "kompressor.svc.cluster.local";
const KEY_A: &str = "sk-team-a";
const KEY_B: &str = "sk-team-b";

/// A payload big and repetitive enough that the engine compresses it and mints
/// at least one handle.
fn log_payload(lines: usize) -> String {
    (0..lines)
        .map(|i| {
            format!(
                "2026-01-02T03:04:{:02}Z INFO worker heartbeat seq={i}",
                i % 60
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn keyed_config() -> Config {
    let mut config = Config {
        auth: AuthConfig {
            mode: AuthMode::ApiKeys,
            secret: "cluster-secret".into(),
            keys: vec![
                ApiKey {
                    key: KEY_A.into(),
                    tenant: "team-a".into(),
                    enabled: true,
                    mode: None,
                },
                ApiKey {
                    key: KEY_B.into(),
                    tenant: "team-b".into(),
                    enabled: true,
                    mode: None,
                },
            ],
        },
        ..Default::default()
    };
    config.stats.enabled = false;
    config
}

fn core_with(config: Config, refs: Arc<dyn kompressor_store::types::RefStore>) -> Arc<AppCore> {
    Arc::new(AppCore::new(
        config,
        refs,
        kompressor_stats::writer::null_sink(),
    ))
}

fn keyed_core() -> Arc<AppCore> {
    core_with(keyed_config(), Arc::new(MemoryRefStore::new()))
}

/// POST one JSON-RPC message and read the reply.
///
/// Accepts both reply framings the transport may choose, so a test asserts on
/// the protocol rather than on whether this exchange happened to be streamed.
async fn rpc(router: &Router, key: Option<&str>, host: &str, body: Value) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", host)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(key) = key {
        request = request.header("authorization", format!("Bearer {key}"));
    }
    let request = request.body(Body::from(body.to_string())).unwrap();

    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes).to_string();
    let payload = serde_json::from_str::<Value>(&text).unwrap_or_else(|_| {
        // Server-sent events: the JSON-RPC message is the last `data:` line.
        text.lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
            .next_back()
            .unwrap_or(Value::String(text.clone()))
    });
    (status, payload)
}

fn initialize() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0"}
        }
    })
}

fn call(id: i64, tool: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": tool, "arguments": arguments}
    })
}

/// The structured payload of a successful tool result.
fn structured(reply: &Value) -> &Value {
    reply
        .pointer("/result/structuredContent")
        .unwrap_or_else(|| panic!("expected a structured tool result, got {reply}"))
}

/// The text a tool result puts in front of the model.
fn text_of(reply: &Value) -> String {
    reply
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Compress a payload as `key` and return the first handle the markers carry.
async fn mint_handle(router: &Router, key: &str, id: i64) -> String {
    let (status, reply) = rpc(
        router,
        Some(key),
        HOST,
        call(
            id,
            "optscale_kompressor_compress",
            json!({"messages": [{"role": "user", "content": log_payload(400)}]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "compression failed: {reply}");
    let messages = structured(&reply)["messages"].clone();
    let body = messages[0]["content"]
        .as_str()
        .expect("text content")
        .to_string();
    body.split("ref:")
        .nth(1)
        .map(|rest| {
            rest.chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .collect::<String>()
        })
        .unwrap_or_else(|| panic!("a recoverable compression must mint a handle; got {body}"))
}

#[tokio::test]
async fn tools_list_advertises_all_four_tools_with_object_input_schemas() {
    let router = mcp_router(keyed_core());
    let (status, reply) = rpc(
        &router,
        Some(KEY_A),
        HOST,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");

    let tools = reply["result"]["tools"].as_array().expect("a tool list");
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    for expected in [
        "optscale_kompressor_retrieve",
        "optscale_kompressor_compress",
        "optscale_kompressor_compress_raw",
        "optscale_kompressor_stats",
    ] {
        assert!(names.contains(&expected), "{expected} is not advertised");
    }
    assert_eq!(names.len(), 4, "no tool beyond the documented four");

    for tool in tools {
        let schema = &tool["inputSchema"];
        assert_eq!(
            schema["type"], "object",
            "{} has no object input schema",
            tool["name"]
        );
        assert!(
            schema.get("properties").is_some(),
            "{} declares no properties",
            tool["name"]
        );
        let description = tool["description"].as_str().unwrap_or_default();
        assert!(
            description.len() > 40,
            "{} has no description worth reading",
            tool["name"]
        );
        assert!(
            description.contains("optscale_kompressor"),
            "{} does not name the product",
            tool["name"]
        );
    }

    // Retrieval returns structured content, and a client that was never told the
    // shape may show the model nothing at all.
    let retrieve = tools
        .iter()
        .find(|tool| tool["name"] == "optscale_kompressor_retrieve")
        .unwrap();
    assert_eq!(retrieve["outputSchema"]["type"], "object");
    for field in ["content", "found", "expired", "ranged", "truncated", "hint"] {
        assert!(
            retrieve["outputSchema"]["properties"].get(field).is_some(),
            "the output schema does not mention {field}"
        );
    }
}

#[tokio::test]
async fn the_surface_is_mounted_where_the_configuration_puts_it() {
    let mut config = keyed_config();
    config.mcp.path = "tools/mcp/".into();
    let router = mcp_router(core_with(config, Arc::new(MemoryRefStore::new())));

    let request = |uri: &str| {
        Request::builder()
            .method("POST")
            .uri(uri.to_string())
            .header("host", HOST)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("authorization", format!("Bearer {KEY_A}"))
            .body(Body::from(initialize().to_string()))
            .unwrap()
    };

    let response = router.clone().oneshot(request("/tools/mcp")).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a path written without a leading slash or with a trailing one still mounts"
    );
    let response = router.clone().oneshot(request("/mcp")).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_retrieve_description_teaches_the_marker_grammar_and_the_regenerate_rule() {
    // This text is the mechanism, not documentation about it: a model that has
    // not been told the marker shapes never calls the tool at all.
    let router = mcp_router(keyed_core());
    let (_, reply) = rpc(
        &router,
        Some(KEY_A),
        HOST,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await;
    let tools = reply["result"]["tools"].as_array().unwrap();
    let retrieve = tools
        .iter()
        .find(|tool| tool["name"] == "optscale_kompressor_retrieve")
        .expect("the retrieve tool");
    let description = retrieve["description"].as_str().unwrap();

    for shape in [
        "similar items omitted",
        "chars ref:k_",
        "\u{d7}37 ref:",
        "log lines omitted",
        "body lines omitted from",
    ] {
        assert!(
            description.contains(shape),
            "the {shape} marker is not named"
        );
    }
    assert!(
        description.contains("exactly 12"),
        "the handle grammar is not stated"
    );
    assert!(
        description.contains("range"),
        "narrowing is not recommended"
    );
    assert!(
        description.contains("Never reconstruct"),
        "the regenerate-don't-invent rule is missing"
    );
}

#[tokio::test]
async fn initialize_hands_the_client_the_marker_grammar_in_its_instructions() {
    let router = mcp_router(keyed_core());
    let (status, reply) = rpc(&router, Some(KEY_A), HOST, initialize()).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    let instructions = reply["result"]["instructions"]
        .as_str()
        .expect("instructions are the only text every client shows the model");
    assert!(instructions.contains("similar items omitted"));
    assert!(instructions.contains("never reconstruct it from memory"));
}

#[tokio::test]
async fn a_handle_minted_by_kompressor_compress_resolves_through_kompressor_retrieve() {
    let router = mcp_router(keyed_core());
    let handle = mint_handle(&router, KEY_A, 2).await;

    let (status, reply) = rpc(
        &router,
        Some(KEY_A),
        HOST,
        call(3, "optscale_kompressor_retrieve", json!({"ref": handle})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    let output = structured(&reply);
    assert_eq!(
        output["found"], true,
        "the handle the model was given must resolve"
    );
    assert!(!output["content"].as_str().unwrap().is_empty());
    assert_eq!(output["ranged"], false);
    // The text block carries the fragment itself, not a JSON-escaped copy.
    assert_eq!(text_of(&reply), output["content"].as_str().unwrap());
}

#[tokio::test]
async fn a_handle_still_resolves_when_it_is_pasted_with_its_marker_prefix() {
    let router = mcp_router(keyed_core());
    let handle = mint_handle(&router, KEY_A, 2).await;
    let (_, reply) = rpc(
        &router,
        Some(KEY_A),
        HOST,
        call(
            3,
            "optscale_kompressor_retrieve",
            json!({"ref": format!("ref:{handle}")}),
        ),
    )
    .await;
    assert_eq!(
        structured(&reply)["found"],
        true,
        "refusing the text the marker actually shows would fail on documented input"
    );
}

#[tokio::test]
async fn a_range_narrows_the_answer_and_says_that_it_did() {
    let router = mcp_router(keyed_core());
    let handle = mint_handle(&router, KEY_A, 2).await;

    let (_, whole) = rpc(
        &router,
        Some(KEY_A),
        HOST,
        call(3, "optscale_kompressor_retrieve", json!({"ref": handle})),
    )
    .await;
    let (_, window) = rpc(
        &router,
        Some(KEY_A),
        HOST,
        call(
            4,
            "optscale_kompressor_retrieve",
            json!({"ref": handle, "range": {"lines": {"start": 1, "end": 3}}}),
        ),
    )
    .await;

    let whole = structured(&whole);
    let window = structured(&window);
    assert_eq!(window["ranged"], true);
    assert!(
        window["content"].as_str().unwrap().len() < whole["content"].as_str().unwrap().len(),
        "a range is what keeps a retrieval from costing more than it saved"
    );
    assert_eq!(
        window["total_lines"], whole["total_lines"],
        "the totals describe the fragment, not the slice"
    );
}

#[tokio::test]
async fn every_range_shape_the_description_advertises_is_accepted() {
    let router = mcp_router(keyed_core());
    let handle = mint_handle(&router, KEY_A, 2).await;
    for (id, range) in [
        json!({"lines": {"start": 1, "end": 2}}),
        json!({"chars": {"start": 0, "end": 64}}),
        json!({"grep": {"pattern": "INFO", "context": 1}}),
        json!({"json_pointer": {"pointer": "/0"}}),
    ]
    .into_iter()
    .enumerate()
    {
        let (status, reply) = rpc(
            &router,
            Some(KEY_A),
            HOST,
            call(
                10 + id as i64,
                "optscale_kompressor_retrieve",
                json!({"ref": handle, "range": range}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{range} was refused: {reply}");
        assert_eq!(structured(&reply)["ranged"], true, "{range}");
    }
}

#[tokio::test]
async fn a_miss_tells_the_model_to_regenerate_the_data_rather_than_invent_it() {
    let router = mcp_router(keyed_core());
    let (status, reply) = rpc(
        &router,
        Some(KEY_A),
        HOST,
        call(
            2,
            "optscale_kompressor_retrieve",
            json!({"ref": "k_nothinghere"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a miss is an answer, not a failure");

    let output = structured(&reply);
    assert_eq!(output["found"], false);
    assert_eq!(output["expired"], false);
    let hint = output["hint"].as_str().expect("a miss carries a hint");
    assert!(
        hint.contains("Re-run"),
        "the hint must be actionable: {hint}"
    );
    assert!(
        hint.contains("rather than reconstructing"),
        "the hint must forbid inventing the content: {hint}"
    );
    assert!(
        text_of(&reply).contains(hint),
        "the model reads the text block"
    );
}

#[tokio::test]
async fn an_entry_whose_retention_window_has_passed_reports_itself_expired() {
    // Expiry is told apart from a plain miss because they call for different
    // fixes: one is a retention setting, the other is a handle from elsewhere.
    let clock = ManualClock::at(1_000_000);
    let core = core_with(
        keyed_config(),
        Arc::new(MemoryRefStore::new().with_clock(clock.clock())),
    );
    let ttl = core.config().engine.ref_ttl_seconds;
    let router = mcp_router(core);
    let handle = mint_handle(&router, KEY_A, 2).await;

    clock.set(1_000_000 + ttl * 2 + 1);
    let (_, reply) = rpc(
        &router,
        Some(KEY_A),
        HOST,
        call(3, "optscale_kompressor_retrieve", json!({"ref": handle})),
    )
    .await;
    let output = structured(&reply);
    assert_eq!(output["found"], false);
    assert_eq!(output["expired"], true);
    let hint = output["hint"].as_str().unwrap();
    assert!(hint.contains("expired"));
    assert!(hint.contains("do not reconstruct it from memory"));
}

#[tokio::test]
async fn a_handle_minted_under_one_api_key_does_not_resolve_under_another() {
    let router = mcp_router(keyed_core());
    let handle = mint_handle(&router, KEY_A, 2).await;

    let (_, mine) = rpc(
        &router,
        Some(KEY_A),
        HOST,
        call(3, "optscale_kompressor_retrieve", json!({"ref": handle})),
    )
    .await;
    assert_eq!(structured(&mine)["found"], true);

    let (_, theirs) = rpc(
        &router,
        Some(KEY_B),
        HOST,
        call(4, "optscale_kompressor_retrieve", json!({"ref": handle})),
    )
    .await;
    assert_eq!(
        structured(&theirs)["found"],
        false,
        "the tenant is part of the key, not a filter applied afterwards"
    );
}

#[tokio::test]
async fn a_tool_argument_cannot_select_a_tenant() {
    // Tool arguments are written by a model that may itself have been steered by
    // the payload it is compressing, so the tenant is taken from the connection
    // and a `tenant` argument reaches no code at all.
    let router = mcp_router(keyed_core());
    let handle = mint_handle(&router, KEY_A, 2).await;

    let (_, borrowed) = rpc(
        &router,
        Some(KEY_B),
        HOST,
        call(
            3,
            "optscale_kompressor_retrieve",
            json!({"ref": handle, "tenant": "team-a", "organization_id": "team-a"}),
        ),
    )
    .await;
    assert_eq!(
        structured(&borrowed)["found"],
        false,
        "naming a tenant in the arguments must not reach another tenant's fragments"
    );

    let (_, own) = rpc(
        &router,
        Some(KEY_A),
        HOST,
        call(
            4,
            "optscale_kompressor_retrieve",
            json!({"ref": handle, "tenant": "team-b"}),
        ),
    )
    .await;
    assert_eq!(
        structured(&own)["found"],
        true,
        "the argument is ignored rather than honoured, in both directions"
    );
}

#[tokio::test]
async fn a_request_with_no_credential_is_refused_by_the_transport() {
    let router = mcp_router(keyed_core());
    let (status, _) = rpc(&router, None, HOST, initialize()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = rpc(&router, Some("sk-guessed"), HOST, initialize()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_retrieval_rate_limit_trips_and_explains_itself() {
    let mut config = keyed_config();
    config.mcp.max_retrievals_per_minute = 2;
    let core = core_with(config, Arc::new(MemoryRefStore::new()));
    let router = mcp_router(core);
    let handle = mint_handle(&router, KEY_A, 2).await;

    for id in 3..5 {
        let (_, reply) = rpc(
            &router,
            Some(KEY_A),
            HOST,
            call(id, "optscale_kompressor_retrieve", json!({"ref": handle})),
        )
        .await;
        assert_eq!(structured(&reply)["found"], true, "under the ceiling");
    }

    let (status, reply) = rpc(
        &router,
        Some(KEY_A),
        HOST,
        call(5, "optscale_kompressor_retrieve", json!({"ref": handle})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a refusal the model must act on has to arrive as a tool result, not a transport error"
    );
    assert_eq!(reply["result"]["isError"], true);
    let text = text_of(&reply);
    assert!(text.contains("rate limit"), "{text}");
    assert!(text.contains('2'), "the ceiling is named: {text}");
    assert!(text.contains("range"), "the way out is named: {text}");

    // The ceiling is per tenant, so one caller's loop cannot deny another.
    let (_, other) = rpc(
        &router,
        Some(KEY_B),
        HOST,
        call(
            6,
            "optscale_kompressor_retrieve",
            json!({"ref": "k_notmine00000"}),
        ),
    )
    .await;
    assert_eq!(
        structured(&other)["found"],
        false,
        "answered, not rate-limited"
    );
}

#[tokio::test]
async fn an_unknown_host_is_rejected_only_when_allowed_hosts_is_set() {
    let mut config = keyed_config();
    config.mcp.allowed_hosts = vec![HOST.to_string()];
    let guarded = mcp_router(core_with(config, Arc::new(MemoryRefStore::new())));

    let (status, _) = rpc(&guarded, Some(KEY_A), HOST, initialize()).await;
    assert_eq!(status, StatusCode::OK, "the configured host is served");
    let (status, _) = rpc(&guarded, Some(KEY_A), "attacker.example", initialize()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Empty means accept any Host. An internal service is reached by its own
    // cluster DNS name, and a localhost-only default rejected exactly that,
    // which silently cost the deployment its retrieval capability.
    let open = mcp_router(keyed_core());
    let (status, _) = rpc(&open, Some(KEY_A), "kompressor.prod.svc", initialize()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = rpc(&open, Some(KEY_A), "anything.at.all", initialize()).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn kompressor_compress_defaults_to_recoverable_and_an_explicit_mode_overrides_it() {
    let router = mcp_router(keyed_core());

    let (_, default) = rpc(
        &router,
        Some(KEY_A),
        HOST,
        call(
            2,
            "optscale_kompressor_compress",
            json!({"messages": [{"role": "user", "content": log_payload(400)}]}),
        ),
    )
    .await;
    let stats = &structured(&default)["stats"];
    assert_eq!(stats["mode"], "recoverable");
    assert!(
        stats["refs_emitted"].as_u64().unwrap() > 0,
        "recoverable retains what it elided"
    );

    let (_, aggressive) = rpc(
        &router,
        Some(KEY_A),
        HOST,
        call(
            3,
            "optscale_kompressor_compress",
            json!({
                "messages": [{"role": "user", "content": log_payload(400)}],
                "mode": "aggressive"
            }),
        ),
    )
    .await;
    let stats = &structured(&aggressive)["stats"];
    assert_eq!(stats["mode"], "aggressive");
    assert_eq!(
        stats["refs_emitted"].as_u64().unwrap(),
        0,
        "aggressive announces elisions without paying to keep them"
    );
}

#[tokio::test]
async fn kompressor_compress_raw_compresses_a_standalone_payload_and_mints_a_handle() {
    let router = mcp_router(keyed_core());
    let payload = log_payload(400);
    let (status, reply) = rpc(
        &router,
        Some(KEY_A),
        HOST,
        call(
            2,
            "optscale_kompressor_compress_raw",
            json!({"content": payload}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");

    let output = structured(&reply);
    let content = output["content"].as_str().unwrap();
    assert!(content.len() < payload.len(), "nothing was compressed");
    assert_eq!(output["stats"]["mode"], "recoverable");

    let handle: String = content
        .split("ref:")
        .nth(1)
        .map(|rest| {
            rest.chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .collect()
        })
        .expect("a marker carries a handle");
    let (_, back) = rpc(
        &router,
        Some(KEY_A),
        HOST,
        call(3, "optscale_kompressor_retrieve", json!({"ref": handle})),
    )
    .await;
    assert_eq!(structured(&back)["found"], true);
}

#[tokio::test]
async fn a_message_the_engine_cannot_read_comes_back_unchanged() {
    let router = mcp_router(keyed_core());
    let (status, reply) = rpc(
        &router,
        Some(KEY_A),
        HOST,
        call(
            2,
            "optscale_kompressor_compress",
            json!({"messages": [
                {"role": "user", "content": null},
                {"role": "tool", "content": [{"text": "short", "protected": true}]}
            ]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    let messages = &structured(&reply)["messages"];
    assert!(
        messages[0]["content"].is_null(),
        "unreadable content is preserved"
    );
    assert_eq!(messages[1]["content"][0]["text"], "short");
    assert_eq!(messages[1]["content"][0]["protected"], true);
}

#[tokio::test]
async fn an_unknown_engine_option_is_refused_rather_than_silently_ignored() {
    let router = mcp_router(keyed_core());
    let (_, reply) = rpc(
        &router,
        Some(KEY_A),
        HOST,
        call(
            2,
            "optscale_kompressor_compress",
            json!({
                "messages": [{"role": "user", "content": "hello"}],
                "options": {"min_compres_chars": 10}
            }),
        ),
    )
    .await;
    // A typo that left the default quietly in place is how a deployment ends up
    // "proving" a setting does nothing.
    assert!(
        reply.get("error").is_some() || reply["result"]["isError"] == json!(true),
        "the typo was accepted: {reply}"
    );
}

#[tokio::test]
async fn kompressor_stats_reports_only_the_calling_tenants_numbers() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = keyed_config();
    config.stats.enabled = true;
    config.stats.path = directory.path().join("stats.db");

    let (sink, handle) = kompressor_stats::writer::spawn(&kompressor_stats::StatsConfig::at(
        config.stats.path.clone(),
    ))
    .unwrap();
    let core = Arc::new(AppCore::new(
        config,
        Arc::new(MemoryRefStore::new()),
        sink.clone(),
    ));
    let router = mcp_router(core);

    for (id, key) in [(2, KEY_A), (3, KEY_B), (4, KEY_B)] {
        let (status, reply) = rpc(
            &router,
            Some(key),
            HOST,
            call(
                id,
                "optscale_kompressor_compress",
                json!({"messages": [{"role": "user", "content": log_payload(400)}]}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{reply}");
    }
    sink.flush().unwrap();

    let (_, mine) = rpc(
        &router,
        Some(KEY_A),
        HOST,
        call(5, "optscale_kompressor_stats", json!({})),
    )
    .await;
    let mine = structured(&mine);
    assert_eq!(mine["tenant"], "team-a");
    assert_eq!(mine["requests"], 1);

    let (_, theirs) = rpc(
        &router,
        Some(KEY_B),
        HOST,
        call(6, "optscale_kompressor_stats", json!({})),
    )
    .await;
    let theirs = structured(&theirs);
    assert_eq!(theirs["tenant"], "team-b");
    assert_eq!(
        theirs["requests"], 2,
        "each tenant sees its own work and no one else's"
    );
    assert!(mine["tokens_in"].as_i64().unwrap() > 0);

    handle.shutdown().unwrap();
}

#[tokio::test]
async fn kompressor_stats_says_so_rather_than_failing_when_statistics_are_off() {
    let router = mcp_router(keyed_core());
    let (status, reply) = rpc(
        &router,
        Some(KEY_A),
        HOST,
        call(2, "optscale_kompressor_stats", json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(reply["result"]["isError"], true);
    assert!(text_of(&reply).contains("disabled"));
}

#[tokio::test]
async fn the_stdio_transport_answers_initialize_and_a_tool_call() {
    // Driven over an in-memory pipe rather than a child process, so the test
    // exercises the transport without a build artifact or a process to reap.
    let mut config = Config::default();
    config.stats.enabled = false;
    let core = core_with(config, Arc::new(MemoryRefStore::new()));
    let identity = Identity {
        tenant: Tenant::new("local"),
        mode: None,
        trusted: false,
    };

    let (mine, theirs) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(kompressor_mcp::serve_pipe(core, identity, theirs));

    let (reader, mut writer) = tokio::io::split(mine);
    let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(reader));

    async fn send(writer: &mut (impl tokio::io::AsyncWrite + Unpin), message: Value) {
        tokio::io::AsyncWriteExt::write_all(writer, format!("{message}\n").as_bytes())
            .await
            .unwrap();
    }

    send(&mut writer, initialize()).await;
    let reply: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert!(
        reply["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("similar items omitted"),
        "stdio serves the same instructions as the HTTP surface"
    );

    send(
        &mut writer,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    send(
        &mut writer,
        call(
            2,
            "optscale_kompressor_compress_raw",
            json!({"content": log_payload(400)}),
        ),
    )
    .await;
    let reply: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    let output = structured(&reply);
    assert_eq!(output["stats"]["mode"], "recoverable");
    assert!(output["content"].as_str().unwrap().contains("ref:k_"));

    drop(writer);
    drop(lines);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server).await;
}

#[tokio::test]
async fn the_ref_store_the_router_is_built_on_is_the_one_the_config_asks_for() {
    // A guard against the surface quietly constructing its own store: a handle
    // minted here has to be readable by a `serve` process sharing the same file.
    let directory = tempfile::tempdir().unwrap();
    let mut config = keyed_config();
    config.refs.path = directory.path().join("refs.db");
    let refs = build_ref_store(&config).unwrap();
    let router = mcp_router(core_with(config, refs.clone()));

    let handle = mint_handle(&router, KEY_A, 2).await;
    let id = kompressor_engine::RefId::parse(&handle).expect("a well-formed handle");
    let found = refs.get(&Tenant::new("team-a"), &id, false).unwrap();
    assert!(
        found.is_hit(),
        "the fragment must land in the store the deployment configured"
    );
}

/// Every count in a tool schema uses JSON Schema's own vocabulary.
///
/// `schemars` renders a `usize` with `"format": "uint"`, which is not a format
/// the specification defines. Validators ignore it, so nothing breaks — but
/// Claude Code prints a warning for each one, on every session that lists these
/// tools, in the place a user reads to find out what went wrong. This keeps the
/// next `usize` added to a wire type from quietly putting it back.
#[test]
fn no_tool_schema_declares_a_format_json_schema_does_not_define() {
    use schemars::schema_for;

    for (name, schema) in [
        (
            "RetrieveInput",
            schema_for!(kompressor_mcp::wire::RetrieveInput),
        ),
        (
            "RetrieveOutput",
            schema_for!(kompressor_mcp::wire::RetrieveOutput),
        ),
        (
            "CompressInput",
            schema_for!(kompressor_mcp::wire::CompressInput),
        ),
    ] {
        let rendered = serde_json::to_string(&schema).expect("schema serialises");
        assert!(
            !rendered.contains("\"uint"),
            "{name} still declares a uint format: use #[schemars(schema_with = \"non_negative_integer\")]"
        );
    }
}

/// A ranged or truncated retrieval says so in a text block, not only in the
/// envelope.
///
/// Claude Desktop's chat surfaces text blocks and drops `structuredContent`, so
/// a model there sees the fragment and nothing else — `truncated` included,
/// which is the one field the tool description tells it to act on. Reported from
/// a real session as "`found`: absent — the response was raw log text with no
/// envelope fields".
#[tokio::test]
async fn a_ranged_retrieval_reports_the_whole_size_in_text() {
    let core = keyed_core();
    let router = kompressor_mcp::mcp_router(core.clone());

    // Something long enough to be retained and worth ranging.
    let body: String = (1..=200)
        .map(|i| format!("line {i} of the fragment\n"))
        .collect();
    let (status, compressed) = rpc(
        &router,
        Some(KEY_A),
        HOST,
        call(
            1,
            "optscale_kompressor_compress_raw",
            json!({ "content": body }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{compressed}");

    let text = serde_json::to_string(&compressed).unwrap();
    let Some(handle) = text
        .split("ref:k_")
        .nth(1)
        .and_then(|rest| {
            rest.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
                .next()
        })
        .map(|r| format!("k_{r}"))
    else {
        // Nothing was elided, so there is nothing to range over.
        return;
    };

    let (status, retrieved) = rpc(
        &router,
        Some(KEY_A),
        HOST,
        call(
            2,
            "optscale_kompressor_retrieve",
            json!({ "ref": handle, "range": { "lines": { "start": 1, "end": 3 } } }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{retrieved}");

    let blocks = serde_json::to_string(&retrieved).unwrap();
    assert!(
        blocks.contains("[kompressor: a range of ref:"),
        "a ranged retrieval must say so in text, for clients that show no envelope: {blocks}"
    );
    assert!(
        blocks.contains("lines /"),
        "the note must carry the whole fragment's size: {blocks}"
    );
}
