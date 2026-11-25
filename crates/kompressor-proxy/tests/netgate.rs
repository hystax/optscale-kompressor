//! The net-cost elision gate (`proxy.elision_gate = "net_cost"`), end to end.
//!
//! The gate prices each handle-bearing elision against what a retrieve round
//! costs on this conversation, and that cost grows with the prompt. What must
//! not grow with it is a message already sent: the gate decides a message once,
//! when it first arrives, and every later turn replays that decision, or the
//! bytes of a message the provider has cached would change under it.

mod support;

use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, Request, ResponseTemplate};

use kompressor_core::config::ElisionGate;
use support::Harness;

/// Search output over five files, each file's middle elided behind a handle.
fn search_payload(module: &str) -> String {
    (0..5)
        .flat_map(|f| {
            (0..40).map(move |i| {
                format!(
                    "src/{module}_{f}/handler.rs:{}:    let value_{i} = compute_thing(input_{i}, &state);",
                    i * 3 + 1
                )
            })
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn turn_one() -> Value {
    json!({
        "model": "gpt-5",
        "messages": [
            {"role": "system", "content": "You are a careful assistant."},
            {"role": "user", "content": "Where is compute_thing called?"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_0", "type": "function",
                 "function": {"name": "grep", "arguments": "{\"pattern\":\"compute_thing\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_0", "content": search_payload("module")}
        ]
    })
}

fn turn_two() -> Value {
    let mut body = turn_one();
    let messages = body["messages"].as_array_mut().unwrap();
    messages.push(json!({"role": "assistant", "content": null, "tool_calls": [
        {"id": "call_1", "type": "function",
         "function": {"name": "grep", "arguments": "{\"pattern\":\"compute_other\"}"}}
    ]}));
    messages.push(json!({"role": "tool", "tool_call_id": "call_1",
                         "content": search_payload("crate")}));
    body
}

/// Answers every turn and reports an enormous billed prompt. The first turn
/// has no usage to go on and is priced at its own estimated size, a few
/// thousand tokens; every later turn reads the reported one, and prices a
/// fetch far above anything the first turn admitted.
async fn mount_growing_prompt(harness: &Harness) {
    Mock::given(any())
        .respond_with(|_: &Request| {
            let prompt = 10_000_000;
            ResponseTemplate::new(200).set_body_json(json!({
                "id": "chatcmpl-1",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                             "finish_reason": "stop"}],
                "usage": {"prompt_tokens": prompt, "completion_tokens": 5}
            }))
        })
        .mount(&harness.server)
        .await;
}

fn handles(text: &str) -> usize {
    kompressor_engine::refs_in(text).len()
}

#[tokio::test]
async fn under_the_net_cost_gate_a_message_ships_the_same_bytes_as_the_prompt_grows() {
    let harness = Harness::with(|config| config.proxy.elision_gate = ElisionGate::NetCost).await;
    mount_growing_prompt(&harness).await;

    assert_eq!(
        harness
            .post("/v1/chat/completions", &turn_one())
            .await
            .status,
        200
    );
    assert_eq!(
        harness
            .post("/v1/chat/completions", &turn_two())
            .await
            .status,
        200
    );
    let sent = harness.upstream_json().await;
    assert_eq!(sent.len(), 2);
    let first = sent[0]["messages"][3]["content"].as_str().unwrap();
    let again = sent[1]["messages"][3]["content"].as_str().unwrap();

    // Turn one, priced against a small prompt: the elision pays.
    assert!(
        handles(first) >= 3,
        "turn one admits the elision against a small prompt: {first:.300}"
    );
    // Turn two reports a ten-million-token prompt, which no elision pays for:
    // the output that is new on turn two ships whole.
    let new = sent[1]["messages"][5]["content"].as_str().unwrap();
    assert!(
        new == search_payload("crate"),
        "the gate really did see the larger prompt: {} handles in the new output",
        handles(new)
    );
    // And the message decided on turn one is decided the same on turn two.
    assert!(
        again == first,
        "the same message, the same bytes, however much the prompt grew: \
         {} handles then, {} now",
        handles(first),
        handles(again)
    );
}

#[tokio::test]
async fn the_fixed_gate_is_the_default_and_ignores_the_prompt() {
    let harness = Harness::new().await;
    mount_growing_prompt(&harness).await;
    harness.post("/v1/chat/completions", &turn_one()).await;
    harness.post("/v1/chat/completions", &turn_two()).await;
    let sent = harness.upstream_json().await;
    let new = sent[1]["messages"][5]["content"].as_str().unwrap();
    assert!(
        handles(new) >= 3,
        "without the net-cost gate the new output is elided whatever the prompt: {new:.300}"
    );
}
