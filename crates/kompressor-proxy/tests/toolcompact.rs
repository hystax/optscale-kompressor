//! `proxy.tools_compact`, end to end: what goes upstream, that it is the same
//! on every turn, and that a conversation it did not see open is left alone.

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

/// Two tools as a schema generator writes them, one carrying the breakpoint.
fn tools() -> Value {
    json!([
        {"name": "read_file", "description": "Read a file.\n\nUse absolute paths.",
         "input_schema": {
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "ReadFileArguments", "type": "object",
            "properties": {
                "file_path": {"title": "File Path", "type": "string",
                              "description": "The file to read."},
                "limit": {"title": "Limit", "type": "integer", "default": 2000,
                          "examples": [100]}
            },
            "required": ["file_path"]}},
        {"name": "search", "description": "Search the tree.",
         "input_schema": {
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"q": {"title": "Query, as a regular expression", "type": "string"}}},
         "cache_control": {"type": "ephemeral"}}
    ])
}

fn conversation(turns: usize) -> Value {
    let mut messages = vec![json!({"role": "user", "content": "Where is the bug?"})];
    for turn in 0..turns {
        messages.push(json!({"role": "assistant", "content": format!("answer {turn}")}));
        messages.push(json!({"role": "user", "content": format!("and then {turn}?")}));
    }
    json!({"model": "claude-opus-5-5", "max_tokens": 1024, "tools": tools(),
           "messages": messages})
}

fn compacting(config: &mut Config) {
    config.proxy.retrieve_tool = RetrieveTool::Never;
    config.proxy.tools_compact = true;
}

#[tokio::test]
async fn generated_bookkeeping_is_gone_from_the_first_turn_and_the_block_never_changes() {
    let harness = Harness::with(compacting).await;
    answering(&harness).await;
    for turns in 0..3 {
        harness.post(MESSAGES, &conversation(turns)).await;
    }
    let sent = harness.upstream_json().await;
    assert_eq!(
        sent[0]["tools"],
        json!([
            {"name": "read_file", "description": "Read a file.\n\nUse absolute paths.",
             "input_schema": {
                "type": "object",
                "properties": {
                    "file_path": {"type": "string", "description": "The file to read."},
                    "limit": {"type": "integer", "default": 2000, "examples": [100]}
                },
                "required": ["file_path"]}},
            {"name": "search", "description": "Search the tree.",
             "input_schema": {
                "type": "object",
                "properties": {"q": {"title": "Query, as a regular expression", "type": "string"}}},
             "cache_control": {"type": "ephemeral"}}
        ]),
        "descriptions, examples, defaults, a title that says something and the \
         breakpoint are all where they were"
    );
    let block = serde_json::to_string(&sent[0]["tools"]).unwrap();
    for (later, turns) in sent[1..].iter().zip(1..) {
        assert_eq!(serde_json::to_string(&later["tools"]).unwrap(), block);
        assert_eq!(later["messages"], conversation(turns)["messages"]);
    }

    let rows = proxy_rows(&harness.events);
    for row in &rows {
        let seen = row.prefix.as_ref().expect("observed");
        assert!(
            seen.tools_compacted_bytes > 100,
            "{}",
            seen.tools_compacted_bytes
        );
        assert_eq!(seen.drift_out, 0, "the same bytes every turn is not drift");
    }
}

/// The provider already holds this conversation's tools as the client wrote
/// them. Compacting them now would lose the cache from the first tool on.
#[tokio::test]
async fn a_conversation_first_seen_under_way_is_forwarded_as_it_arrives() {
    let harness = Harness::with(compacting).await;
    answering(&harness).await;
    let bodies = [conversation(2), conversation(3)];
    for body in &bodies {
        harness.post(MESSAGES, body).await;
    }
    for (sent, body) in harness.upstream_bodies().await.iter().zip(&bodies) {
        assert_eq!(*sent, body.to_string(), "the bytes that arrived");
    }
    assert!(proxy_rows(&harness.events).iter().all(|row| {
        row.prefix
            .as_ref()
            .is_some_and(|seen| seen.tools_compacted_bytes == 0)
    }));
}

#[tokio::test]
async fn by_default_tool_definitions_go_upstream_as_written() {
    let harness = Harness::with(|config| config.proxy.retrieve_tool = RetrieveTool::Never).await;
    answering(&harness).await;
    let body = conversation(0);
    harness.post(MESSAGES, &body).await;
    assert_eq!(harness.upstream_bodies().await[0], body.to_string());
}

/// Tools with nothing to take are not re-serialized: the request is the
/// client's own bytes, compaction on or off.
#[tokio::test]
async fn tools_with_nothing_to_take_leave_the_request_untouched() {
    let harness = Harness::with(compacting).await;
    answering(&harness).await;
    let mut body = conversation(0);
    body["tools"] = json!([{"name": "read_file", "description": "Read a file.",
        "input_schema": {"type": "object", "properties": {"file_path": {"type": "string"}}}}]);
    harness.post(MESSAGES, &body).await;
    assert_eq!(harness.upstream_bodies().await[0], body.to_string());
}

/// With the retrieve tool offered, the injected definition is not touched and
/// the client's are compacted around it.
#[tokio::test]
async fn the_injected_retrieve_tool_is_left_as_written() {
    let harness = Harness::with(|config| config.proxy.tools_compact = true).await;
    answering(&harness).await;
    harness.post(MESSAGES, &conversation(0)).await;
    harness.post(MESSAGES, &conversation(1)).await;
    let sent = harness.upstream_json().await;
    let names: Vec<&str> = sent[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(
        names,
        ["read_file", "search", "optscale_kompressor_retrieve"]
    );
    assert!(sent[0]["tools"][0]["input_schema"].get("$schema").is_none());
    assert_eq!(sent[0]["tools"], sent[1]["tools"]);
}

/// A schema whose meaning depends on its dialect goes upstream still saying
/// which one, and a Markdown description that says more than the plain one
/// goes with it; the tool beside it, which depends on neither, is compacted.
#[tokio::test]
async fn a_schema_that_depends_on_its_dialect_keeps_its_declaration_upstream() {
    let harness = Harness::with(compacting).await;
    answering(&harness).await;
    let draft7 = "http://json-schema.org/draft-07/schema#";
    let pair = json!({
        "$schema": draft7, "type": "object",
        "properties": {
            "range": {"type": "array",
                      "items": [{"type": "integer"}, {"type": "integer"}],
                      "additionalItems": false,
                      "description": "First and last line.",
                      "markdownDescription": "First and last line. **Both 1-based.**"}
        }
    });
    let plain = json!({
        "$schema": draft7, "type": "object",
        "properties": {"path": {"type": "string", "description": "The file.",
                                "markdownDescription": "The `file`."}}
    });
    let mut body = conversation(0);
    body["tools"] = json!([
        {"name": "slice", "description": "Read a range.", "input_schema": pair},
        {"name": "open", "description": "Open a file.", "input_schema": plain}
    ]);
    harness.post(MESSAGES, &body).await;
    let sent = harness.upstream_json().await;
    assert_eq!(
        sent[0]["tools"][0], body["tools"][0],
        "a tuple under draft-07 is nothing under 2020-12: untouched"
    );
    assert_eq!(
        sent[0]["tools"][1]["input_schema"],
        json!({"type": "object",
               "properties": {"path": {"type": "string", "description": "The file."}}})
    );
}
