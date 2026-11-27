//! `proxy.tools_defer`, end to end: what goes upstream on the first-party
//! mount, that it is the same on every turn, and where it is not applied.

mod support;

use serde_json::{Value, json};
use support::{Harness, proxy_rows};
use wiremock::matchers::any;
use wiremock::{Mock, ResponseTemplate};

use kompressor_core::config::{Config, RetrieveTool};

const MESSAGES: &str = "/v1/messages";

async fn answering(harness: &Harness) {
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn",
            "usage": {"input_tokens": 12, "output_tokens": 2}
        })))
        .mount(&harness.server)
        .await;
}

fn tool(name: &str) -> Value {
    json!({"name": name, "description": format!("The {name} tool. ").repeat(20),
           "input_schema": {"type": "object", "properties": {"x": {"type": "string"}}}})
}

/// Claude Code's core tools and `extra` MCP tools behind them.
fn tools(extra: usize) -> Value {
    let mut tools = vec![tool("Bash"), tool("Read"), tool("Edit")];
    tools.extend((0..extra).map(|i| tool(&format!("mcp__server__tool_{i}"))));
    Value::Array(tools)
}

fn conversation(turns: usize, extra: usize) -> Value {
    let mut messages = vec![json!({"role": "user", "content": "Where is the bug?"})];
    for turn in 0..turns {
        messages.push(json!({"role": "assistant", "content": format!("answer {turn}")}));
        messages.push(json!({"role": "user", "content": format!("and then {turn}?")}));
    }
    json!({"model": "claude-opus-5-5", "max_tokens": 1024, "tools": tools(extra),
           "messages": messages})
}

/// The mount is named `anthropic`, which is what makes it first-party here.
fn deferring(config: &mut Config) {
    config.proxy.retrieve_tool = RetrieveTool::Never;
    config.proxy.tools_defer = true;
    config.proxy.upstreams[0].name = "anthropic".into();
    config.proxy.upstreams[0].api_family = "anthropic".into();
}

#[tokio::test]
async fn the_catalogue_is_deferred_from_the_first_turn_and_the_resident_tools_never_change() {
    let harness = Harness::with(deferring).await;
    answering(&harness).await;
    // A tool appears on the third turn: it is deferred like the others.
    for (turns, extra) in [(0, 15), (1, 15), (2, 16)] {
        harness.post(MESSAGES, &conversation(turns, extra)).await;
    }
    let sent = harness.upstream_json().await;
    let first = sent[0]["tools"].as_array().unwrap();
    assert_eq!(first.len(), 3 + 15 + 1);
    for core in &first[..3] {
        assert!(core.get("defer_loading").is_none(), "{core}");
    }
    assert!(
        first[3..18]
            .iter()
            .all(|tool| tool["defer_loading"] == true)
    );
    assert_eq!(first[18]["type"], "tool_search_tool_regex_20251119");

    assert_eq!(
        sent[1]["tools"], sent[0]["tools"],
        "byte-identical next turn"
    );
    let third = sent[2]["tools"].as_array().unwrap();
    assert_eq!(third[..3], first[..3], "the rendered prefix did not move");
    assert_eq!(third[18]["defer_loading"], true, "the new tool is deferred");

    for row in proxy_rows(&harness.events) {
        let seen = row.prefix.expect("observed");
        assert!(seen.tools_proxy_deferred >= 15);
        assert!(seen.tools_proxy_deferred_bytes > 15 * 300);
    }
}

#[tokio::test]
async fn a_conversation_first_seen_under_way_is_never_deferred() {
    let harness = Harness::with(deferring).await;
    answering(&harness).await;
    let body = conversation(3, 20);
    harness.post(MESSAGES, &body).await;
    assert_eq!(harness.upstream_bodies().await[0], body.to_string());
}

#[tokio::test]
async fn a_gateway_mount_is_left_alone() {
    let harness = Harness::with(|config| {
        deferring(config);
        config.proxy.upstreams[0].name = "corporate-gateway".into();
    })
    .await;
    answering(&harness).await;
    let body = conversation(0, 20);
    harness.post(MESSAGES, &body).await;
    assert_eq!(harness.upstream_bodies().await[0], body.to_string());
}

#[tokio::test]
async fn by_default_nothing_is_deferred() {
    let harness = Harness::with(|config| {
        deferring(config);
        config.proxy.tools_defer = false;
    })
    .await;
    answering(&harness).await;
    let body = conversation(0, 20);
    harness.post(MESSAGES, &body).await;
    assert_eq!(harness.upstream_bodies().await[0], body.to_string());
}

#[tokio::test]
async fn a_side_request_with_fewer_tools_has_its_search_history_repaired() {
    let harness = Harness::with(deferring).await;
    answering(&harness).await;
    // The opening turn decides the conversation is deferred.
    let opening = conversation(0, 15);
    harness.post(MESSAGES, &opening).await;
    // The same conversation replayed with only the core tools, its transcript
    // carrying a search that loaded an MCP tool this request does not declare.
    let mut side = opening.clone();
    side["tools"] = json!([tool("Bash"), tool("Read"), tool("Edit")]);
    side["messages"].as_array_mut().unwrap().extend([
        json!({"role": "assistant", "content": [
            {"type": "server_tool_use", "id": "srv_1", "name": "tool_search_tool_regex",
             "input": {"pattern": "tool_3"}},
            {"type": "tool_search_tool_result", "tool_use_id": "srv_1",
             "content": {"type": "tool_search_tool_search_result",
                         "tool_references": [{"type": "tool_reference",
                                              "tool_name": "mcp__server__tool_3"}]}},
            {"type": "text", "text": "loaded"}]}),
        json!({"role": "user", "content": "stop?"}),
    ]);
    harness.post(MESSAGES, &side).await;
    let sent = harness.upstream_json().await;
    let replayed = sent[1]["messages"][1]["content"].as_array().unwrap();
    assert_eq!(replayed.len(), 3, "positions kept");
    assert_eq!(replayed[0]["type"], "text");
    assert_eq!(replayed[1]["type"], "text");
    assert_eq!(replayed[2]["text"], "loaded");
}
