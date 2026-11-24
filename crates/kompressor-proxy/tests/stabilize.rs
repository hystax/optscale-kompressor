//! `proxy.tools_order`, `proxy.sticky_beta` and `proxy.prompt_cache_key`: what
//! each does to a conversation that drifts, and — as much the point — what
//! none of them does to one that does not.

mod support;

use serde_json::{Value, json};
use wiremock::matchers::{any, body_string_contains};
use wiremock::{Mock, ResponseTemplate};

use kompressor_core::config::{
    Config, PromptCacheKey, ProviderOverrides, RetrieveTool, ToolsOrder,
};
use kompressor_stats::context::DRIFT_TOOL_ORDER;
use support::{Harness, anthropic_body, proxy_rows};

const CHAT: &str = "/v1/chat/completions";
const MESSAGES: &str = "/v1/messages";
const KEY: &str = "prompt_cache_key";

fn answered() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "chatcmpl-1",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "done"},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 900, "completion_tokens": 12}
    }))
}

async fn answering(harness: &Harness) {
    Mock::given(any())
        .respond_with(answered())
        .mount(&harness.server)
        .await;
}

fn tool(name: &str) -> Value {
    json!({"type": "function", "function": {
        "name": name, "description": format!("the {name} tool"),
        "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
    }})
}

/// A short chat conversation of `turns` exchanges, with `tools` in that order.
fn chat(turns: usize, tools: &[&str]) -> Value {
    let mut messages = vec![
        json!({"role": "system", "content": "You are a careful assistant."}),
        json!({"role": "user", "content": "Where is the bug?"}),
    ];
    for turn in 0..turns {
        messages.push(json!({"role": "assistant", "content": format!("answer {turn}")}));
        messages.push(json!({"role": "user", "content": format!("and then {turn}?")}));
    }
    json!({
        "model": "gpt-5.5",
        "messages": messages,
        "tools": tools.iter().map(|name| tool(name)).collect::<Vec<_>>()
    })
}

fn tool_names(body: &Value) -> Vec<&str> {
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str())
                .collect()
        })
        .unwrap_or_default()
}

fn pinned(config: &mut Config) {
    config.proxy.retrieve_tool = RetrieveTool::Never;
    config.proxy.tools_order = ToolsOrder::Pinned;
}

#[tokio::test]
async fn reordered_tools_go_upstream_in_the_conversations_first_order() {
    let harness = Harness::with(pinned).await;
    answering(&harness).await;

    let first = chat(0, &["read", "bash", "grep"]);
    harness.post(CHAT, &first).await;
    harness
        .post(CHAT, &chat(1, &["grep", "read", "bash"]))
        .await;
    // A new tool, put first by the client, goes after the ones already sent.
    harness
        .post(CHAT, &chat(2, &["edit", "bash", "grep", "read"]))
        .await;

    let sent = harness.upstream_json().await;
    assert_eq!(
        harness.upstream_bodies().await[0],
        first.to_string(),
        "the first request is not touched, byte for byte"
    );
    assert_eq!(tool_names(&sent[1]), ["read", "bash", "grep"]);
    assert_eq!(tool_names(&sent[2]), ["read", "bash", "grep", "edit"]);
    assert_eq!(
        serde_json::to_string(&sent[1]["tools"]).unwrap(),
        serde_json::to_string(&sent[0]["tools"]).unwrap(),
        "the tool block the provider cached is the one it is sent again"
    );
    // Only the tools moved.
    assert_eq!(sent[1]["messages"], chat(1, &[])["messages"]);

    // The statistics still say the client reordered, and that nothing changed
    // on the way out because of it.
    let rows = proxy_rows(&harness.events);
    let second = rows[1].prefix.clone().expect("observed");
    assert_eq!(second.drift_in, DRIFT_TOOL_ORDER);
    assert_eq!(second.drift_out, 0);
    assert_eq!(
        second.repaired, DRIFT_TOOL_ORDER,
        "put back before it went upstream, so no loss is laid at its door"
    );
}

#[tokio::test]
async fn a_client_that_keeps_its_order_is_never_rewritten() {
    let harness = Harness::with(pinned).await;
    answering(&harness).await;
    let bodies = [
        chat(0, &["read", "bash"]),
        chat(1, &["read", "bash"]),
        chat(2, &["read", "bash", "edit"]),
    ];
    for body in &bodies {
        harness.post(CHAT, body).await;
    }
    let sent = harness.upstream_bodies().await;
    for (sent, body) in sent.iter().zip(&bodies) {
        assert_eq!(*sent, body.to_string(), "the bytes that arrived");
    }
}

#[tokio::test]
async fn by_default_the_clients_order_is_the_order() {
    let harness = Harness::with(|config| config.proxy.retrieve_tool = RetrieveTool::Never).await;
    answering(&harness).await;
    harness.post(CHAT, &chat(0, &["read", "bash"])).await;
    let reordered = chat(1, &["bash", "read"]);
    harness.post(CHAT, &reordered).await;
    assert_eq!(harness.upstream_bodies().await[1], reordered.to_string());
}

/// The retrieve tool this proxy adds is not the client's to reorder: it stays
/// last, and the pinned order is of the client's tools alone.
#[tokio::test]
async fn pinning_leaves_the_retrieve_tool_last() {
    let harness = Harness::with(|config| config.proxy.tools_order = ToolsOrder::Pinned).await;
    answering(&harness).await;
    harness.post(CHAT, &chat(0, &["read", "bash"])).await;
    harness.post(CHAT, &chat(1, &["bash", "read"])).await;
    let sent = harness.upstream_json().await;
    let ours = "optscale_kompressor_retrieve";
    assert_eq!(tool_names(&sent[0]), ["read", "bash", ours]);
    assert_eq!(tool_names(&sent[1]), ["read", "bash", ours]);
}

fn anthropic_answered() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5",
        "content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    }))
}

async fn betas_sent(harness: &Harness) -> Vec<Vec<String>> {
    harness
        .server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|request| {
            request
                .headers
                .get_all("anthropic-beta")
                .iter()
                .map(|value| value.to_str().unwrap().to_string())
                .collect()
        })
        .collect()
}

fn sticky(config: &mut Config) {
    config.proxy.providers.insert(
        "anthropic".to_string(),
        ProviderOverrides {
            sticky_beta: Some(true),
            ..Default::default()
        },
    );
}

#[tokio::test]
async fn a_beta_token_the_conversation_sent_before_is_sent_again() {
    let harness = Harness::with(sticky).await;
    Mock::given(any())
        .respond_with(anthropic_answered())
        .mount(&harness.server)
        .await;
    let body = anthropic_body("short");
    let beta = |value| [("anthropic-beta", value)];
    harness
        .post_with(MESSAGES, &body, &beta("tools-2024,context-1m"))
        .await;
    harness
        .post_with(MESSAGES, &body, &beta("context-1m"))
        .await;
    harness.post(MESSAGES, &body).await;
    // Another conversation never sent them, and is not given them.
    let mut other = anthropic_body("short");
    other["messages"][0]["content"] = json!("A different question entirely.");
    harness
        .post_with(MESSAGES, &other, &beta("context-1m"))
        .await;

    assert_eq!(
        betas_sent(&harness).await,
        [
            vec!["tools-2024,context-1m".to_string()],
            vec!["tools-2024,context-1m".to_string()],
            vec!["tools-2024,context-1m".to_string()],
            vec!["context-1m".to_string()],
        ]
    );
}

/// The client did drop a token, and the provider never saw it dropped: the
/// statistics record the first and do not blame it for anything.
#[tokio::test]
async fn a_token_put_back_is_recorded_as_repaired() {
    let harness = Harness::with(sticky).await;
    Mock::given(any())
        .respond_with(anthropic_answered())
        .mount(&harness.server)
        .await;
    let body = anthropic_body("short");
    harness
        .post_with(
            MESSAGES,
            &body,
            &[("anthropic-beta", "tools-2024,context-1m")],
        )
        .await;
    harness
        .post_with(MESSAGES, &body, &[("anthropic-beta", "context-1m")])
        .await;
    let rows = proxy_rows(&harness.events);
    let seen = rows[1].prefix.clone().expect("observed");
    assert_eq!(seen.drift_in, kompressor_stats::context::DRIFT_BETA);
    assert_eq!(seen.repaired, kompressor_stats::context::DRIFT_BETA);
    assert_eq!(seen.drift_out, 0);
}

#[tokio::test]
async fn by_default_the_beta_header_is_the_clients() {
    let harness = Harness::new().await;
    Mock::given(any())
        .respond_with(anthropic_answered())
        .mount(&harness.server)
        .await;
    let body = anthropic_body("short");
    harness
        .post_with(
            MESSAGES,
            &body,
            &[("anthropic-beta", "tools-2024,context-1m")],
        )
        .await;
    harness
        .post_with(MESSAGES, &body, &[("anthropic-beta", "context-1m")])
        .await;
    harness.post(MESSAGES, &body).await;
    let sent = betas_sent(&harness).await;
    assert_eq!(sent[1], ["context-1m"]);
    assert!(sent[2].is_empty());
}

/// The OpenAI upstream, with the conversation key on.
fn keyed(config: &mut Config) {
    config.proxy.upstreams[0].name = "openai".to_string();
    config.proxy.retrieve_tool = RetrieveTool::Never;
    config.proxy.providers.insert(
        "openai".to_string(),
        ProviderOverrides {
            prompt_cache_key: Some(PromptCacheKey::Conversation),
            ..Default::default()
        },
    );
}

#[tokio::test]
async fn every_turn_of_a_conversation_names_the_same_cache_route() {
    let harness = Harness::with(keyed).await;
    answering(&harness).await;
    harness.post(CHAT, &chat(0, &["read"])).await;
    harness.post(CHAT, &chat(3, &["read"])).await;
    let mut other = chat(0, &["read"]);
    other["messages"][1]["content"] = json!("A different question entirely.");
    harness.post(CHAT, &other).await;

    let sent = harness.upstream_json().await;
    let key = sent[0][KEY].as_str().expect("a key was added");
    assert!(key.starts_with("kmp-") && key.len() == 20, "{key}");
    assert_eq!(sent[1][KEY], key, "a later turn is routed with the first");
    assert_ne!(sent[2][KEY], key, "another conversation has its own");
    // Nothing else changed.
    let mut without = sent[1].clone();
    without.as_object_mut().unwrap().remove(KEY);
    assert_eq!(without, chat(3, &["read"]));
}

/// A conversation whose first request is a single message — no system
/// prompt — names the same route on that request as on every later one. A key
/// taken from "the first two messages" changed after the first turn.
#[tokio::test]
async fn a_conversation_that_opens_with_one_message_keeps_its_route() {
    let harness = Harness::with(keyed).await;
    answering(&harness).await;
    let opening = json!({"role": "user", "content": "Where is the bug?"});
    let turn = |messages: Vec<Value>| json!({"model": "gpt-5.5", "messages": messages});
    harness.post(CHAT, &turn(vec![opening.clone()])).await;
    harness
        .post(
            CHAT,
            &turn(vec![
                opening.clone(),
                json!({"role": "assistant", "content": "In the parser."}),
                json!({"role": "user", "content": "Show me."}),
            ]),
        )
        .await;
    let sent = harness.upstream_json().await;
    assert!(sent[0][KEY].is_string());
    assert_eq!(sent[0][KEY], sent[1][KEY]);
}

/// A request that continues a response stored at the provider carries only
/// this turn's items. It has no opening to name a route by, and gets none.
#[tokio::test]
async fn a_request_continuing_a_stored_response_is_given_no_key() {
    let harness = Harness::with(keyed).await;
    answering(&harness).await;
    let body = json!({
        "model": "gpt-5.5",
        "previous_response_id": "resp_123",
        "input": [{"type": "message", "role": "user",
                   "content": [{"type": "input_text", "text": "And the tests?"}]}]
    });
    harness.post("/v1/responses", &body).await;
    assert_eq!(harness.upstream_bodies().await[0], body.to_string());
}

/// An upstream that reports one unknown parameter at a time refuses the
/// second only once the first is gone. Neither was the client's; the request
/// still succeeds, and both are remembered.
#[tokio::test]
async fn two_added_fields_refused_one_at_a_time_still_cost_no_request() {
    use kompressor_core::config::PromptCacheRetention;
    let harness = Harness::with(|config| {
        keyed(config);
        config
            .proxy
            .providers
            .get_mut("openai")
            .expect("set by keyed")
            .prompt_cache_retention = Some(PromptCacheRetention::Extended);
    })
    .await;
    for field in ["prompt_cache_retention", KEY] {
        Mock::given(body_string_contains(field))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": {"message": format!("Unknown parameter: '{field}'."),
                          "type": "invalid_request_error", "param": field}
            })))
            .mount(&harness.server)
            .await;
    }
    answering(&harness).await;

    let reply = harness.post(CHAT, &chat(0, &["read"])).await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    let reply = harness.post(CHAT, &chat(1, &["read"])).await;
    assert_eq!(reply.status, 200);
    let carried: Vec<(bool, bool)> = harness
        .upstream_json()
        .await
        .iter()
        .map(|body| {
            (
                body.get("prompt_cache_retention").is_some(),
                body.get(KEY).is_some(),
            )
        })
        .collect();
    assert_eq!(
        carried,
        [(true, true), (false, true), (false, false), (false, false)],
        "each refusal takes out the field it named; the next request sends neither"
    );
}

/// A second proxy, or this one restarted, names the same route: the key comes
/// from the conversation's opening and from nothing this process remembers.
#[tokio::test]
async fn the_cache_route_is_the_same_from_another_process() {
    let mut keys = Vec::new();
    for _ in 0..2 {
        let harness = Harness::with(keyed).await;
        answering(&harness).await;
        // Seen first mid-conversation, as after a restart.
        harness.post(CHAT, &chat(2, &["read"])).await;
        keys.push(harness.upstream_json().await[0][KEY].clone());
    }
    assert!(keys[0].is_string());
    assert_eq!(keys[0], keys[1]);
}

#[tokio::test]
async fn a_key_the_client_named_is_left_alone() {
    let harness = Harness::with(keyed).await;
    answering(&harness).await;
    let mut body = chat(0, &["read"]);
    body[KEY] = json!("codex-session-7");
    harness.post(CHAT, &body).await;
    assert_eq!(harness.upstream_bodies().await[0], body.to_string());
}

#[tokio::test]
async fn by_default_and_off_openai_no_key_is_added() {
    for configure in [
        (|config: &mut Config| config.proxy.retrieve_tool = RetrieveTool::Never) as fn(&mut Config),
        // On, but the upstream is not OpenAI's own API.
        |config: &mut Config| {
            keyed(config);
            config.proxy.upstreams[0].name = "provider".to_string();
        },
    ] {
        let harness = Harness::with(configure).await;
        answering(&harness).await;
        let body = chat(0, &["read"]);
        harness.post(CHAT, &body).await;
        assert_eq!(harness.upstream_bodies().await[0], body.to_string());
    }
}

#[tokio::test]
async fn a_refused_key_costs_a_round_trip_not_the_request_and_is_remembered() {
    let harness = Harness::with(keyed).await;
    Mock::given(body_string_contains(KEY))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"message": "Unknown parameter: 'prompt_cache_key'.",
                      "type": "invalid_request_error", "param": KEY}
        })))
        .mount(&harness.server)
        .await;
    answering(&harness).await;

    for turns in [0, 1] {
        let reply = harness.post(CHAT, &chat(turns, &["read"])).await;
        assert_eq!(reply.status, 200, "{}", reply.text());
    }
    let carried: Vec<bool> = harness
        .upstream_json()
        .await
        .iter()
        .map(|body| body.get(KEY).is_some())
        .collect();
    assert_eq!(carried, [true, false, false]);
}
