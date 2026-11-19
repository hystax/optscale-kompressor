//! `proxy.reasoning_elide_models`, end to end: a turn in progress keeps its
//! reasoning, a finished one loses it once the user speaks again, what was
//! taken can be fetched back, and nothing happens for a model nobody named.

mod support;

use std::sync::Arc;

use kompressor_core::config::{Config, RetrieveTool};
use kompressor_engine::refs::RefId;
use kompressor_stats::context::DRIFT_HISTORY;
use kompressor_store::memory::MemoryRefStore;
use kompressor_store::types::{Lookup, RefStore, Tenant};
use serde_json::{Value, json};
use support::{Harness, proxy_rows};
use wiremock::matchers::any;
use wiremock::{Mock, ResponseTemplate};

const CHAT: &str = "/v1/chat/completions";
const MARKER: &str = "[reasoning from an earlier turn omitted";

async fn answering(harness: &Harness) {
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1",
            "choices": [{"index": 0, "finish_reason": "stop",
                         "message": {"role": "assistant", "content": "done"}}],
            "usage": {"prompt_tokens": 900, "completion_tokens": 12}
        })))
        .mount(&harness.server)
        .await;
}

fn thinking(tag: &str) -> String {
    format!("{tag}: ") + &"weighing the options carefully. ".repeat(20)
}

/// One user turn: a tool round of `steps` calls and, when `finished`, the
/// model's closing answer.
fn turn(ask: &str, steps: usize, finished: bool) -> Vec<Value> {
    let mut messages = vec![json!({"role": "user", "content": ask})];
    for step in 0..steps {
        let id = format!("{ask}-{step}");
        messages.push(json!({"role": "assistant", "content": null,
            "reasoning_content": thinking(&id),
            "tool_calls": [{"id": id, "type": "function",
                            "function": {"name": "read", "arguments": "{}"}}]}));
        messages.push(json!({"role": "tool", "tool_call_id": id, "content": "a result"}));
    }
    if finished {
        messages.push(
            json!({"role": "assistant", "content": format!("{ask}: done"),
                             "reasoning_content": thinking(&format!("{ask}-end"))}),
        );
    }
    messages
}

fn body(model: &str, turns: &[Vec<Value>]) -> Value {
    let messages: Vec<Value> = turns.iter().flatten().cloned().collect();
    json!({"model": model, "messages": messages})
}

fn eliding(config: &mut Config) {
    config.proxy.retrieve_tool = RetrieveTool::Never;
    config.proxy.reasoning_elide_models = vec!["deepseek-reasoner".to_string()];
}

fn reasoning_sent(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["reasoning_content"].as_str())
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn a_turn_in_progress_goes_upstream_exactly_as_it_arrived() {
    let harness = Harness::with(eliding).await;
    answering(&harness).await;
    let bodies = [
        body("deepseek-reasoner", &[turn("fix it", 1, false)]),
        body("deepseek-reasoner", &[turn("fix it", 2, false)]),
        body("deepseek-reasoner", &[turn("fix it", 2, true)]),
    ];
    for request in &bodies {
        harness.post(CHAT, request).await;
    }
    for (sent, request) in harness.upstream_bodies().await.iter().zip(&bodies) {
        assert_eq!(*sent, request.to_string(), "the bytes that arrived");
    }
}

#[tokio::test]
async fn a_finished_turn_loses_its_reasoning_once_and_then_reads_the_same() {
    let store = Arc::new(MemoryRefStore::new());
    let harness = Harness::with_store(eliding, store.clone()).await;
    answering(&harness).await;
    let first = turn("fix it", 2, true);
    // The user speaks again: the first turn is over. Then the second turn
    // runs, and then a third begins.
    let requests = [
        body("deepseek-reasoner", std::slice::from_ref(&first)),
        body(
            "deepseek-reasoner",
            &[first.clone(), turn("now the tests", 0, false)],
        ),
        body(
            "deepseek-reasoner",
            &[first.clone(), turn("now the tests", 2, false)],
        ),
        body(
            "deepseek-reasoner",
            &[
                first.clone(),
                turn("now the tests", 2, true),
                turn("ship it", 0, false),
            ],
        ),
    ];
    for request in &requests {
        harness.post(CHAT, request).await;
    }
    let sent = harness.upstream_json().await;

    assert_eq!(reasoning_sent(&sent[0]).len(), 3);
    assert!(
        reasoning_sent(&sent[0])
            .iter()
            .all(|text| !text.starts_with(MARKER))
    );

    let after = reasoning_sent(&sent[1]);
    assert_eq!(after.len(), 3);
    assert!(
        after.iter().all(|text| text.starts_with(MARKER)),
        "{after:?}"
    );

    // Mid-way through the second turn: the first is as it was a request ago,
    // and the second is whole.
    let later = reasoning_sent(&sent[2]);
    assert_eq!(later[..3], after[..]);
    assert!(later[3..].iter().all(|text| !text.starts_with(MARKER)));
    let finished = first.len();
    assert_eq!(
        sent[2]["messages"].as_array().unwrap()[..finished],
        sent[1]["messages"].as_array().unwrap()[..finished]
    );

    // The third turn begins: now the second is elided too, the first unmoved.
    let last = reasoning_sent(&sent[3]);
    assert_eq!(last[..3], after[..]);
    assert!(last[3..].iter().all(|text| text.starts_with(MARKER)));

    // Everything else in the conversation is the client's.
    let mut stripped = sent[3].clone();
    let mut original = requests[3].clone();
    for body in [&mut stripped, &mut original] {
        for message in body["messages"].as_array_mut().unwrap() {
            message.as_object_mut().unwrap().remove("reasoning_content");
        }
    }
    assert_eq!(stripped, original);

    // What was taken comes back, to the byte.
    let handle = after[0]
        .split("ref:")
        .nth(1)
        .and_then(|rest| rest.get(..14))
        .and_then(RefId::parse)
        .expect("the marker carries a handle");
    match store.get(&Tenant::anonymous(), &handle, false) {
        Ok(Lookup::Hit(record)) => assert_eq!(record.fragment, thinking("fix it-0")),
        other => panic!("the elided reasoning was not retained: {other:?}"),
    }

    // The statistics show the step for what it is: history this proxy
    // changed, on the request that opened a new turn and on no other.
    let rows = proxy_rows(&harness.events);
    let seen: Vec<_> = rows
        .iter()
        .map(|row| row.prefix.clone().expect("observed"))
        .collect();
    assert!(seen[1].reasoning_elided_bytes > 1_000);
    assert_eq!(seen[1].drift_out, DRIFT_HISTORY);
    assert_eq!(seen[2].drift_out, 0, "nothing moves while a turn runs");
    assert_eq!(seen[3].drift_out, DRIFT_HISTORY);
    assert_eq!(
        seen[3].history_out_at,
        Some(finished as u32 + 1),
        "from the second turn's first assistant message on"
    );
}

#[tokio::test]
async fn a_model_nobody_named_keeps_its_reasoning() {
    let harness = Harness::with(eliding).await;
    answering(&harness).await;
    let request = body(
        "gpt-5.5",
        &[turn("fix it", 1, true), turn("next", 0, false)],
    );
    harness.post(CHAT, &request).await;
    assert_eq!(harness.upstream_bodies().await[0], request.to_string());
}

#[tokio::test]
async fn by_default_no_model_is_named() {
    let harness = Harness::with(|config| config.proxy.retrieve_tool = RetrieveTool::Never).await;
    answering(&harness).await;
    let request = body(
        "deepseek-reasoner",
        &[turn("fix it", 1, true), turn("next", 0, false)],
    );
    harness.post(CHAT, &request).await;
    assert_eq!(harness.upstream_bodies().await[0], request.to_string());
}

/// `off` is a pipe, and `lossless` promises to drop nothing.
#[tokio::test]
async fn off_and_lossless_requests_keep_their_reasoning() {
    let harness = Harness::with(eliding).await;
    answering(&harness).await;
    let request = body(
        "deepseek-reasoner",
        &[turn("fix it", 1, true), turn("next", 0, false)],
    );
    for mode in ["off", "lossless"] {
        harness
            .post_with(CHAT, &request, &[("x-kompressor-mode", mode)])
            .await;
    }
    for sent in harness.upstream_json().await {
        assert!(
            reasoning_sent(&sent)
                .iter()
                .all(|text| !text.starts_with(MARKER))
        );
    }
}

/// In `aggressive` mode nothing is retained, and the marker says so by
/// carrying no handle.
#[tokio::test]
async fn aggressive_mode_elides_without_a_handle() {
    let harness = Harness::with(eliding).await;
    answering(&harness).await;
    let request = body(
        "deepseek-reasoner",
        &[turn("fix it", 1, true), turn("next", 0, false)],
    );
    harness
        .post_with(CHAT, &request, &[("x-kompressor-mode", "aggressive")])
        .await;
    let sent = harness.upstream_json().await;
    for text in reasoning_sent(&sent[0]) {
        assert_eq!(text, "[reasoning from an earlier turn omitted]");
    }
}
