//! Gemini `generateContent`: Google AI (`/v1beta/models/{m}:generateContent`)
//! and Vertex AI (`…/publishers/google/models/{m}:generateContent`), and their
//! `:streamGenerateContent` forms. One body shape for all four.
//!
//! - `systemInstruction.parts[i].text` is the system prompt.
//! - `contents[i]` is a turn: `role` is `user` or `model` (reported as
//!   `assistant`, so the engine's role rules apply as on the other families),
//!   or the older `function`, reported as `tool`.
//! - A part's `text` is a slot. A part marked `thought: true`, or carrying a
//!   `thoughtSignature`, is protected: the signature is checked against it.
//! - A `functionResponse` part is where tool output lives. Its `response` is a
//!   free-form object; the conventional string fields (`output`, `content`,
//!   `result`, `text`, `stdout`) are slots, attributed to the function's
//!   `name`. Anything else in it is the tool's structured data and is left as
//!   it is.
//! - `inlineData`, `fileData`, `functionCall`, `executableCode` and
//!   `codeExecutionResult` are references or structure, never rewritten.
//!
//! Headroom's Gemini handler forwards every turn holding a non-text part
//! verbatim, so tool output is never compressed there; reaching it is the
//! point of walking `functionResponse` here.
//!
//! There are no cache breakpoints in the body: Gemini's explicit cache is a
//! separate `cachedContent` resource the request names, whose contents are not
//! in this body at all, and its implicit cache is a plain prefix match, which
//! `proxy.frozen_prefix = "first_seen"` handles as it does for OpenAI.

use serde_json::{Map, Value};

use kompressor_engine::messages::{MessageMeta, TextSlot};

use crate::path::SlotPath;
use crate::{ApiFamily, Walk, WalkBuilder, Walker};

/// The string fields of a `functionResponse.response` read as tool output.
const RESPONSE_TEXT_FIELDS: [&str; 5] = ["output", "content", "result", "text", "stdout"];

/// Walks `generateContent` bodies.
pub struct GeminiWalker;

impl Walker for GeminiWalker {
    fn family(&self) -> ApiFamily {
        ApiFamily::GeminiGenerateContent
    }

    fn walk(&self, body: &Value) -> Option<Walk> {
        let object = body.as_object()?;
        let contents = object.get("contents")?.as_array()?;
        let mut builder = WalkBuilder::new();

        if let Some(system) = object
            .get("systemInstruction")
            .or_else(|| object.get("system_instruction"))
        {
            let key = if object.contains_key("systemInstruction") {
                "systemInstruction"
            } else {
                "system_instruction"
            };
            let parts = system.as_object()?.get("parts")?.as_array()?;
            let here = SlotPath::root().key(key).key("parts");
            let slots = text_parts(parts, &here)?;
            builder.push_blocks("system", MessageMeta::default(), slots);
        }

        for (index, content) in contents.iter().enumerate() {
            let content = content.as_object()?;
            let role = match content
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user")
            {
                "model" => "assistant",
                "function" => "tool",
                other => other,
            };
            let parts = content.get("parts")?.as_array()?;
            let here = SlotPath::root().key("contents").index(index).key("parts");
            let slots = text_parts(parts, &here)?;
            builder.push_blocks(role, meta_for(parts), slots);
        }
        // The model is in the path, not the body; the proxy fills it in.
        Some(builder.finish(ApiFamily::GeminiGenerateContent, 0, None))
    }
}

/// The function a turn's tool output came from, when it names exactly one.
fn meta_for(parts: &[Value]) -> MessageMeta {
    let mut names = parts.iter().filter_map(|part| {
        part.get("functionResponse")
            .and_then(|response| response.get("name"))
            .and_then(Value::as_str)
    });
    let first = names.next();
    let tool_name = match (first, names.next()) {
        (Some(name), None) => Some(name.to_string()),
        _ => None,
    };
    MessageMeta {
        tool_name,
        ..MessageMeta::default()
    }
}

/// Every readable string in a list of parts, with its address. `None` for a
/// list that is not a list of objects: not a body this walker understands.
fn text_parts(parts: &[Value], here: &SlotPath) -> Option<Vec<(TextSlot, SlotPath)>> {
    let mut slots = Vec::new();
    for (j, part) in parts.iter().enumerate() {
        let part = part.as_object()?;
        let at = here.clone().index(j);
        if let Some(text) = part.get("text") {
            // A non-string `text` is a shape we do not know: not ours to guess.
            let text = text.as_str()?;
            let mut slot = TextSlot::new(text);
            slot.protected = is_thought(part);
            slots.push((slot, at.key("text")));
            continue;
        }
        if let Some(response) = part
            .get("functionResponse")
            .and_then(|r| r.get("response"))
            .and_then(Value::as_object)
        {
            slots.extend(response_slots(
                response,
                at.key("functionResponse").key("response"),
            ));
        }
    }
    Some(slots)
}

fn is_thought(part: &Map<String, Value>) -> bool {
    part.get("thought").and_then(Value::as_bool) == Some(true)
        || part.contains_key("thoughtSignature")
        || part.contains_key("thought_signature")
}

fn response_slots(response: &Map<String, Value>, here: SlotPath) -> Vec<(TextSlot, SlotPath)> {
    RESPONSE_TEXT_FIELDS
        .iter()
        .filter_map(|field| {
            let text = response.get(*field)?.as_str()?;
            Some((TextSlot::new(text), here.clone().key(field)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kompressor_engine::messages::MessageContent;
    use serde_json::json;

    fn body() -> Value {
        json!({
            "systemInstruction": {"parts": [{"text": "You are Gemini CLI."}]},
            "contents": [
                {"role": "user", "parts": [{"text": "read main.rs"}]},
                {"role": "model", "parts": [
                    {"text": "thinking...", "thought": true},
                    {"functionCall": {"name": "read_file", "args": {"path": "main.rs"}},
                     "thoughtSignature": "sig"}
                ]},
                {"role": "user", "parts": [
                    {"functionResponse": {"name": "read_file",
                        "response": {"output": "fn main() {}", "exit_code": 0}}}
                ]},
                {"role": "user", "parts": [
                    {"inlineData": {"mimeType": "image/png", "data": "AAAA"}},
                    {"text": "and this screenshot"}
                ]}
            ],
            "generationConfig": {"temperature": 0.2}
        })
    }

    #[test]
    fn every_readable_slot_is_found_with_its_role() {
        let walk = GeminiWalker.walk(&body()).expect("known shape");
        let paths: Vec<String> = walk.origins.iter().map(|o| o.path.to_string()).collect();
        assert_eq!(
            paths,
            vec![
                "systemInstruction.parts[0].text",
                "contents[0].parts[0].text",
                "contents[1].parts[0].text",
                "contents[2].parts[0].functionResponse.response.output",
                "contents[3].parts[1].text",
            ]
        );
        let roles: Vec<&str> = walk.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "user", "user"]);
        assert_eq!(
            walk.messages[3].meta.tool_name.as_deref(),
            Some("read_file")
        );
        assert_eq!(walk.family, ApiFamily::GeminiGenerateContent);
        assert_eq!(walk.frozen_message_count, 0);
    }

    #[test]
    fn a_thought_is_protected() {
        let walk = GeminiWalker.walk(&body()).expect("known shape");
        let thought = walk
            .origins
            .iter()
            .find(|o| o.original == "thinking...")
            .unwrap();
        assert!(thought.protected);
    }

    #[test]
    fn a_rewritten_function_response_lands_in_place_and_nothing_else_moves() {
        let mut body = body();
        let before = body.clone();
        let mut walk = GeminiWalker.walk(&body).unwrap();
        let MessageContent::Blocks(slots) = &mut walk.messages[3].content else {
            panic!("blocks");
        };
        slots[0].text = "fn main() { /* elided */ }".into();
        assert_eq!(GeminiWalker.apply(&mut body, &walk), 1);
        assert_eq!(
            body["contents"][2]["parts"][0]["functionResponse"]["response"]["output"],
            "fn main() { /* elided */ }"
        );
        body["contents"][2]["parts"][0]["functionResponse"]["response"]["output"] =
            before["contents"][2]["parts"][0]["functionResponse"]["response"]["output"].clone();
        assert_eq!(body, before, "every other byte is where it was");
    }

    #[test]
    fn an_unknown_shape_is_not_walked() {
        assert!(GeminiWalker.walk(&json!({"prompt": "hi"})).is_none());
        assert!(
            GeminiWalker
                .walk(&json!({"contents": [{"parts": "text"}]}))
                .is_none()
        );
        assert!(
            GeminiWalker
                .walk(&json!({"contents": [{"parts": [{"text": 3}]}]}))
                .is_none()
        );
    }

    #[test]
    fn an_image_only_turn_is_opaque() {
        let walk = GeminiWalker
            .walk(&json!({"contents": [{"role": "user", "parts": [
                {"inlineData": {"mimeType": "image/png", "data": "AAAA"}}]}]}))
            .unwrap();
        assert!(walk.messages[0].content.is_opaque());
    }
}
