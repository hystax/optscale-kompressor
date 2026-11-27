//! The cloud transports end to end: Gemini `generateContent`, Claude on
//! Vertex AI, and Claude on Bedrock (signed and streamed). See
//! docs/design/HEADROOM-PORT.md, Tier 5.

mod support;

use serde_json::{Value, json};
use support::{Harness, log_payload, proxy_rows};
use wiremock::matchers::any;
use wiremock::{Mock, ResponseTemplate};

use kompressor_core::config::{AwsSigV4, Config, RetrieveTool};

async fn answering(harness: &Harness, body: Value) {
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&harness.server)
        .await;
}

fn recoverable(config: &mut Config) {
    // The tool is asked for on every request, so a family that must not get
    // it is tested against the strongest setting.
    config.proxy.retrieve_tool = RetrieveTool::Always;
}

fn gemini_body(output: &str) -> Value {
    json!({
        "systemInstruction": {"parts": [{"text": "You are Gemini CLI."}]},
        "contents": [
            {"role": "user", "parts": [{"text": "why does the worker stall?"}]},
            {"role": "model", "parts": [
                {"functionCall": {"name": "run_shell_command", "args": {"command": "tail worker.log"}}}]},
            {"role": "user", "parts": [
                {"functionResponse": {"name": "run_shell_command",
                                      "response": {"output": output}}}]}
        ]
    })
}

#[tokio::test]
async fn a_gemini_function_response_is_compressed_and_its_key_and_model_kept() {
    let harness = Harness::with(recoverable).await;
    answering(
        &harness,
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "ok"}]},
                               "finishReason": "STOP"}],
               "usageMetadata": {"promptTokenCount": 1200, "candidatesTokenCount": 9,
                                 "cachedContentTokenCount": 800, "totalTokenCount": 1209}}),
    )
    .await;
    let log = log_payload(400);
    let reply = harness
        .post_with(
            "/v1beta/models/gemini-2.5-pro:generateContent?key=AIza-test",
            &gemini_body(&log),
            &[("x-goog-api-key", "AIza-test")],
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.text());

    let requests = harness.server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].url.query(),
        Some("key=AIza-test"),
        "the key rides through"
    );
    assert_eq!(requests[0].headers["x-goog-api-key"], "AIza-test");
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let output = sent["contents"][2]["parts"][0]["functionResponse"]["response"]["output"]
        .as_str()
        .unwrap();
    assert!(
        output.len() < log.len() / 2,
        "compressed: {} -> {}",
        log.len(),
        output.len()
    );
    assert!(
        sent.get("tools").is_none(),
        "no retrieve tool on a family whose loop is not read"
    );
    assert_eq!(
        sent["systemInstruction"],
        gemini_body("")["systemInstruction"]
    );

    let row = &proxy_rows(&harness.events)[0];
    assert_eq!(row.api_family, "gemini_generate_content");
    assert_eq!(row.model.as_deref(), Some("gemini-2.5-pro"));
    assert_eq!(row.prompt_tokens, Some(1200));
    assert_eq!(row.cache_read_tokens, Some(800));
}

#[tokio::test]
async fn a_gemini_stream_is_relayed_as_it_arrives() {
    let harness = Harness::with(recoverable).await;
    let sse = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"o\"}]}}]}\r\n\r\n\
               data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"k\"}]},\"finishReason\":\"STOP\"}],\
               \"usageMetadata\":{\"promptTokenCount\":50,\"candidatesTokenCount\":2}}\r\n\r\n";
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse.as_bytes().to_vec(), "text/event-stream"),
        )
        .mount(&harness.server)
        .await;
    let reply = harness
        .post(
            "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse",
            &gemini_body(&log_payload(300)),
        )
        .await;
    assert_eq!(reply.status, 200);
    let text = reply.text();
    assert!(
        text.contains("\"text\":\"o\"") && text.contains("finishReason"),
        "{text}"
    );
    let rows = proxy_rows(&harness.events);
    assert_eq!(rows[0].prompt_tokens, Some(50));
}

#[tokio::test]
async fn claude_on_vertex_is_walked_as_anthropic_and_named_from_its_path() {
    let harness = Harness::with(recoverable).await;
    answering(
        &harness,
        json!({"id": "msg_1", "type": "message", "role": "assistant",
               "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn",
               "usage": {"input_tokens": 300, "output_tokens": 2}}),
    )
    .await;
    let log = log_payload(400);
    let body = json!({
        "anthropic_version": "vertex-2023-10-16", "max_tokens": 512,
        "messages": [
            {"role": "user", "content": "check the log"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "tail log"}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": log}]}
        ]
    });
    let path = "/v1/projects/p/locations/us-east5/publishers/anthropic/models/claude-sonnet-4-5@20250929:rawPredict";
    let reply = harness
        .post_with(path, &body, &[("authorization", "Bearer ya29.token")])
        .await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    let requests = harness.server.received_requests().await.unwrap();
    assert_eq!(requests[0].headers["authorization"], "Bearer ya29.token");
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let result = sent["messages"][2]["content"][0]["content"]
        .as_str()
        .unwrap();
    assert!(result.len() < log.len() / 2);
    let row = &proxy_rows(&harness.events)[0];
    assert_eq!(row.api_family, "anthropic_messages");
    assert_eq!(row.model.as_deref(), Some("claude-sonnet-4-5@20250929"));
}

fn bedrock_body(log: &str) -> Value {
    json!({
        "anthropic_version": "bedrock-2023-05-31", "max_tokens": 512,
        "messages": [
            {"role": "user", "content": "check the log"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "tail log"}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": log}]}
        ]
    })
}

/// One test touches the AWS variables, and it sets them before any request:
/// no other test in this binary reads them.
#[tokio::test]
async fn a_bedrock_request_is_signed_over_the_body_that_goes_out() {
    // SAFETY: edition-2024 marks this unsafe because another thread could be
    // reading the environment; nothing in this test binary reads these
    // variables except the signer, which runs after this line.
    unsafe {
        std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDTEST");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "secret/test");
        std::env::remove_var("AWS_SESSION_TOKEN");
    }
    let harness = Harness::with(|config| {
        config.proxy.upstreams[0].name = "bedrock".into();
        config.proxy.upstreams[0].aws_sigv4 = Some(AwsSigV4 {
            region: "us-west-2".into(),
            service: "bedrock".into(),
        });
    })
    .await;
    answering(
        &harness,
        json!({"id": "msg_1", "type": "message", "role": "assistant",
               "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn",
               "usage": {"input_tokens": 300, "output_tokens": 2}}),
    )
    .await;
    let log = log_payload(400);
    let reply = harness
        .post_with(
            "/model/anthropic.claude-sonnet-4-5-20250929-v1:0/invoke",
            &bedrock_body(&log),
            &[
                (
                    "authorization",
                    "AWS4-HMAC-SHA256 Credential=CLIENT/x, Signature=stale",
                ),
                ("x-amz-date", "20000101T000000Z"),
            ],
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    let requests = harness.server.received_requests().await.unwrap();
    let auth = requests[0].headers["authorization"].to_str().unwrap();
    assert!(
        auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDTEST/")
            && auth.contains("/us-west-2/bedrock/aws4_request"),
        "{auth}"
    );
    assert!(!auth.contains("stale"));
    assert_ne!(requests[0].headers["x-amz-date"], "20000101T000000Z");
    let hash = ring::digest::digest(&ring::digest::SHA256, &requests[0].body);
    let hex: String = hash.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        requests[0].headers["x-amz-content-sha256"]
            .to_str()
            .unwrap(),
        hex,
        "the signature covers the compressed body"
    );
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(
        sent["messages"][2]["content"][0]["content"]
            .as_str()
            .unwrap()
            .len()
            < log.len() / 2
    );
    let row = &proxy_rows(&harness.events)[0];
    assert_eq!(
        row.model.as_deref(),
        Some("anthropic.claude-sonnet-4-5-20250929-v1:0")
    );
}

#[tokio::test]
async fn a_bedrock_event_stream_is_piped_through_and_never_offered_the_tool() {
    let harness = Harness::with(recoverable).await;
    // Not real event-stream frames: bytes the proxy must relay untouched.
    let frames: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(frames.clone(), "application/vnd.amazon.eventstream"),
        )
        .mount(&harness.server)
        .await;
    let reply = harness
        .post_with(
            "/model/anthropic.claude-sonnet-4-5-20250929-v1:0/invoke-with-response-stream",
            &bedrock_body(&log_payload(400)),
            &[("authorization", "Bearer ABSKbedrockapikey")],
        )
        .await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.body.as_ref(), frames.as_slice(), "byte for byte");
    let requests = harness.server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].headers["authorization"],
        "Bearer ABSKbedrockapikey"
    );
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(sent.get("tools").is_none(), "{sent}");
}

#[tokio::test]
async fn a_converse_tool_result_is_compressed() {
    let harness = Harness::new().await;
    answering(
        &harness,
        json!({"output": {"message": {"role": "assistant", "content": [{"text": "ok"}]}},
               "stopReason": "end_turn",
               "usage": {"inputTokens": 700, "outputTokens": 3, "totalTokens": 703}}),
    )
    .await;
    let log = log_payload(400);
    let body = json!({
        "system": [{"text": "You are an agent."}],
        "messages": [
            {"role": "user", "content": [{"text": "check the log"}]},
            {"role": "assistant", "content": [
                {"toolUse": {"toolUseId": "t1", "name": "bash", "input": {"cmd": "tail"}}}]},
            {"role": "user", "content": [
                {"toolResult": {"toolUseId": "t1", "content": [{"text": log}]}}]}
        ]
    });
    let reply = harness
        .post("/model/amazon.nova-pro-v1:0/converse", &body)
        .await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    let sent = &harness.upstream_json().await[0];
    let text = sent["messages"][2]["content"][0]["toolResult"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.len() < log.len() / 2);
    let row = &proxy_rows(&harness.events)[0];
    assert_eq!(row.api_family, "bedrock_converse");
    assert_eq!(row.prompt_tokens, Some(700));
}
