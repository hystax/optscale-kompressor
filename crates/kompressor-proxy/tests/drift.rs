//! What the proxy records about a request's cacheable prefix, end to end.
//!
//! The unit tests in `drift.rs` state the comparison. These state that it is
//! made on the real request path — after every rewrite `prepare` makes — and
//! that the verdict reaches the statistics row: a client that changes a tool is
//! blamed for it, a change this proxy makes is its own, and a conversation it
//! compresses the same way every turn reports no drift at all.

mod support;

use kompressor_stats::context::{
    DRIFT_HISTORY, DRIFT_TOOL_DEFS, DRIFT_TOOL_SET, PrefixObservation,
};
use serde_json::{Value, json};
use support::{Harness, log_payload, proxy_rows};
use wiremock::matchers::any;
use wiremock::{Mock, ResponseTemplate};

const CHAT: &str = "/v1/chat/completions";

fn answered() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": "ok"}}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 5}
    }))
}

/// A conversation with a tool list and one large tool result, `turns` further
/// exchanges appended.
fn conversation(turns: usize) -> Value {
    let mut messages = vec![
        json!({"role": "system", "content": "You are a careful assistant."}),
        json!({"role": "user", "content": "What went wrong in this log?"}),
        json!({"role": "assistant", "content": null, "tool_calls": [
            {"id": "call_0", "type": "function",
             "function": {"name": "read_file", "arguments": "{\"path\":\"/var/log/app.log\"}"}}
        ]}),
        json!({"role": "tool", "tool_call_id": "call_0", "content": log_payload(500)}),
    ];
    for turn in 0..turns {
        messages.push(json!({"role": "assistant", "content": format!("answer {turn}")}));
        messages.push(json!({"role": "user", "content": format!("and then {turn}?")}));
    }
    json!({
        "model": "gpt-5",
        "messages": messages,
        "tools": [{"type": "function", "function": {
            "name": "read_file", "description": "Read a file",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
        }}]
    })
}

async fn harness() -> Harness {
    let harness = Harness::new().await;
    Mock::given(any())
        .respond_with(answered())
        .mount(&harness.server)
        .await;
    harness
}

/// The observation on the one row the last request filed.
fn observed(harness: &Harness) -> PrefixObservation {
    let rows = proxy_rows(&harness.events);
    assert_eq!(rows.len(), 1, "one request, one row");
    rows.into_iter()
        .next()
        .and_then(|row| row.prefix)
        .expect("a parsed request carries an observation")
}

#[tokio::test]
async fn a_conversation_compressed_the_same_every_turn_reports_no_drift() {
    let harness = harness().await;

    assert!(
        harness
            .post(CHAT, &conversation(0))
            .await
            .status
            .is_success()
    );
    let first = observed(&harness);
    assert!(!first.compared, "nothing to compare the first request with");
    assert_eq!((first.tools_count, first.messages_count), (1, 3));
    assert!(first.system_bytes > 0 && first.tools_bytes > 0);
    assert!(
        first.text_bytes > 20_000,
        "the tool result is text the engine sees"
    );
    assert!(first.body_bytes >= first.messages_bytes + first.tools_bytes);

    assert!(
        harness
            .post(CHAT, &conversation(1))
            .await
            .status
            .is_success()
    );
    let second = observed(&harness);
    // The upstream bodies really were rewritten, or this proves nothing.
    let sent = harness.upstream_bodies().await;
    assert!(
        sent[1].contains("ref:k_"),
        "the history went out compressed"
    );
    assert!(second.compared);
    assert_eq!(
        (second.drift_in, second.drift_out),
        (0, 0),
        "appending a turn moves nothing, on either side"
    );
    assert!(second.idle_ms.is_some());

    assert!(
        harness
            .post(CHAT, &conversation(2))
            .await
            .status
            .is_success()
    );
    let third = observed(&harness);
    assert_eq!((third.drift_in, third.drift_out), (0, 0));
}

#[tokio::test]
async fn a_tool_the_client_redefines_is_the_clients_drift() {
    let harness = harness().await;
    harness.post(CHAT, &conversation(0)).await;
    observed(&harness);

    let mut changed = conversation(1);
    changed["tools"][0]["function"]["description"] = json!("Read a file from disk");
    harness.post(CHAT, &changed).await;
    let seen = observed(&harness);
    assert_eq!(seen.drift_in, DRIFT_TOOL_DEFS);
    assert_eq!(seen.drift_out, 0, "the proxy forwarded what it was sent");
}

/// The first turn is forwarded untouched and the second is compressed, so the
/// history and the tool list both change on the way out while the client
/// changed neither — the shape of the first-handle break in DESIGN §20 item 8.
#[tokio::test]
async fn a_change_the_proxy_makes_is_its_own_drift() {
    let harness = harness().await;
    harness
        .post_with(CHAT, &conversation(0), &[("x-kompressor-mode", "off")])
        .await;
    assert!(!observed(&harness).compared);

    harness.post(CHAT, &conversation(1)).await;
    let seen = observed(&harness);
    let sent = harness.upstream_bodies().await;
    assert!(!sent[0].contains("ref:k_") && sent[1].contains("ref:k_"));
    assert_eq!(
        seen.drift_in, 0,
        "the client appended a turn and nothing else"
    );
    assert_eq!(seen.drift_out, DRIFT_HISTORY | DRIFT_TOOL_SET);
    assert_eq!(
        seen.history_out_at,
        Some(2),
        "the tool result, system left out"
    );
}

/// Observation only: with it recorded, a request the proxy does not compress
/// still reaches the upstream as the bytes that arrived.
#[tokio::test]
async fn observing_changes_no_byte_of_the_request() {
    let harness = harness().await;
    let body = conversation(0);
    harness
        .post_with(CHAT, &body, &[("x-kompressor-mode", "off")])
        .await;
    assert!(observed(&harness).body_bytes > 0);
    assert_eq!(harness.upstream_bodies().await[0], body.to_string());
}

#[tokio::test]
async fn a_body_that_is_not_a_conversation_files_no_observation() {
    let harness = harness().await;
    harness.post(CHAT, &json!({"model": "gpt-5"})).await;
    let rows = proxy_rows(&harness.events);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].prefix, None, "unknown, not `nothing drifted`");
}

/// The half of a lapse prediction that idle time does not show is on every
/// row, whatever the settings: whether the request carried more messages
/// than any before it under its conversation.
#[tokio::test]
async fn each_row_says_whether_its_request_continued_the_conversation() {
    let harness = harness().await;
    harness.post(CHAT, &conversation(0)).await;
    assert_eq!(
        observed(&harness).continues,
        None,
        "the first request has no conversation to continue"
    );
    harness.post(CHAT, &conversation(1)).await;
    assert_eq!(observed(&harness).continues, Some(true));
    // The same request again, as a retry or a second agent opening with the
    // same bytes sends it: no further than the conversation has been.
    harness.post(CHAT, &conversation(1)).await;
    assert_eq!(observed(&harness).continues, Some(false));
    harness.post(CHAT, &conversation(0)).await;
    assert_eq!(observed(&harness).continues, Some(false), "a branch");
    harness.post(CHAT, &conversation(2)).await;
    assert_eq!(observed(&harness).continues, Some(true));
}

/// The volatile scan is reported — and so made — on the first request of a
/// conversation and again when its tools change, and on no request between.
#[tokio::test]
async fn volatile_values_are_reported_when_the_opening_is_news_and_not_between() {
    let harness = harness().await;
    let dated = |turns, described: &str| {
        let mut body = conversation(turns);
        body["messages"][0]["content"] = json!("Today's date is 2026-11-20.");
        body["tools"][0]["function"]["description"] = json!(described);
        body
    };
    harness.post(CHAT, &dated(0, "Read a file")).await;
    assert_eq!(
        observed(&harness).volatile.map(|found| found.dates),
        Some(1)
    );
    harness.post(CHAT, &dated(1, "Read a file")).await;
    assert_eq!(observed(&harness).volatile, None);
    harness
        .post(CHAT, &dated(2, "Read a file, as of 2026-11-21"))
        .await;
    let changed = observed(&harness);
    assert_eq!(changed.drift_in, DRIFT_TOOL_DEFS);
    assert_eq!(
        changed.volatile.map(|found| found.dates),
        Some(2),
        "the instructions' and the tool's"
    );
    harness
        .post(CHAT, &dated(3, "Read a file, as of 2026-11-21"))
        .await;
    assert_eq!(observed(&harness).volatile, None);
}

/// The request's size is the bytes that arrived, not a re-serialization of
/// what they parsed to; and the provider's tool-search tool in the list is on
/// the row.
#[tokio::test]
async fn the_row_carries_the_size_that_arrived_and_the_tool_search_tool() {
    let harness = harness().await;
    let mut body = conversation(0);
    let spaced = serde_json::to_string_pretty(&body).unwrap();
    let reply = harness
        .send(
            http::Request::post(CHAT)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(spaced.clone()))
                .unwrap(),
        )
        .await;
    assert!(reply.status.is_success());
    let seen = observed(&harness);
    assert_eq!(seen.body_bytes as usize, spaced.len());
    assert!(spaced.len() > body.to_string().len());
    assert!(!seen.tool_search);

    body["tools"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type": "tool_search"}));
    body["messages"][1]["content"] = json!("Another conversation.");
    harness.post(CHAT, &body).await;
    let seen = observed(&harness);
    assert!(seen.tool_search);
    assert_eq!((seen.tools_count, seen.tools_deferred), (2, 0));
}
