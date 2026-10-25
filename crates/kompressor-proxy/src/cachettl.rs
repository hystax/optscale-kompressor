//! Asking Anthropic for one-hour cache entries instead of the default five
//! minutes.
//!
//! The rewrite touches `cache_control` only where the Messages API reads it:
//! the request's top level, `system` blocks, tool definitions, message content
//! blocks and the blocks nested in a `tool_result`. Nowhere else. A
//! `cache_control` key inside a `tool_use` input is the agent's own data — an
//! agent editing API client code writes exactly that — and changing it would
//! rewrite the conversation, not the request.
//!
//! Every marker in the request is upgraded, not some of them. The API requires
//! longer-lived entries to come before shorter ones, and rejects a request
//! whose top-level automatic marker and an explicit marker on the last block
//! disagree; one TTL for the whole request satisfies both rules by
//! construction. The markers themselves are not part of any cache key, so the
//! prefix the provider already holds is still read.

use serde_json::{Map, Value};

/// The TTL value this module writes.
const ONE_HOUR: &str = "1h";

/// Upgrade every `cache_control` marker the Messages API reads to one hour.
/// Returns how many it changed; zero leaves the body byte-identical.
pub(crate) fn extend_cache_ttl(body: &mut Value) -> usize {
    let Some(object) = body.as_object_mut() else {
        return 0;
    };
    let mut changed = upgrade(object);
    if let Some(Value::Array(blocks)) = object.get_mut("system") {
        changed += blocks.iter_mut().map(upgrade_block).sum::<usize>();
    }
    if let Some(Value::Array(tools)) = object.get_mut("tools") {
        changed += tools.iter_mut().map(upgrade_block).sum::<usize>();
    }
    if let Some(Value::Array(messages)) = object.get_mut("messages") {
        for message in messages {
            if let Some(Value::Array(blocks)) = message.get_mut("content") {
                for block in blocks {
                    changed += upgrade_block(block);
                    // A tool result carries blocks of its own; its `content`
                    // is the only nested place a marker is read.
                    if block.get("type").and_then(Value::as_str) == Some("tool_result")
                        && let Some(Value::Array(inner)) = block.get_mut("content")
                    {
                        changed += inner.iter_mut().map(upgrade_block).sum::<usize>();
                    }
                }
            }
        }
    }
    changed
}

fn upgrade_block(block: &mut Value) -> usize {
    block.as_object_mut().map_or(0, upgrade)
}

/// Upgrade the marker on one object, if it carries an ephemeral one.
fn upgrade(object: &mut Map<String, Value>) -> usize {
    let Some(Value::Object(marker)) = object.get_mut("cache_control") else {
        return 0;
    };
    if marker.get("type").and_then(Value::as_str) != Some("ephemeral")
        || marker.get("ttl").and_then(Value::as_str) == Some(ONE_HOUR)
    {
        return 0;
    }
    marker.insert("ttl".to_string(), Value::String(ONE_HOUR.to_string()));
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_marker_the_api_reads_asks_for_an_hour() {
        let mut body = json!({
            "cache_control": {"type": "ephemeral"},
            "system": [{"type": "text", "text": "rules", "cache_control": {"type": "ephemeral"}}],
            "tools": [{"name": "Bash", "input_schema": {}, "cache_control": {"type": "ephemeral", "ttl": "5m"}}],
            "messages": [
                {"role": "user", "content": "plain"},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t", "content": [
                        {"type": "text", "text": "out", "cache_control": {"type": "ephemeral"}}
                    ]},
                    {"type": "text", "text": "next", "cache_control": {"type": "ephemeral"}}
                ]}
            ]
        });
        assert_eq!(extend_cache_ttl(&mut body), 5);
        for marker in [
            &body["cache_control"],
            &body["system"][0]["cache_control"],
            &body["tools"][0]["cache_control"],
            &body["messages"][1]["content"][0]["content"][0]["cache_control"],
            &body["messages"][1]["content"][1]["cache_control"],
        ] {
            assert_eq!(marker["ttl"], "1h", "{marker}");
        }
        assert_eq!(
            extend_cache_ttl(&mut body),
            0,
            "already an hour: nothing to change"
        );
    }

    #[test]
    fn the_agents_own_data_is_never_rewritten() {
        // An agent writing API client code puts `cache_control` in a tool
        // input. That is conversation content, not a marker.
        let mut body = json!({"messages": [{"role": "assistant", "content": [
            {"type": "tool_use", "id": "t", "name": "Write",
             "input": {"content": {"cache_control": {"type": "ephemeral"}}}}
        ]}]});
        let before = body.clone();
        assert_eq!(extend_cache_ttl(&mut body), 0);
        assert_eq!(body, before);
    }

    #[test]
    fn a_body_with_no_markers_is_left_alone() {
        let mut body = json!({"messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(extend_cache_ttl(&mut body), 0);
        let mut not_an_object = json!([1, 2]);
        assert_eq!(extend_cache_ttl(&mut not_an_object), 0);
    }
}
