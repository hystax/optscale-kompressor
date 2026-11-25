//! The stability guards, end to end: what the provider receives when a
//! rewrite breaks something the provider checks, under each mode.
//!
//! No rewrite this proxy makes is known to break a guard's rule, so a trip is
//! produced with `ProxyState::corrupt_next_rewrite_for_test`, which damages
//! the next rewritten body after every rewrite and before the guards run —
//! as a defective rewrite would. Each mode is then held to its contract:
//! `observe` sends the rewritten body and counts; `enforce` sends the
//! client's own bytes and counts a fail-open; `off` does nothing.

mod support;

use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, ResponseTemplate};

use support::{Harness, log_payload};

use kompressor_core::config::{Config, GuardMode};
use kompressor_proxy::Guard;
use kompressor_proxy::failopen::Stage;

const MESSAGES: &str = "/v1/messages";
const RESPONSES: &str = "/v1/responses";

async fn answering(harness: &Harness) {
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": "Disk full."}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 900, "output_tokens": 12}
        })))
        .mount(&harness.server)
        .await;
}

/// An agent turn with signed thinking, a tool call, a tool result worth
/// compressing, and four breakpoints — one on each of tools and system, the
/// opening question and the tool call — which is as many as the API allows.
/// The tool result comes after the last of them, so it is compressed.
fn with_thinking(lines: usize) -> Value {
    json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 1024,
        "tools": [{"name": "read_file", "input_schema": {"type": "object"},
                   "cache_control": {"type": "ephemeral"}}],
        "system": [{"type": "text", "text": "You are a careful assistant.",
                    "cache_control": {"type": "ephemeral"}}],
        "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "What went wrong in this log?",
                 "cache_control": {"type": "ephemeral"}}
            ]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "I should read the log first.",
                 "signature": "RXZlcnkgYnl0ZSBvZiB0aGlzIGlzIHNpZ25lZA=="},
                {"type": "redacted_thinking", "data": "b3BhcXVlIHRvIHVz"},
                {"type": "tool_use", "id": "toolu_0", "name": "read_file",
                 "input": {"path": "/var/log/app.log"},
                 "cache_control": {"type": "ephemeral"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_0", "content": log_payload(lines)}
            ]}
        ]
    })
}

/// The tool result's text as it went upstream.
fn tool_result(sent: &Value) -> &str {
    sent["messages"][2]["content"][0]["content"]
        .as_str()
        .unwrap_or_default()
}

fn guarded(thinking: GuardMode, upstream_body: GuardMode) -> impl FnOnce(&mut Config) {
    move |config: &mut Config| {
        config.proxy.guard_thinking = thinking;
        config.proxy.guard_upstream_body = upstream_body;
    }
}

/// The bytes the upstream received for the request at `at`.
async fn raw(harness: &Harness, at: usize) -> Vec<u8> {
    harness.upstream_raw_bodies().await.swap_remove(at)
}

#[tokio::test]
async fn a_rewrite_that_keeps_every_rule_trips_nothing_in_either_mode() {
    for mode in [GuardMode::Observe, GuardMode::Enforce] {
        let harness = Harness::with(guarded(mode, mode)).await;
        answering(&harness).await;
        let body = with_thinking(400);
        let reply = harness.post(MESSAGES, &body).await;
        assert_eq!(reply.status, 200);

        let sent = &harness.upstream_json().await[0];
        assert!(
            tool_result(sent).len() < log_payload(400).len(),
            "the fixture must be compressed, or nothing is being guarded"
        );
        assert_eq!(
            sent["messages"][1], body["messages"][1],
            "signed thinking goes out as it came"
        );
        let guards = harness.state.guards();
        assert_eq!(guards.total(), 0, "{:?}", guards.snapshot());
        assert_eq!(guards.snapshot()["client_invalid"], 0);
        assert_eq!(harness.failopen.count(Stage::Guard), 0);
    }
}

#[tokio::test]
async fn observe_counts_a_changed_signature_and_still_sends_the_rewrite() {
    let harness = Harness::with(guarded(GuardMode::Observe, GuardMode::Observe)).await;
    answering(&harness).await;
    harness.state.corrupt_next_rewrite_for_test(|body| {
        body["messages"][1]["content"][0]["signature"] = json!("Zm9yZ2Vk");
    });
    let reply = harness.post(MESSAGES, &with_thinking(400)).await;
    assert_eq!(reply.status, 200);

    let sent = &harness.upstream_json().await[0];
    assert_eq!(
        sent["messages"][1]["content"][0]["signature"], "Zm9yZ2Vk",
        "observe-only: the rewritten body, defect and all, went out"
    );
    assert!(tool_result(sent).len() < log_payload(400).len());
    let guards = harness.state.guards();
    assert_eq!(guards.trips(Guard::Thinking), 1);
    assert_eq!(guards.trips(Guard::UpstreamBody), 0);
    assert_eq!(guards.enforced(), 0);
    assert_eq!(harness.failopen.count(Stage::Guard), 0, "not a fail-open");
    assert_eq!(
        harness.state.held().map(|held| held.guard_trips),
        Some(1),
        "and the Maintenance page's count says so"
    );
}

#[tokio::test]
async fn enforce_sends_the_clients_own_bytes_when_thinking_changed() {
    let harness = Harness::with(guarded(GuardMode::Enforce, GuardMode::Observe)).await;
    answering(&harness).await;
    harness.state.corrupt_next_rewrite_for_test(|body| {
        let blocks = body["messages"][1]["content"].as_array_mut().unwrap();
        blocks.remove(1);
    });
    let body = with_thinking(400);
    let reply = harness.post(MESSAGES, &body).await;
    assert_eq!(reply.status, 200);

    assert_eq!(
        raw(&harness, 0).await,
        body.to_string().into_bytes(),
        "the client's bytes, exactly as they arrived"
    );
    let guards = harness.state.guards();
    assert_eq!(guards.trips(Guard::Thinking), 1);
    assert_eq!(guards.enforced(), 1);
    assert_eq!(harness.failopen.count(Stage::Guard), 1);
    assert_eq!(
        reply.header("x-kompressor-saved-tokens"),
        "0",
        "nothing is claimed for a rewrite that was not sent"
    );
}

#[tokio::test]
async fn a_fifth_breakpoint_is_caught_and_enforce_sends_the_original() {
    let fifth = |body: &mut Value| {
        body["cache_control"] = json!({"type": "ephemeral"});
    };

    let observed = Harness::with(guarded(GuardMode::Observe, GuardMode::Observe)).await;
    answering(&observed).await;
    observed.state.corrupt_next_rewrite_for_test(fifth);
    observed.post(MESSAGES, &with_thinking(400)).await;
    assert!(
        observed.upstream_json().await[0]
            .get("cache_control")
            .is_some()
    );
    assert_eq!(observed.state.guards().trips(Guard::UpstreamBody), 1);
    assert_eq!(observed.state.guards().trips(Guard::Thinking), 0);

    let enforced = Harness::with(guarded(GuardMode::Observe, GuardMode::Enforce)).await;
    answering(&enforced).await;
    enforced.state.corrupt_next_rewrite_for_test(fifth);
    let body = with_thinking(400);
    enforced.post(MESSAGES, &body).await;
    assert_eq!(raw(&enforced, 0).await, body.to_string().into_bytes());
    assert_eq!(enforced.state.guards().trips(Guard::UpstreamBody), 1);
    assert_eq!(enforced.failopen.count(Stage::Guard), 1);
}

#[tokio::test]
async fn a_one_hour_breakpoint_after_a_five_minute_one_is_caught() {
    let harness = Harness::with(guarded(GuardMode::Observe, GuardMode::Enforce)).await;
    answering(&harness).await;
    harness.state.corrupt_next_rewrite_for_test(|body| {
        body["messages"][1]["content"][2]["cache_control"] =
            json!({"type": "ephemeral", "ttl": "1h"});
    });
    let body = with_thinking(400);
    harness.post(MESSAGES, &body).await;
    assert_eq!(raw(&harness, 0).await, body.to_string().into_bytes());
    assert_eq!(harness.state.guards().trips(Guard::UpstreamBody), 1);
}

#[tokio::test]
async fn a_tool_result_whose_tool_use_was_lost_is_caught() {
    let harness = Harness::with(guarded(GuardMode::Observe, GuardMode::Enforce)).await;
    answering(&harness).await;
    harness.state.corrupt_next_rewrite_for_test(|body| {
        body["messages"][1]["content"][2]["id"] = json!("toolu_renamed");
    });
    let body = with_thinking(400);
    harness.post(MESSAGES, &body).await;
    assert_eq!(raw(&harness, 0).await, body.to_string().into_bytes());
    assert_eq!(harness.state.guards().trips(Guard::UpstreamBody), 1);
    assert_eq!(harness.failopen.count(Stage::Guard), 1);
}

/// The client sent five breakpoints and an orphaned tool result. The provider
/// will refuse that whatever this proxy does, so sending the original instead
/// of the rewrite would only throw the compression away: no trip, even under
/// `enforce`, and the client's fault is counted apart.
#[tokio::test]
async fn a_fault_the_client_sent_does_not_trip_even_under_enforce() {
    let harness = Harness::with(guarded(GuardMode::Enforce, GuardMode::Enforce)).await;
    answering(&harness).await;
    let mut body = with_thinking(400);
    body["cache_control"] = json!({"type": "ephemeral"});
    body["messages"][1]["content"][2]["id"] = json!("toolu_elsewhere");
    harness.post(MESSAGES, &body).await;

    let sent = &harness.upstream_json().await[0];
    assert!(
        tool_result(sent).len() < log_payload(400).len(),
        "the rewrite went out"
    );
    let guards = harness.state.guards();
    assert_eq!(guards.total(), 0, "{:?}", guards.snapshot());
    assert_eq!(guards.snapshot()["client_invalid"], 1);
    assert_eq!(harness.failopen.count(Stage::Guard), 0);
}

#[tokio::test]
async fn off_does_not_look() {
    let harness = Harness::with(guarded(GuardMode::Off, GuardMode::Off)).await;
    answering(&harness).await;
    harness.state.corrupt_next_rewrite_for_test(|body| {
        body["messages"][1]["content"][0]["signature"] = json!("Zm9yZ2Vk");
        body["cache_control"] = json!({"type": "ephemeral"});
    });
    harness.post(MESSAGES, &with_thinking(400)).await;
    let sent = &harness.upstream_json().await[0];
    assert_eq!(sent["messages"][1]["content"][0]["signature"], "Zm9yZ2Vk");
    assert_eq!(harness.state.guards().total(), 0);
}

#[tokio::test]
async fn a_responses_reasoning_item_with_encrypted_content_is_held_too() {
    let harness = Harness::with(guarded(GuardMode::Enforce, GuardMode::Observe)).await;
    answering(&harness).await;
    harness.state.corrupt_next_rewrite_for_test(|body| {
        body["input"][1]["encrypted_content"] = json!("gAAAAB-forged");
    });
    let body = json!({
        "model": "gpt-5",
        "instructions": "You are a careful assistant.",
        "input": [
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "What went wrong in this log?"}]},
            {"type": "reasoning", "id": "rs_0", "summary": [],
             "encrypted_content": "gAAAAB-signed-by-the-provider"},
            {"type": "function_call", "id": "fc_0", "call_id": "call_0",
             "name": "read_file", "arguments": "{\"path\":\"/var/log/app.log\"}"},
            {"type": "function_call_output", "call_id": "call_0", "output": log_payload(400)}
        ]
    });
    harness.post(RESPONSES, &body).await;
    assert_eq!(raw(&harness, 0).await, body.to_string().into_bytes());
    assert_eq!(harness.state.guards().trips(Guard::Thinking), 1);
}

/// Two turns of one conversation, the second carrying the first's history.
fn turn(n: usize) -> Value {
    let mut messages = vec![json!({"role": "user", "content": "What went wrong in these logs?"})];
    for i in 0..n {
        messages.push(json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": format!("toolu_{i}"), "name": "read_file",
             "input": {"path": format!("/var/log/app{i}.log")}}
        ]}));
        messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": format!("toolu_{i}"),
             "content": log_payload(300 + i)}
        ]}));
    }
    json!({"model": "claude-sonnet-4-5", "max_tokens": 1024, "messages": messages})
}

/// The second turn gives up (its rewrite cannot be re-encoded, so the
/// client's bytes go out) after the first went out compressed: counted on
/// `after_compressed` as well as on the stage that gave up. Without an earlier
/// compressed turn the same give-up is not.
#[tokio::test]
async fn a_give_up_after_a_compressed_turn_is_counted_apart() {
    let harness = Harness::new().await;
    answering(&harness).await;
    let first = harness.post_zstd(MESSAGES, &turn(1)).await;
    assert_ne!(first.header("x-kompressor-saved-tokens"), "0");
    assert_eq!(harness.failopen.count(Stage::AfterCompressed), 0);

    harness.state.fail_next_encode_for_test();
    harness.post_zstd(MESSAGES, &turn(2)).await;
    assert_eq!(harness.failopen.count(Stage::Parse), 1);
    assert_eq!(harness.failopen.count(Stage::AfterCompressed), 1);
    assert_eq!(
        harness
            .state
            .held()
            .map(|held| held.failopen_after_compressed),
        Some(1)
    );

    // The same give-up on a conversation's first turn has nothing before it.
    let fresh = Harness::new().await;
    answering(&fresh).await;
    fresh.state.fail_next_encode_for_test();
    fresh.post_zstd(MESSAGES, &turn(1)).await;
    fresh.state.fail_next_encode_for_test();
    fresh.post_zstd(MESSAGES, &turn(2)).await;
    assert_eq!(fresh.failopen.count(Stage::Parse), 2);
    assert_eq!(fresh.failopen.count(Stage::AfterCompressed), 0);
}

/// An enforced guard is a give-up like any other, and after a compressed
/// turn it is counted as one.
#[tokio::test]
async fn an_enforced_guard_after_a_compressed_turn_is_counted_too() {
    let harness = Harness::with(guarded(GuardMode::Enforce, GuardMode::Enforce)).await;
    answering(&harness).await;
    harness.post(MESSAGES, &turn(1)).await;
    harness.state.corrupt_next_rewrite_for_test(|body| {
        body["messages"][1]["content"][0]["id"] = json!("toolu_renamed");
    });
    harness.post(MESSAGES, &turn(2)).await;
    assert_eq!(harness.failopen.count(Stage::Guard), 1);
    assert_eq!(harness.failopen.count(Stage::AfterCompressed), 1);
}
