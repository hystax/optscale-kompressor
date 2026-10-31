//! Keeping an Anthropic conversation's cache warm while the client runs a tool.
//!
//! What these pin down is what the provider sees and what the bill records:
//! pings go out only after a `tool_use` stop, at the configured interval, as
//! the same request with `max_tokens` of 1; they stop when the client comes
//! back, when the cap is reached, and when a ping finds the cache gone; and
//! each one is filed as its own `keep_warm` row.

mod support;

use std::time::Duration;

use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, ResponseTemplate};

use support::{Harness, proxy_rows};

const INTERVAL: Duration = Duration::from_millis(150);

async fn keeping_warm(max_pings: u32) -> Harness {
    Harness::with(|config| {
        config.proxy.cache_keep_warm = true;
        config.proxy.cache_keep_warm_interval = INTERVAL;
        config.proxy.cache_keep_warm_max_pings = max_pings;
    })
    .await
}

fn conversation(turns: usize) -> Value {
    let mut messages = vec![json!({"role": "user", "content": "Build the project."})];
    for turn in 0..turns {
        messages.push(json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": format!("toolu_{turn}"), "name": "bash",
             "input": {"command": "make"}}
        ]}));
        messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": format!("toolu_{turn}"), "content": "ok"}
        ]}));
    }
    json!({
        "model": "claude-opus-5-5",
        "max_tokens": 32000,
        "thinking": {"type": "adaptive"},
        "system": [{"type": "text", "text": "You are an agent.",
                    "cache_control": {"type": "ephemeral"}}],
        "messages": messages,
    })
}

fn answer(stop_reason: &str) -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-opus-5-5",
        "content": [{"type": "tool_use", "id": "toolu_x", "name": "bash", "input": {"command": "make"}}],
        "stop_reason": stop_reason,
        "usage": {"input_tokens": 3, "cache_read_input_tokens": 4000,
                  "cache_creation_input_tokens": 200, "output_tokens": 40}
    })
}

/// The upstream: pings are answered as a cache read, everything else with
/// `stop_reason`.
async fn answering(harness: &Harness, stop_reason: &str, ping_reads: u32) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({"max_tokens": 1})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_ping", "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": "o"}], "stop_reason": "max_tokens",
            "usage": {"input_tokens": 3, "cache_read_input_tokens": ping_reads,
                      "cache_creation_input_tokens": 0, "output_tokens": 1}
        })))
        .with_priority(1)
        .mount(&harness.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(answer(stop_reason)))
        .with_priority(2)
        .mount(&harness.server)
        .await;
}

async fn pings(harness: &Harness) -> Vec<Value> {
    harness
        .upstream_json()
        .await
        .into_iter()
        .filter(|body| body["max_tokens"] == 1)
        .collect()
}

#[tokio::test]
async fn a_tool_use_stop_is_kept_warm_until_the_cap_with_the_request_the_provider_cached() {
    let harness = keeping_warm(3).await;
    answering(&harness, "tool_use", 4000).await;
    let body = conversation(1);
    harness.post("/v1/messages", &body).await;

    tokio::time::sleep(INTERVAL * 6).await;
    let sent = harness.upstream_json().await;
    let pings = pings(&harness).await;
    assert_eq!(pings.len(), 3, "stops at the cap");
    for ping in &pings {
        let mut expected = sent[0].clone();
        expected["max_tokens"] = json!(1);
        expected["stream"] = json!(false);
        assert_eq!(
            ping, &expected,
            "the request the provider cached, made cheap"
        );
    }
    assert_eq!(
        harness.state.keepwarm().armed(),
        0,
        "a timer that ran out is gone"
    );

    let rows = proxy_rows(&harness.events);
    let warm: Vec<_> = rows.iter().filter(|row| row.keep_warm).collect();
    assert_eq!(warm.len(), 3, "every ping is in the bill");
    let client = rows
        .iter()
        .find(|row| !row.keep_warm)
        .expect("the client's row");
    for (n, row) in warm.iter().enumerate() {
        assert_eq!(
            row.request_id,
            format!("{}.keepwarm{}", client.request_id, n + 1)
        );
        assert_eq!(
            row.conversation, client.conversation,
            "filed to its conversation"
        );
        assert_eq!(row.cache_read_tokens, Some(4000));
        assert_eq!(row.completion_tokens, Some(1));
        assert_eq!(row.status, Some(200));
    }
}

#[tokio::test]
async fn a_turn_that_ended_is_not_kept_warm() {
    let harness = keeping_warm(3).await;
    answering(&harness, "end_turn", 4000).await;
    harness.post("/v1/messages", &conversation(1)).await;
    tokio::time::sleep(INTERVAL * 3).await;
    assert!(pings(&harness).await.is_empty());
    assert_eq!(harness.state.keepwarm().armed(), 0);
}

#[tokio::test]
async fn it_is_off_unless_asked_for() {
    let harness = Harness::with(|config| {
        config.proxy.cache_keep_warm_interval = INTERVAL;
    })
    .await;
    answering(&harness, "tool_use", 4000).await;
    harness.post("/v1/messages", &conversation(1)).await;
    tokio::time::sleep(INTERVAL * 3).await;
    assert!(pings(&harness).await.is_empty());
}

#[tokio::test]
async fn the_clients_next_request_stops_the_pings() {
    let harness = keeping_warm(20).await;
    answering(&harness, "tool_use", 4000).await;
    harness.post("/v1/messages", &conversation(1)).await;
    tokio::time::sleep(INTERVAL + INTERVAL / 2).await;
    assert_eq!(pings(&harness).await.len(), 1, "one interval, one ping");

    // The client comes back with the tool result; this answer ends the turn,
    // so nothing is armed again.
    harness.server.reset().await;
    answering(&harness, "end_turn", 4000).await;
    harness.post("/v1/messages", &conversation(2)).await;
    tokio::time::sleep(INTERVAL * 3).await;
    assert!(
        pings(&harness).await.is_empty(),
        "no ping after the client's request"
    );
    assert_eq!(harness.state.keepwarm().armed(), 0);
}

#[tokio::test]
async fn a_ping_that_finds_the_cache_gone_is_the_last() {
    let harness = keeping_warm(5).await;
    answering(&harness, "tool_use", 0).await;
    harness.post("/v1/messages", &conversation(1)).await;
    tokio::time::sleep(INTERVAL * 4).await;
    assert_eq!(
        pings(&harness).await.len(),
        1,
        "a ping that read nothing wrote the prefix; another would write it again"
    );
}

#[tokio::test]
async fn a_one_hour_request_is_not_kept_warm() {
    let harness = Harness::with(|config| {
        config.proxy.cache_keep_warm = true;
        config.proxy.cache_keep_warm_interval = INTERVAL;
        config.proxy.cache_ttl = kompressor_core::config::CacheTtl::OneHour;
    })
    .await;
    answering(&harness, "tool_use", 4000).await;
    harness.post("/v1/messages", &conversation(1)).await;
    tokio::time::sleep(INTERVAL * 3).await;
    assert!(pings(&harness).await.is_empty());
}

#[tokio::test]
async fn it_can_be_turned_on_for_anthropic_alone() {
    let harness = Harness::with(|config| {
        config.proxy.cache_keep_warm_interval = INTERVAL;
        config.proxy.providers.insert(
            "anthropic".into(),
            kompressor_core::config::ProviderOverrides {
                cache_keep_warm: Some(true),
                ..Default::default()
            },
        );
    })
    .await;
    answering(&harness, "tool_use", 4000).await;
    harness.post("/v1/messages", &conversation(1)).await;
    tokio::time::sleep(INTERVAL + INTERVAL / 2).await;
    assert_eq!(pings(&harness).await.len(), 1);
}

#[tokio::test]
async fn a_streamed_tool_use_stop_is_kept_warm() {
    let harness = keeping_warm(1).await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({"max_tokens": 1})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "type": "message", "content": [], "stop_reason": "max_tokens",
            "usage": {"input_tokens": 3, "cache_read_input_tokens": 4000, "output_tokens": 1}
        })))
        .with_priority(1)
        .mount(&harness.server)
        .await;
    let stream = [
        r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"usage":{"input_tokens":3,"cache_read_input_tokens":4000,"cache_creation_input_tokens":200,"output_tokens":1}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_x","name":"bash","input":{}}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"make\"}"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":40}}"#,
        r#"{"type":"message_stop"}"#,
    ]
    .iter()
    .map(|data| {
        let event = serde_json::from_str::<Value>(data).unwrap()["type"]
            .as_str()
            .unwrap()
            .to_string();
        format!("event: {event}\ndata: {data}\n\n")
    })
    .collect::<String>();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .with_priority(2)
        .mount(&harness.server)
        .await;
    let mut body = conversation(1);
    body["stream"] = json!(true);
    let reply = harness.post("/v1/messages", &body).await;
    assert!(reply.text().contains("tool_use"));
    tokio::time::sleep(INTERVAL * 3).await;
    let pings = pings(&harness).await;
    assert_eq!(pings.len(), 1);
    assert_eq!(pings[0]["stream"], false);
}

#[tokio::test]
async fn a_prompt_too_short_to_cache_or_a_failed_answer_is_not_kept_warm() {
    // Too short: under the smallest prompt the provider caches.
    let short = keeping_warm(3).await;
    let mut tiny = answer("tool_use");
    tiny["usage"] = json!({"input_tokens": 300, "cache_read_input_tokens": 0,
                           "cache_creation_input_tokens": 0, "output_tokens": 5});
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(tiny))
        .mount(&short.server)
        .await;
    short.post("/v1/messages", &conversation(1)).await;

    // Failed: the provider's error is relayed and nothing is armed.
    let failed = keeping_warm(3).await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(529).set_body_json(json!({
            "type": "error", "error": {"type": "overloaded_error", "message": "busy"},
            "stop_reason": "tool_use"
        })))
        .mount(&failed.server)
        .await;
    failed.post("/v1/messages", &conversation(1)).await;

    tokio::time::sleep(INTERVAL * 3).await;
    for harness in [&short, &failed] {
        assert!(pings(harness).await.is_empty());
        assert_eq!(harness.state.keepwarm().armed(), 0);
    }
}

#[tokio::test]
async fn a_conversation_has_one_timer_and_it_pings_the_latest_request() {
    let harness = keeping_warm(20).await;
    answering(&harness, "tool_use", 4000).await;
    harness.post("/v1/messages", &conversation(1)).await;
    harness.post("/v1/messages", &conversation(2)).await;
    assert_eq!(
        harness.state.keepwarm().armed(),
        1,
        "the second replaced the first"
    );

    tokio::time::sleep(INTERVAL + INTERVAL / 2).await;
    let pings = pings(&harness).await;
    assert_eq!(pings.len(), 1, "one timer, one ping per interval");
    assert_eq!(
        pings[0]["messages"].as_array().map(Vec::len),
        Some(5),
        "the ping is the request the provider cached last"
    );
}

/// A conversation whose agent put a job in the background, and — when
/// `reported` — has since been told it finished.
fn with_background_task(reported: bool) -> Value {
    let mut body = conversation(1);
    let messages = body["messages"].as_array_mut().unwrap();
    messages.push(json!({"role": "assistant", "content": [
        {"type": "tool_use", "id": "toolu_bg", "name": "bash",
         "input": {"command": "make train", "run_in_background": true}}
    ]}));
    messages.push(json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "toolu_bg",
         "content": "Command running in background with ID: b2s048qdc. You will be notified when it completes."}
    ]}));
    if reported {
        messages.push(json!({"role": "user", "content": [{"type": "text", "text":
            "[SYSTEM NOTIFICATION - NOT USER INPUT]\n<task-notification>\n<task-id>b2s048qdc</task-id>\n<status>completed</status>"}]}));
    }
    body
}

async fn keeping_background_warm(background_pings: u32) -> Harness {
    Harness::with(|config| {
        config.proxy.cache_keep_warm = true;
        config.proxy.cache_keep_warm_interval = INTERVAL;
        config.proxy.cache_keep_warm_max_pings = 5;
        config.proxy.cache_keep_warm_background_pings = background_pings;
    })
    .await
}

#[tokio::test]
async fn an_end_turn_with_a_background_task_running_is_kept_warm_under_its_own_cap() {
    let harness = keeping_background_warm(2).await;
    answering(&harness, "end_turn", 4000).await;
    harness
        .post("/v1/messages", &with_background_task(false))
        .await;
    tokio::time::sleep(INTERVAL * 5).await;
    assert_eq!(
        pings(&harness).await.len(),
        2,
        "the background cap, not the tool_use one"
    );
    assert_eq!(harness.state.keepwarm().armed(), 0);
}

#[tokio::test]
async fn an_end_turn_after_the_task_was_reported_is_not_kept_warm() {
    let harness = keeping_background_warm(2).await;
    answering(&harness, "end_turn", 4000).await;
    harness
        .post("/v1/messages", &with_background_task(true))
        .await;
    tokio::time::sleep(INTERVAL * 3).await;
    assert!(pings(&harness).await.is_empty());
}

#[tokio::test]
async fn background_waits_can_be_left_out() {
    let harness = keeping_background_warm(0).await;
    answering(&harness, "end_turn", 4000).await;
    harness
        .post("/v1/messages", &with_background_task(false))
        .await;
    tokio::time::sleep(INTERVAL * 3).await;
    assert!(pings(&harness).await.is_empty());
}

#[tokio::test]
async fn a_tool_use_stop_keeps_its_own_cap_with_a_background_task_running() {
    let harness = keeping_background_warm(1).await;
    answering(&harness, "tool_use", 4000).await;
    harness
        .post("/v1/messages", &with_background_task(false))
        .await;
    tokio::time::sleep(INTERVAL * 7).await;
    assert_eq!(
        pings(&harness).await.len(),
        5,
        "tool_use waits are not shortened"
    );
}

#[tokio::test]
async fn the_background_cap_never_exceeds_the_overall_one() {
    let harness = Harness::with(|config| {
        config.proxy.cache_keep_warm = true;
        config.proxy.cache_keep_warm_interval = INTERVAL;
        config.proxy.cache_keep_warm_max_pings = 1;
        config.proxy.cache_keep_warm_background_pings = 4;
    })
    .await;
    answering(&harness, "end_turn", 4000).await;
    harness
        .post("/v1/messages", &with_background_task(false))
        .await;
    tokio::time::sleep(INTERVAL * 6).await;
    assert_eq!(pings(&harness).await.len(), 1);
}

#[tokio::test]
async fn the_clients_next_request_ends_a_background_wait_too() {
    let harness = keeping_background_warm(10).await;
    answering(&harness, "end_turn", 4000).await;
    harness
        .post("/v1/messages", &with_background_task(false))
        .await;
    tokio::time::sleep(INTERVAL + INTERVAL / 2).await;
    assert_eq!(pings(&harness).await.len(), 1);

    // The notification arrives: the task is reported, and this turn ends.
    harness.server.reset().await;
    answering(&harness, "end_turn", 4000).await;
    harness
        .post("/v1/messages", &with_background_task(true))
        .await;
    tokio::time::sleep(INTERVAL * 3).await;
    assert!(pings(&harness).await.is_empty());
    assert_eq!(harness.state.keepwarm().armed(), 0);
}

#[tokio::test]
async fn a_short_prompt_is_not_kept_warm_for_a_background_task_either() {
    let harness = keeping_background_warm(2).await;
    let mut tiny = answer("end_turn");
    tiny["usage"] = json!({"input_tokens": 300, "output_tokens": 5});
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(tiny))
        .mount(&harness.server)
        .await;
    harness
        .post("/v1/messages", &with_background_task(false))
        .await;
    tokio::time::sleep(INTERVAL * 3).await;
    assert!(pings(&harness).await.is_empty());
}

#[tokio::test]
async fn a_streamed_end_turn_with_a_background_task_running_is_kept_warm() {
    let harness = keeping_background_warm(1).await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({"max_tokens": 1})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "type": "message", "content": [], "stop_reason": "max_tokens",
            "usage": {"input_tokens": 3, "cache_read_input_tokens": 4000, "output_tokens": 1}
        })))
        .with_priority(1)
        .mount(&harness.server)
        .await;
    let stream = [
        r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"usage":{"input_tokens":3,"cache_read_input_tokens":4000,"cache_creation_input_tokens":200,"output_tokens":1}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Waiting on the build."}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":6}}"#,
        r#"{"type":"message_stop"}"#,
    ]
    .iter()
    .map(|data| {
        let event = serde_json::from_str::<Value>(data).unwrap()["type"]
            .as_str()
            .unwrap()
            .to_string();
        format!("event: {event}\ndata: {data}\n\n")
    })
    .collect::<String>();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .with_priority(2)
        .mount(&harness.server)
        .await;
    let mut body = with_background_task(false);
    body["stream"] = json!(true);
    let reply = harness.post("/v1/messages", &body).await;
    assert!(reply.text().contains("Waiting on the build."));
    tokio::time::sleep(INTERVAL * 3).await;
    assert_eq!(pings(&harness).await.len(), 1);
}
