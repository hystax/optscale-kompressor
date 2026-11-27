//! AWS Bedrock Converse: `POST /model/{modelId}/converse` and
//! `/converse-stream`.
//!
//! Claude on Bedrock's `InvokeModel` carries an Anthropic Messages body and is
//! walked by the Anthropic walker; this is the provider-neutral Converse shape.
//!
//! - `system[i].text` is the system prompt.
//! - `messages[i].content[j]` is a block: `{"text"}` is a slot;
//!   `{"toolResult": {"content": [{"text"} | {"json"} | …]}}` holds tool
//!   output, whose `text` entries are slots attributed to the `toolUse` with
//!   the same `toolUseId`; `toolUse`, `image`, `document`, `reasoningContent`
//!   and anything else are left alone.
//! - `{"cachePoint": {…}}` is a cache breakpoint, as Anthropic's
//!   `cache_control` is: everything before the last one is in the provider's
//!   cache. Slots before it in its own message are protected while cached, and
//!   the frozen floor stops before that message when anything follows the
//!   breakpoint in it, after it otherwise.

use std::collections::BTreeMap;

use serde_json::Value;

use kompressor_engine::messages::{MessageMeta, Protection, TextSlot};

use crate::path::SlotPath;
use crate::{ApiFamily, Walk, WalkBuilder, Walker};

/// Walks Converse bodies.
pub struct BedrockConverseWalker;

fn is_cache_point(block: &Value) -> bool {
    block.get("cachePoint").is_some()
}

impl Walker for BedrockConverseWalker {
    fn family(&self) -> ApiFamily {
        ApiFamily::BedrockConverse
    }

    fn walk(&self, body: &Value) -> Option<Walk> {
        let object = body.as_object()?;
        let messages = object.get("messages")?.as_array()?;
        let mut builder = WalkBuilder::new();

        if let Some(system) = object.get("system") {
            let blocks = system.as_array()?;
            let slots = blocks
                .iter()
                .enumerate()
                .filter_map(|(i, block)| {
                    let text = block.get("text")?.as_str()?;
                    Some((
                        TextSlot::new(text),
                        SlotPath::root().key("system").index(i).key("text"),
                    ))
                })
                .collect();
            builder.push_blocks("system", MessageMeta::default(), slots);
        }

        let tool_names = index_tool_uses(messages);
        let last_marked = messages.iter().rposition(|message| {
            message
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| blocks.iter().any(is_cache_point))
        });
        let mut frozen = 0;

        for (index, message) in messages.iter().enumerate() {
            let message = message.as_object()?;
            let role = message.get("role")?.as_str()?;
            let blocks = message.get("content")?.as_array()?;
            let here = SlotPath::root().key("messages").index(index).key("content");
            // Slots before this message's last cache point are inside the
            // cached prefix, and so is every message before the last marked one.
            let marker = blocks.iter().rposition(is_cache_point);
            let mut slots = Vec::new();
            let mut names = Vec::new();
            for (j, block) in blocks.iter().enumerate() {
                let cached = marker.is_some_and(|m| j < m);
                let mut push = |text: &str, path: SlotPath| {
                    let mut slot = TextSlot::new(text);
                    if cached {
                        slot.protected = true;
                        slot.protection = Protection::WhileCached;
                    }
                    slots.push((slot, path));
                };
                if let Some(text) = block.get("text") {
                    push(text.as_str()?, here.clone().index(j).key("text"));
                } else if let Some(result) = block.get("toolResult") {
                    if let Some(name) = result
                        .get("toolUseId")
                        .and_then(Value::as_str)
                        .and_then(|id| tool_names.get(id))
                    {
                        names.push(name.clone());
                    }
                    for (k, item) in result
                        .get("content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .enumerate()
                    {
                        if let Some(text) = item.get("text").and_then(Value::as_str) {
                            push(
                                text,
                                here.clone()
                                    .index(j)
                                    .key("toolResult")
                                    .key("content")
                                    .index(k)
                                    .key("text"),
                            );
                        }
                    }
                }
            }
            if Some(index) == last_marked {
                let wholly_cached = slots.iter().all(|(slot, _)| slot.protected);
                frozen = builder.next_index() + usize::from(wholly_cached);
            }
            names.dedup();
            let meta = MessageMeta {
                tool_name: (names.len() == 1).then(|| names.remove(0)),
                cache_breakpoint: marker.is_some(),
                ..MessageMeta::default()
            };
            builder.push_blocks(role, meta, slots);
        }
        Some(builder.finish(ApiFamily::BedrockConverse, frozen, None))
    }
}

/// `toolUseId` → tool name, from every assistant `toolUse` block.
fn index_tool_uses(messages: &[Value]) -> BTreeMap<String, String> {
    messages
        .iter()
        .filter_map(|m| m.get("content")?.as_array())
        .flatten()
        .filter_map(|block| {
            let tool_use = block.get("toolUse")?;
            Some((
                tool_use.get("toolUseId")?.as_str()?.to_string(),
                tool_use.get("name")?.as_str()?.to_string(),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body() -> Value {
        json!({
            "system": [{"text": "You are a coding agent."}, {"cachePoint": {"type": "default"}}],
            "messages": [
                {"role": "user", "content": [{"text": "run the tests"}]},
                {"role": "assistant", "content": [
                    {"toolUse": {"toolUseId": "t1", "name": "bash", "input": {"cmd": "make test"}}}]},
                {"role": "user", "content": [
                    {"toolResult": {"toolUseId": "t1", "status": "success",
                        "content": [{"text": "ok 1\nok 2"}, {"json": {"passed": 2}}]}}]}
            ],
            "inferenceConfig": {"maxTokens": 512}
        })
    }

    #[test]
    fn text_and_tool_result_text_are_slots() {
        let walk = BedrockConverseWalker.walk(&body()).expect("known shape");
        let paths: Vec<String> = walk.origins.iter().map(|o| o.path.to_string()).collect();
        assert_eq!(
            paths,
            vec![
                "system[0].text",
                "messages[0].content[0].text",
                "messages[2].content[0].toolResult.content[0].text",
            ]
        );
        assert_eq!(walk.messages[3].meta.tool_name.as_deref(), Some("bash"));
        assert!(
            walk.messages[2].content.is_opaque(),
            "a toolUse turn has no text"
        );
        assert_eq!(walk.frozen_message_count, 0, "no cache point in messages");
    }

    #[test]
    fn a_cache_point_freezes_what_precedes_it() {
        let mut body = body();
        body["messages"][0]["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"cachePoint": {"type": "default"}}));
        let walk = BedrockConverseWalker.walk(&body).unwrap();
        // system is message 0; the marked user turn is message 1, wholly cached
        assert_eq!(walk.frozen_message_count, 2);
        let marked = walk
            .origins
            .iter()
            .find(|o| o.original == "run the tests")
            .unwrap();
        assert!(marked.protected);
        let after = walk
            .origins
            .iter()
            .find(|o| o.original.starts_with("ok 1"))
            .unwrap();
        assert!(!after.protected);
    }

    #[test]
    fn an_unknown_shape_is_not_walked() {
        assert!(
            BedrockConverseWalker
                .walk(&json!({"prompt": "x"}))
                .is_none()
        );
        assert!(
            BedrockConverseWalker
                .walk(&json!({"messages": [{"role": "user", "content": "x"}]}))
                .is_none()
        );
    }
}
