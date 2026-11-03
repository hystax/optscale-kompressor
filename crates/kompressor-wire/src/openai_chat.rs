//! OpenAI Chat Completions: `POST /v1/chat/completions`.
//!
//! The conversation is the top-level `messages` array. A message's `content` is
//! either one string or a list of typed parts, and only `text` parts are ours —
//! an `image_url`, `input_audio` or `file` part is a reference the provider
//! resolves, and rewriting one would at best waste tokens and at worst break
//! the request.
//!
//! The prize here is the `tool` role. A `role:"tool"` message carries the raw
//! output of a call — a file, a test log, an API response — and that is where
//! the bytes in an agent conversation actually are. The matching assistant
//! turn's `tool_calls` are *never* touched: their `arguments` are a JSON string
//! the provider parses, so a compressor that shortened one would change what
//! the tool was asked to do.

use serde_json::{Map, Value};

use kompressor_engine::messages::{MessageMeta, TextSlot};

use crate::path::SlotPath;
use crate::tools::{ToolCall, ToolCallIndex, file_path_in_json_arguments};
use crate::{ApiFamily, Walk, WalkBuilder, Walker, model_of};

/// Walks `/v1/chat/completions` bodies.
pub struct OpenAiChatWalker;

impl Walker for OpenAiChatWalker {
    fn family(&self) -> ApiFamily {
        ApiFamily::OpenAiChat
    }

    fn walk(&self, body: &Value) -> Option<Walk> {
        let messages = body.as_object()?.get("messages")?.as_array()?;
        let calls = index_tool_calls(messages);
        let mut builder = WalkBuilder::new();

        for (index, message) in messages.iter().enumerate() {
            // a non-object entry, or one without a role, is not a shape we can
            // reason about; the caller relays the request rather than guessing
            let message = message.as_object()?;
            let role = message.get("role")?.as_str()?;
            let mut meta = meta_for(message, role, &calls);
            // Billed, never readable: the arguments of the calls an assistant
            // turn made. Counted for promotion pricing only.
            meta.unseen_tokens = message
                .get("tool_calls")
                .filter(|calls| !calls.is_null())
                .map_or(0, |calls| {
                    kompressor_engine::estimate_tokens(&calls.to_string(), "tool")
                });
            let here = SlotPath::root().key("messages").index(index);

            match message.get("content") {
                Some(Value::String(text)) => {
                    builder.push_text(role, meta, text, here.key("content"));
                }
                Some(Value::Array(parts)) => {
                    let mut slots = Vec::new();
                    for (position, part) in parts.iter().enumerate() {
                        let part = part.as_object()?;
                        if part.get("type").and_then(Value::as_str) != Some("text") {
                            continue;
                        }
                        let Some(text) = part.get("text").and_then(Value::as_str) else {
                            continue;
                        };
                        slots.push((
                            TextSlot::new(text),
                            here.clone().key("content").index(position).key("text"),
                        ));
                    }
                    builder.push_blocks(role, meta, slots);
                }
                // `content: null` is how an assistant turn that only calls
                // tools arrives, and a content shape we do not know is treated
                // the same way: present in the conversation, not ours to read
                _ => builder.push_opaque(role, meta),
            }
        }

        // OpenAI marks no cached prefix in the request, so nothing in the body
        // can tell us where one ends. Whether to freeze a prefix anyway is an
        // operator policy (`proxy.openai_frozen_prefix`) applied by the caller,
        // not something this walker may invent.
        Some(builder.finish(ApiFamily::OpenAiChat, 0, model_of(body)))
    }
}

/// Every `tool_calls` entry in the request, keyed by the id its output quotes.
fn index_tool_calls(messages: &[Value]) -> ToolCallIndex {
    let mut index = ToolCallIndex::new();
    for message in messages {
        let Some(calls) = message.get("tool_calls").and_then(Value::as_array) else {
            continue;
        };
        for call in calls {
            let Some(id) = call.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(function) = call.get("function") else {
                continue;
            };
            let Some(name) = function.get("name").and_then(Value::as_str) else {
                continue;
            };
            let file_path = function
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(file_path_in_json_arguments);
            index.insert(
                id.to_string(),
                ToolCall {
                    name: name.to_string(),
                    file_path,
                },
            );
        }
    }
    index
}

/// Tool metadata for a `role:"tool"` message whose call is in this request.
fn meta_for(message: &Map<String, Value>, role: &str, calls: &ToolCallIndex) -> MessageMeta {
    if !role.eq_ignore_ascii_case("tool") {
        return MessageMeta::default();
    }
    let Some(call) = message
        .get("tool_call_id")
        .and_then(Value::as_str)
        .and_then(|id| calls.get(id))
    else {
        return MessageMeta::default();
    };
    MessageMeta {
        tool_name: Some(call.name.clone()),
        file_path: call.file_path.clone(),
        ..MessageMeta::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kompressor_engine::messages::MessageContent;
    use serde_json::json;

    /// A turn of a real agent session: a system prompt, a user turn, an
    /// assistant turn that calls a file read, the read's output, and a
    /// multimodal user turn mixing text with an image.
    fn body() -> Value {
        serde_json::from_str(
            r#"{
              "model": "gpt-4o",
              "temperature": 0,
              "max_tokens": 1e5,
              "messages": [
                {"role": "system", "content": "You are a careful engineer."},
                {"role": "user", "content": "Why does the build fail?"},
                {"role": "assistant", "content": null, "tool_calls": [
                  {"id": "call_a1", "type": "function",
                   "function": {"name": "read_file", "arguments": "{\"path\": \"src/build.rs\", \"limit\": 400}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_a1",
                 "content": "error[E0308]: mismatched types\n  --> src/build.rs:14:9\n"},
                {"role": "user", "content": [
                  {"type": "text", "text": "and this screenshot of the CI page"},
                  {"type": "image_url", "image_url": {"url": "https://ci.example/run/9.png"}},
                  {"type": "text", "text": "the second panel is the interesting one"}
                ]}
              ]
            }"#,
        )
        .expect("fixture parses")
    }

    #[test]
    fn every_compressible_slot_is_found_with_the_role_the_wire_gave_it() {
        let walk = OpenAiChatWalker.walk(&body()).expect("known shape");
        let addresses: Vec<String> = walk.origins.iter().map(|o| o.path.to_string()).collect();
        assert_eq!(
            addresses,
            vec![
                "messages[0].content",
                "messages[1].content",
                "messages[3].content",
                "messages[4].content[0].text",
                "messages[4].content[2].text",
            ]
        );
        let roles: Vec<&str> = walk.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["system", "user", "assistant", "tool", "user"]);
        assert_eq!(walk.model.as_deref(), Some("gpt-4o"));
        assert_eq!(walk.frozen_message_count, 0, "no markers in this family");
    }

    #[test]
    fn a_tool_message_carrying_the_call_output_is_reached() {
        let walk = OpenAiChatWalker.walk(&body()).expect("known shape");
        match &walk.messages[3].content {
            MessageContent::Text(text) => assert!(text.starts_with("error[E0308]")),
            other => panic!("tool output not offered: {other:?}"),
        }
    }

    #[test]
    fn an_assistant_turn_that_only_calls_tools_is_opaque_and_its_arguments_are_untouched() {
        let walk = OpenAiChatWalker.walk(&body()).expect("known shape");
        assert!(walk.messages[2].content.is_opaque());
        assert!(
            !walk
                .origins
                .iter()
                .any(|o| o.path.to_string().starts_with("messages[2]")),
            "a tool call's arguments are what the tool was asked to do"
        );
    }

    #[test]
    fn an_image_part_is_never_offered_for_compression() {
        let walk = OpenAiChatWalker.walk(&body()).expect("known shape");
        assert!(
            !walk
                .origins
                .iter()
                .any(|o| o.path.to_string() == "messages[4].content[1].image_url")
        );
        assert!(
            !walk
                .origins
                .iter()
                .any(|o| o.original.contains("ci.example"))
        );
    }

    #[test]
    fn audio_and_file_parts_are_never_offered_for_compression() {
        let body = json!({
          "model": "gpt-4o-audio-preview",
          "messages": [{"role": "user", "content": [
            {"type": "input_audio", "input_audio": {"data": "AAAA", "format": "wav"}},
            {"type": "file", "file": {"file_id": "file-abc"}}
          ]}]
        });
        let walk = OpenAiChatWalker.walk(&body).expect("known shape");
        assert!(walk.origins.is_empty());
        assert!(
            walk.messages[0].content.is_opaque(),
            "a turn with nothing readable is opaque, not an empty block list"
        );
        assert!(walk.is_empty());
    }

    #[test]
    fn the_tool_name_and_file_path_come_from_the_matching_call() {
        let walk = OpenAiChatWalker.walk(&body()).expect("known shape");
        let meta = &walk.messages[3].meta;
        assert_eq!(meta.tool_name.as_deref(), Some("read_file"));
        assert_eq!(meta.file_path.as_deref(), Some("src/build.rs"));
    }

    #[test]
    fn tool_metadata_is_none_when_the_call_is_not_in_this_request() {
        // a trimmed history: the output survived, the call that produced it did not
        let body = json!({
          "model": "gpt-4o",
          "messages": [
            {"role": "tool", "tool_call_id": "call_gone", "content": "stdout: ok"},
            {"role": "user", "content": "continue"}
          ]
        });
        let walk = OpenAiChatWalker.walk(&body).expect("known shape");
        assert_eq!(walk.messages[0].meta.tool_name, None);
        assert_eq!(walk.messages[0].meta.file_path, None);
    }

    #[test]
    fn a_walk_and_an_apply_that_changes_nothing_leave_the_body_byte_identical() {
        let original = body();
        let mut body = original.clone();
        let walk = OpenAiChatWalker.walk(&body).expect("known shape");
        let rewritten = OpenAiChatWalker.apply(&mut body, &walk);
        assert_eq!(rewritten, 0);
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            serde_json::to_string(&original).unwrap()
        );
    }

    #[test]
    fn key_order_and_numeric_literals_survive_the_round_trip() {
        // the 30-digit integer and the 19-digit decimal are the point: without
        // arbitrary_precision both go through an f64 and come back rounded,
        // silently changing a request we were only supposed to shorten
        let source = r#"{"model":"gpt-4o","temperature":0,"max_tokens":100000,"top_p":1.0,"seed":123456789012345678901234567890,"presence_penalty":0.1234567890123456789,"messages":[{"role":"user","content":"hello"}]}"#;
        let mut body: Value = serde_json::from_str(source).expect("fixture parses");
        let mut walk = OpenAiChatWalker.walk(&body).expect("known shape");
        walk.messages[0].content = MessageContent::Text("hi".into());
        OpenAiChatWalker.apply(&mut body, &walk);
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            source.replace("\"hello\"", "\"hi\""),
            "preserve_order and arbitrary_precision keep everything we did not touch"
        );
    }

    #[test]
    fn an_exponent_literal_keeps_its_value_but_not_its_spelling() {
        // The one respelling the round trip allows: serde_json's parser
        // normalises `1e5` to `1e+5`. The value is identical and so is the
        // token stream the provider sees — providers cache on tokens, not on
        // the bytes of the request — so this is recorded rather than fought.
        let mut body: Value = serde_json::from_str(
            r#"{"max_tokens":1e5,"messages":[{"role":"user","content":"x"}]}"#,
        )
        .expect("fixture parses");
        let walk = OpenAiChatWalker.walk(&body).expect("known shape");
        assert_eq!(OpenAiChatWalker.apply(&mut body, &walk), 0);
        assert_eq!(body["max_tokens"].as_f64(), Some(100000.0));
        assert!(
            serde_json::to_string(&body)
                .unwrap()
                .contains("\"max_tokens\":1e+5")
        );
    }

    #[test]
    fn apply_rewrites_only_the_slots_the_engine_changed() {
        let mut body = body();
        let mut walk = OpenAiChatWalker.walk(&body).expect("known shape");
        // the engine compresses the tool output and leaves the two text parts
        walk.messages[3].content = MessageContent::Text("error[E0308] [... 1 line omitted]".into());
        let rewritten = OpenAiChatWalker.apply(&mut body, &walk);
        assert_eq!(rewritten, 1);
        assert_eq!(
            body["messages"][3]["content"],
            json!("error[E0308] [... 1 line omitted]")
        );
        assert_eq!(
            body["messages"][4]["content"][0]["text"],
            json!("and this screenshot of the CI page")
        );
        assert_eq!(
            body["messages"][4]["content"][2]["text"],
            json!("the second panel is the interesting one")
        );
    }

    #[test]
    fn a_string_content_stays_a_string_and_a_block_list_stays_a_block_list() {
        let mut body = body();
        let mut walk = OpenAiChatWalker.walk(&body).expect("known shape");
        walk.messages[1].content = MessageContent::Text("shorter".into());
        if let MessageContent::Blocks(slots) = &mut walk.messages[4].content {
            slots[0].text = "shorter too".into();
        }
        OpenAiChatWalker.apply(&mut body, &walk);
        assert!(
            body["messages"][1]["content"].is_string(),
            "providers treat a string and a one-element block list as equal; caches do not"
        );
        assert!(body["messages"][4]["content"].is_array());
        assert_eq!(
            body["messages"][4]["content"].as_array().unwrap().len(),
            3,
            "the image part is still where it was"
        );
    }

    #[test]
    fn an_unrecognised_body_shape_yields_none_rather_than_a_partial_walk() {
        assert!(
            OpenAiChatWalker
                .walk(&json!({"prompt": "no messages here"}))
                .is_none()
        );
        assert!(
            OpenAiChatWalker
                .walk(&json!({"messages": "not an array"}))
                .is_none()
        );
        assert!(
            OpenAiChatWalker
                .walk(&json!({"messages": [{"role": "user", "content": "ok"}, "stray string"]}))
                .is_none()
        );
        assert!(
            OpenAiChatWalker
                .walk(&json!({"messages": [{"content": "no role"}]}))
                .is_none()
        );
        assert!(OpenAiChatWalker.walk(&json!([])).is_none());
    }
}
