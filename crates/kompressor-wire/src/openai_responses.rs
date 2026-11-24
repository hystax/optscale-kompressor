//! OpenAI Responses: `POST /v1/responses`.
//!
//! The conversation is `input`, which is either one string — the whole user
//! turn — or a list of typed items. Only two item shapes carry text we may
//! touch: a `message` and a tool output (`function_call_output`,
//! `custom_tool_call_output`). `reasoning`, `function_call`, `item_reference`,
//! images and files are relayed as they arrived, and so is top-level
//! `instructions`, which is the system prompt under another name.
//!
//! # `previous_response_id`: tool outputs yes, turns no
//!
//! A request that quotes a previous response id is the visible tail of a
//! conversation whose earlier turns live on the provider's side, where we
//! cannot see or rewrite them. The hazard is a prompt that disagrees with
//! itself: if this request *re-sends* something the history also holds, the
//! model sees the full-fidelity copy and an elided one, with an omission marker
//! pointing at content the history contradicts.
//!
//! That needs a duplicate, and only some items can be one. Everything echoed
//! back from a previous response — `reasoning`, `function_call`,
//! `custom_tool_call`, `item_reference`, assistant turns — is already relayed
//! untouched, so it cannot produce the contradiction. What remains is:
//!
//! * **A tool output is safe.** The client produced it by running a tool
//!   *after* the response being quoted, so by construction it is not in that
//!   history. Compressing it cannot contradict anything.
//! * **A `message` is not obviously safe.** A client is free to replay its own
//!   earlier turns alongside a `previous_response_id` — forking a conversation
//!   does exactly that — and from here a replayed turn and a new one look the
//!   same. So under a previous response id, messages are left alone.
//!
//! The trade is measured rather than assumed. On a captured codex websocket
//! session, 17 of 18 requests carried a previous response id, and refusing all
//! of them declined 607,193 characters the engine compresses by 86.6%. Of the
//! request bytes, 85.7% were tool output and 4.1% were messages — so keeping
//! the guard exactly where the hazard is arguable costs almost nothing and
//! recovers nearly everything.
//!
//! A request whose items are *all* declined still yields a walk carrying the
//! model and the family, so the caller accounts for it and forwards it whole.

use serde_json::{Map, Value};

use kompressor_engine::messages::{MessageMeta, TextSlot};

use crate::path::SlotPath;
use crate::tools::{ToolCall, ToolCallIndex, file_path_in_json_arguments};
use crate::{ApiFamily, Walk, WalkBuilder, Walker, model_of};

/// The role a tool output is presented to the engine under. The wire has no
/// role for these items; `tool` is what the engine's role policy already knows.
const TOOL_ROLE: &str = "tool";

/// Walks `/v1/responses` bodies.
pub struct OpenAiResponsesWalker;

impl Walker for OpenAiResponsesWalker {
    fn family(&self) -> ApiFamily {
        ApiFamily::OpenAiResponses
    }

    fn walk(&self, body: &Value) -> Option<Walk> {
        let object = body.as_object()?;
        let input = object.get("input")?;
        let model = model_of(body);

        // Earlier turns live on the provider's side, so anything in this
        // request that might *also* be there must not be rewritten. See the
        // module docs: that narrows to `message` items, not to the whole
        // request.
        let history_elsewhere = object
            .get("previous_response_id")
            .is_some_and(|id| !id.is_null());

        let mut builder = WalkBuilder::new();
        match input {
            Value::String(text) => {
                // A bare string is the whole user turn, which is the one shape
                // that cannot be anything but this request's own content — but
                // it is still a turn, so it follows the same rule as a message.
                if history_elsewhere {
                    return Some(builder.finish(ApiFamily::OpenAiResponses, 0, model));
                }
                builder.push_text(
                    "user",
                    MessageMeta::default(),
                    text,
                    SlotPath::root().key("input"),
                );
            }
            Value::Array(items) => {
                let calls = index_calls(items);
                for (index, item) in items.iter().enumerate() {
                    let item = item.as_object()?;
                    walk_item(&mut builder, item, index, &calls, history_elsewhere)?;
                }
            }
            _ => return None,
        }

        Some(builder.finish(ApiFamily::OpenAiResponses, 0, model))
    }
}

/// One `input` item. Returns `None` only for a shape we cannot classify at all;
/// an item we simply may not touch is skipped without a message.
fn walk_item(
    builder: &mut WalkBuilder,
    item: &Map<String, Value>,
    index: usize,
    calls: &ToolCallIndex,
    history_elsewhere: bool,
) -> Option<()> {
    let here = SlotPath::root().key("input").index(index);
    match item.get("type").and_then(Value::as_str) {
        // a bare `{"role": …, "content": …}` with no `type` is the shorthand
        // the API accepts for a message
        Some("message") | None => {
            let role = item.get("role")?.as_str()?;
            // A turn replayed alongside a `previous_response_id` is
            // indistinguishable from a new one, and rewriting a replay would
            // contradict the copy the provider already holds. Presented to the
            // engine as opaque rather than skipped, so the message still
            // occupies its position — the engine's own indices, and anything
            // that reasons about what came before what, stay honest.
            if history_elsewhere {
                builder.push_opaque(role, MessageMeta::default());
                return Some(());
            }
            match item.get("content") {
                Some(Value::String(text)) => {
                    builder.push_text(role, MessageMeta::default(), text, here.key("content"));
                }
                Some(Value::Array(parts)) => {
                    let slots = text_slots(parts, here.key("content"))?;
                    builder.push_blocks(role, MessageMeta::default(), slots);
                }
                _ => builder.push_opaque(role, MessageMeta::default()),
            }
        }
        Some("function_call_output") | Some("custom_tool_call_output") => {
            let meta = meta_for(item, calls);
            match item.get("output") {
                Some(Value::String(text)) => {
                    builder.push_text(TOOL_ROLE, meta, text, here.key("output"));
                }
                Some(Value::Array(parts)) => {
                    let slots = text_slots(parts, here.key("output"))?;
                    builder.push_blocks(TOOL_ROLE, meta, slots);
                }
                _ => builder.push_opaque(TOOL_ROLE, meta),
            }
        }
        // reasoning, function_call, custom_tool_call, item_reference, the
        // built-in tool calls: all relayed byte for byte, and none of them is a
        // conversation turn we would have anything to say about
        Some(_) => {}
    }
    Some(())
}

/// The text parts of a content or output array.
///
/// `input_text` is what a user turn and a tool output use; `output_text` is
/// what the model's own turns come back as when a client replays them. A part
/// of any other type — `input_image`, `input_file`, a refusal — is not text and
/// is skipped.
fn text_slots(parts: &[Value], base: SlotPath) -> Option<Vec<(TextSlot, SlotPath)>> {
    let mut slots = Vec::new();
    for (position, part) in parts.iter().enumerate() {
        let part = part.as_object()?;
        match part.get("type").and_then(Value::as_str) {
            Some("input_text") | Some("output_text") => {}
            _ => continue,
        }
        let Some(text) = part.get("text").and_then(Value::as_str) else {
            continue;
        };
        slots.push((
            TextSlot::new(text),
            base.clone().index(position).key("text"),
        ));
    }
    Some(slots)
}

/// Every call item in the request, keyed by the `call_id` its output quotes.
fn index_calls(items: &[Value]) -> ToolCallIndex {
    let mut index = ToolCallIndex::new();
    for item in items {
        // a custom tool's arguments arrive in `input` rather than `arguments`,
        // and are free-form text that only sometimes parses as JSON
        let arguments_key = match item.get("type").and_then(Value::as_str) {
            Some("function_call") => "arguments",
            Some("custom_tool_call") => "input",
            _ => continue,
        };
        let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
            continue;
        };
        let Some(name) = item.get("name").and_then(Value::as_str) else {
            continue;
        };
        let file_path = item
            .get(arguments_key)
            .and_then(Value::as_str)
            .and_then(file_path_in_json_arguments);
        index.insert(
            call_id.to_string(),
            ToolCall {
                name: name.to_string(),
                file_path,
                // Not read on this family: its read tools differ by harness.
                read_range: None,
            },
        );
    }
    index
}

/// Tool metadata for an output whose call is in this request.
fn meta_for(item: &Map<String, Value>, calls: &ToolCallIndex) -> MessageMeta {
    let Some(call) = item
        .get("call_id")
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

    /// A Responses turn from an agent harness: instructions, a user message, a
    /// reasoning item, a file-read call and its output, an assistant turn, and
    /// a custom tool call whose output arrives as parts.
    fn body() -> Value {
        serde_json::from_str(
            r#"{
              "model": "gpt-5",
              "instructions": "You are a careful engineer. Never guess.",
              "max_output_tokens": 1e5,
              "input": [
                {"type": "message", "role": "user", "content": [
                  {"type": "input_text", "text": "Why does the build fail?"}
                ]},
                {"type": "reasoning", "id": "rs_1", "summary": [],
                 "encrypted_content": "gAAAAABn0pQ-opaque-blob"},
                {"type": "function_call", "call_id": "call_a1", "name": "read_file",
                 "arguments": "{\"file_path\": \"src/build.rs\"}"},
                {"type": "function_call_output", "call_id": "call_a1",
                 "output": "error[E0308]: mismatched types\n  --> src/build.rs:14:9\n"},
                {"type": "message", "role": "assistant", "content": [
                  {"type": "output_text", "text": "The return type disagrees with the signature."}
                ]},
                {"type": "custom_tool_call", "call_id": "call_b2", "name": "run_tests",
                 "input": "cargo test -p build"},
                {"type": "custom_tool_call_output", "call_id": "call_b2", "output": [
                  {"type": "input_text", "text": "running 41 tests\ntest result: FAILED"}
                ]},
                {"type": "item_reference", "id": "msg_earlier"}
              ]
            }"#,
        )
        .expect("fixture parses")
    }

    #[test]
    fn every_compressible_slot_is_found_with_the_role_the_wire_gave_it() {
        let walk = OpenAiResponsesWalker.walk(&body()).expect("known shape");
        let addresses: Vec<String> = walk.origins.iter().map(|o| o.path.to_string()).collect();
        assert_eq!(
            addresses,
            vec![
                "input[0].content[0].text",
                "input[3].output",
                "input[4].content[0].text",
                "input[6].output[0].text",
            ]
        );
        let roles: Vec<&str> = walk.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "tool", "assistant", "tool"]);
        assert_eq!(walk.model.as_deref(), Some("gpt-5"));
    }

    #[test]
    fn a_function_call_output_is_reached() {
        let walk = OpenAiResponsesWalker.walk(&body()).expect("known shape");
        match &walk.messages[1].content {
            MessageContent::Text(text) => assert!(text.starts_with("error[E0308]")),
            other => panic!("tool output not offered: {other:?}"),
        }
    }

    #[test]
    fn instructions_reasoning_and_call_arguments_are_never_offered_for_compression() {
        let walk = OpenAiResponsesWalker.walk(&body()).expect("known shape");
        for origin in &walk.origins {
            let address = origin.path.to_string();
            assert!(!address.starts_with("instructions"), "{address}");
            for item in ["input[1]", "input[2]", "input[5]", "input[7]"] {
                assert!(!address.starts_with(item), "{address}");
            }
        }
        assert!(
            !walk
                .origins
                .iter()
                .any(|o| o.original.contains("encrypted_content")
                    || o.original.contains("cargo test -p build"))
        );
    }

    #[test]
    fn images_and_files_in_a_user_turn_are_never_offered_for_compression() {
        let body = json!({
          "model": "gpt-5",
          "input": [{"type": "message", "role": "user", "content": [
            {"type": "input_text", "text": "what is in these"},
            {"type": "input_image", "image_url": "https://example/a.png"},
            {"type": "input_file", "file_id": "file-abc"}
          ]}]
        });
        let walk = OpenAiResponsesWalker.walk(&body).expect("known shape");
        assert_eq!(walk.origins.len(), 1);
        assert_eq!(walk.origins[0].path.to_string(), "input[0].content[0].text");
    }

    #[test]
    fn an_input_given_as_a_plain_string_is_one_user_message() {
        let body = json!({"model": "gpt-5", "input": "summarise the failing test output"});
        let walk = OpenAiResponsesWalker.walk(&body).expect("known shape");
        assert_eq!(walk.messages.len(), 1);
        assert_eq!(walk.messages[0].role, "user");
        assert_eq!(walk.origins[0].path.to_string(), "input");
        match &walk.messages[0].content {
            MessageContent::Text(text) => assert_eq!(text, "summarise the failing test output"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_bare_role_and_content_item_is_accepted_as_a_message() {
        let body = json!({"model": "gpt-5", "input": [{"role": "user", "content": "hello"}]});
        let walk = OpenAiResponsesWalker.walk(&body).expect("known shape");
        assert_eq!(walk.origins[0].path.to_string(), "input[0].content");
        assert_eq!(walk.messages[0].role, "user");
    }

    #[test]
    fn a_turn_under_a_previous_response_id_is_left_alone() {
        // A client may replay its own earlier turns alongside a previous
        // response id — forking does — and from here a replay and a new turn
        // look identical. Rewriting a replay would contradict the copy the
        // provider already holds.
        let body = json!({
          "model": "gpt-5",
          "previous_response_id": "resp_68a1f0",
          "input": [{"type": "message", "role": "user", "content": [
            {"type": "input_text", "text": "and now the other file"}
          ]}]
        });
        let walk = OpenAiResponsesWalker.walk(&body).expect("known shape");
        assert!(walk.is_empty(), "nothing compressible");
        assert!(walk.origins.is_empty());
        assert_eq!(
            walk.messages.len(),
            1,
            "the turn still holds its position, as an opaque message"
        );
        assert_eq!(
            walk.model.as_deref(),
            Some("gpt-5"),
            "still worth accounting"
        );
    }

    #[test]
    fn a_tool_output_under_a_previous_response_id_is_compressible() {
        // The narrowing that matters. The client produced this by running a
        // tool *after* the response being quoted, so it cannot be in that
        // history and cannot contradict it. Refusing it declined 607,193
        // characters on one captured codex session — 85.7% of request bytes.
        let body = json!({
          "model": "gpt-5",
          "previous_response_id": "resp_68a1f0",
          "input": [{
            "type": "custom_tool_call_output",
            "call_id": "call_a1",
            "output": [{"type": "input_text", "text": "line one\nline two\nline three\n"}]
          }]
        });
        let walk = OpenAiResponsesWalker.walk(&body).expect("known shape");
        assert!(!walk.is_empty(), "a fresh tool output is ours to compress");
        assert_eq!(walk.origins.len(), 1);
        assert!(!walk.origins[0].protected);
        assert_eq!(walk.origins[0].path.to_string(), "input[0].output[0].text");
    }

    #[test]
    fn a_bare_string_input_under_a_previous_response_id_is_left_alone() {
        // The string form is the whole user turn, so it follows the same rule
        // as a message rather than the tool-output one.
        let body = json!({
          "model": "gpt-5",
          "previous_response_id": "resp_68a1f0",
          "input": "and now the other file"
        });
        let walk = OpenAiResponsesWalker.walk(&body).expect("known shape");
        assert!(walk.is_empty());
        assert!(walk.origins.is_empty());
    }

    #[test]
    fn a_mixed_request_compresses_the_output_and_not_the_turn() {
        let body = json!({
          "model": "gpt-5",
          "previous_response_id": "resp_68a1f0",
          "input": [
            {"type": "message", "role": "user", "content": [
              {"type": "input_text", "text": "this turn may be a replay"}]},
            {"type": "function_call_output", "call_id": "call_a1",
             "output": "fresh output the provider has never seen"}
          ]
        });
        let walk = OpenAiResponsesWalker.walk(&body).expect("known shape");
        assert_eq!(walk.origins.len(), 1, "only the tool output is offered");
        assert_eq!(walk.origins[0].path.to_string(), "input[1].output");
        assert_eq!(walk.messages.len(), 2, "both items keep their positions");
    }

    #[test]
    fn an_explicitly_null_previous_response_id_is_not_a_server_side_history() {
        let body = json!({
          "model": "gpt-5",
          "previous_response_id": null,
          "input": "hello"
        });
        let walk = OpenAiResponsesWalker.walk(&body).expect("known shape");
        assert_eq!(walk.messages.len(), 1);
    }

    #[test]
    fn the_tool_name_and_file_path_come_from_the_matching_call() {
        let walk = OpenAiResponsesWalker.walk(&body()).expect("known shape");
        assert_eq!(
            walk.messages[1].meta.tool_name.as_deref(),
            Some("read_file")
        );
        assert_eq!(
            walk.messages[1].meta.file_path.as_deref(),
            Some("src/build.rs")
        );
        // a custom tool's input is not JSON, so there is no path to recover and
        // none is invented
        assert_eq!(
            walk.messages[3].meta.tool_name.as_deref(),
            Some("run_tests")
        );
        assert_eq!(walk.messages[3].meta.file_path, None);
    }

    #[test]
    fn tool_metadata_is_none_when_the_call_is_not_in_this_request() {
        let body = json!({
          "model": "gpt-5",
          "input": [{"type": "function_call_output", "call_id": "call_gone", "output": "ok"}]
        });
        let walk = OpenAiResponsesWalker.walk(&body).expect("known shape");
        assert_eq!(walk.messages[0].meta.tool_name, None);
        assert_eq!(walk.messages[0].meta.file_path, None);
    }

    #[test]
    fn a_walk_and_an_apply_that_changes_nothing_leave_the_body_byte_identical() {
        let original = body();
        let mut body = original.clone();
        let walk = OpenAiResponsesWalker.walk(&body).expect("known shape");
        assert_eq!(OpenAiResponsesWalker.apply(&mut body, &walk), 0);
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            serde_json::to_string(&original).unwrap()
        );
    }

    #[test]
    fn apply_rewrites_only_the_slots_the_engine_changed() {
        let mut body = body();
        let mut walk = OpenAiResponsesWalker.walk(&body).expect("known shape");
        walk.messages[1].content = MessageContent::Text("error[E0308] [... 1 line omitted]".into());
        assert_eq!(OpenAiResponsesWalker.apply(&mut body, &walk), 1);
        assert_eq!(
            body["input"][3]["output"],
            json!("error[E0308] [... 1 line omitted]")
        );
        assert_eq!(
            body["input"][0]["content"][0]["text"],
            json!("Why does the build fail?")
        );
        assert_eq!(
            body["input"][6]["output"][0]["text"],
            json!("running 41 tests\ntest result: FAILED")
        );
        assert_eq!(
            body["instructions"],
            json!("You are a careful engineer. Never guess.")
        );
    }

    #[test]
    fn an_unrecognised_body_shape_yields_none_rather_than_a_partial_walk() {
        assert!(
            OpenAiResponsesWalker
                .walk(&json!({"model": "gpt-5"}))
                .is_none()
        );
        assert!(OpenAiResponsesWalker.walk(&json!({"input": 42})).is_none());
        assert!(
            OpenAiResponsesWalker
                .walk(&json!({"input": ["a bare string item"]}))
                .is_none()
        );
        assert!(
            OpenAiResponsesWalker
                .walk(&json!({"input": [{"type": "message", "content": "no role"}]}))
                .is_none()
        );
    }
}
