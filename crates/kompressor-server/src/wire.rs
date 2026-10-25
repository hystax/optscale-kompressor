//! Request and response bodies, and the mapping between JSON messages and the
//! engine's view of them.
//!
//! # Why the raw JSON is kept
//!
//! A caller's message carries fields this service has no opinion about —
//! `tool_call_id`, `name`, `annotations`, whatever a harness added last week —
//! and the response is usually forwarded straight to a provider. Rebuilding a
//! message from the engine's `Message` would silently drop every one of them,
//! so the raw object is kept and only the text we changed is written back into
//! it. Everything else comes back byte for byte, including the difference
//! between a string `content` and a one-element block array, which providers
//! treat as equivalent and their prompt caches do not.

use kompressor_core::RetrieveRange;
use kompressor_engine::messages::{Message, MessageContent, MessageMeta, TextSlot};
use kompressor_engine::options::EngineOptionsPatch;
use kompressor_engine::types::Strategy;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::ApiError;

/// `POST /v1/compress`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompressRequest {
    /// The conversation, in the caller's own shape.
    pub messages: Vec<Value>,
    /// Recorded for attribution; nothing routes on it.
    pub model: Option<String>,
    /// Recorded for attribution; nothing routes on it.
    pub provider: Option<String>,
    /// Joins this compression to the retrievals it causes. Generated when absent.
    pub request_id: Option<String>,
    /// Per-request engine overrides.
    pub options: Option<EngineOptionsPatch>,
    /// Messages `[0, n)` are the provider-cached prefix and come back untouched.
    pub frozen_message_count: Option<usize>,
    /// The latest user text, for compressors that want conversation context.
    pub context: Option<String>,
}

/// `POST /v1/compress/raw`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompressRawRequest {
    /// The payload.
    pub content: String,
    /// Skip detection and route as this type: `json`, `log`, `text`, `config`.
    pub content_type: Option<String>,
    /// Per-request engine overrides.
    pub options: Option<EngineOptionsPatch>,
    /// Joins this compression to the retrievals it causes.
    pub request_id: Option<String>,
    /// Conversation context, accepted so both compress endpoints take the same
    /// request shape.
    pub context: Option<String>,
    /// The tool that produced the payload, when the caller knows.
    pub source_tool: Option<String>,
    /// The file it acted on, when it names one.
    pub file_path: Option<String>,
}

/// `POST /v1/retrieve`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetrieveRequest {
    /// The handle from a marker, `k_` prefix included.
    #[serde(rename = "ref")]
    pub reference: String,
    /// Which part of the fragment is wanted. Absent means all of it, capped.
    #[serde(default)]
    pub range: Option<RetrieveRange>,
    /// Joins this retrieval to the compression that minted the handle.
    #[serde(default)]
    pub request_id: Option<String>,
}

/// `POST /v1/detect`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DetectRequest {
    /// The payload to classify.
    pub content: String,
    /// The role it would arrive as; only the size gate reads it.
    pub role: Option<String>,
}

/// `POST /v1/detect` response: what the engine would decide, and why.
#[derive(Clone, Debug, Serialize)]
pub struct DetectResponse {
    /// The detected type, in the detector's vocabulary.
    pub content_type: &'static str,
    /// How sure the detector is, 0.0 to 1.0.
    pub confidence: f32,
    /// The compressor this routes to, with the enable flags applied.
    pub strategy: &'static str,
    /// The analytics name for the type: what `stats.by_type` would count.
    pub kind: &'static str,
    /// The detector's own notes.
    pub meta: kompressor_engine::types::DetectionMeta,
}

/// The name a [`Strategy`] is reported under.
pub fn strategy_name(strategy: Strategy) -> &'static str {
    match strategy {
        Strategy::SmartCrusher => "smart_crusher",
        Strategy::Search => "search",
        Strategy::Log => "log",
        Strategy::Diff => "diff",
        Strategy::CodeAware => "code_aware",
        Strategy::Tabular => "tabular",
        Strategy::Listing => "listing",
        Strategy::Config => "config",
        Strategy::Passthrough => "passthrough",
    }
}

/// Where one compressible run of text lives inside a raw message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotLoc {
    /// `content[i].text`
    Block(usize),
    /// `content[i].content`, a tool result whose body is a plain string.
    ToolResultText(usize),
    /// `content[i].content[j].text`, a tool result made of blocks.
    NestedBlock(usize, usize),
}

/// A conversation held in both shapes at once.
pub struct Conversation {
    raw: Vec<Value>,
    messages: Vec<Message>,
    slots: Vec<Vec<SlotLoc>>,
}

impl Conversation {
    /// Read a caller's messages.
    pub fn parse(raw: Vec<Value>) -> Result<Conversation, ApiError> {
        let mut messages = Vec::with_capacity(raw.len());
        let mut slots = Vec::with_capacity(raw.len());

        for (index, value) in raw.iter().enumerate() {
            let object = value
                .as_object()
                .ok_or_else(|| ApiError::Invalid(format!("messages[{index}] must be an object")))?;
            let role = object
                .get("role")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ApiError::Invalid(format!("messages[{index}] is missing a string `role`"))
                })?
                .to_string();

            let (content, located) = read_content(object);
            messages.push(Message {
                role,
                content,
                meta: read_meta(object),
            });
            slots.push(located);
        }

        Ok(Conversation {
            raw,
            messages,
            slots,
        })
    }

    /// The engine's view, for [`kompressor_core::AppCore::compress`].
    pub fn messages_mut(&mut self) -> &mut [Message] {
        &mut self.messages
    }

    /// How many messages there are.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Whether the conversation is empty.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Write whatever the engine changed back into the caller's own objects.
    pub fn into_json(mut self) -> Vec<Value> {
        for (index, message) in self.messages.into_iter().enumerate() {
            let Some(raw) = self.raw.get_mut(index) else {
                continue;
            };
            match message.content {
                MessageContent::Text(text) => {
                    if let Some(object) = raw.as_object_mut() {
                        object.insert("content".to_string(), Value::String(text));
                    }
                }
                MessageContent::Blocks(slots) => {
                    let locations = self.slots.get(index).cloned().unwrap_or_default();
                    for (slot, location) in slots.into_iter().zip(locations) {
                        write_slot(raw, location, slot.text);
                    }
                }
                // Nothing the engine could read, so nothing to write back: the
                // original bytes stand.
                MessageContent::Opaque => {}
            }
        }
        self.raw
    }
}

/// Read a message's content in whichever of the three shapes it arrived in.
fn read_content(object: &Map<String, Value>) -> (MessageContent, Vec<SlotLoc>) {
    match object.get("content") {
        Some(Value::String(text)) => (MessageContent::Text(text.clone()), Vec::new()),
        Some(Value::Array(items)) => {
            let (slots, locations) = read_blocks(items);
            if slots.is_empty() {
                // An array of images, or of tool calls: readable by the
                // provider, not by us.
                (MessageContent::Opaque, Vec::new())
            } else {
                (MessageContent::Blocks(slots), locations)
            }
        }
        // `null`, a number, an object, or no content at all. All of them
        // round-trip untouched.
        _ => (MessageContent::Opaque, Vec::new()),
    }
}

fn read_blocks(items: &[Value]) -> (Vec<TextSlot>, Vec<SlotLoc>) {
    let mut slots = Vec::new();
    let mut locations = Vec::new();

    for (index, item) in items.iter().enumerate() {
        let Some(block) = item.as_object() else {
            continue;
        };
        let kind = block.get("type").and_then(Value::as_str);
        match kind {
            Some("tool_result") => match block.get("content") {
                Some(Value::String(text)) => {
                    slots.push(slot(text, block_is_protected(block)));
                    locations.push(SlotLoc::ToolResultText(index));
                }
                Some(Value::Array(inner)) => {
                    for (inner_index, nested) in inner.iter().enumerate() {
                        let Some(nested) = nested.as_object() else {
                            continue;
                        };
                        if let Some(Value::String(text)) = nested.get("text") {
                            let protected = block_is_protected(block) || block_is_protected(nested);
                            slots.push(slot(text, protected));
                            locations.push(SlotLoc::NestedBlock(index, inner_index));
                        }
                    }
                }
                _ => {}
            },
            // `{type: "text", text: "…"}`, and the same shape with the type
            // left off, which several clients send.
            //
            // `input_text` and `output_text` are the Responses family's names
            // for the same thing, and the proxy's own walker has always read
            // them (`openai_responses.rs`). This surface did not, so a caller
            // posting the shape codex actually sends — a tool result as
            // `[{"type":"input_text",...}]` — got HTTP 200 with
            // `tokens_in: 0` and nothing compressed. Measured against a live
            // proxy: `text` blocks compressed 15,147 tokens to 1,138 over 26
            // handles, and the identical payload as `input_text` compressed
            // nothing at all.
            Some("text") | Some("input_text") | Some("output_text") | None => {
                if let Some(Value::String(text)) = block.get("text") {
                    slots.push(slot(text, block_is_protected(block)));
                    locations.push(SlotLoc::Block(index));
                }
            }
            _ => {}
        }
    }

    (slots, locations)
}

fn slot(text: &str, protected: bool) -> TextSlot {
    TextSlot {
        text: text.to_string(),
        protected,
        // This surface has no caller that can tell a live provider cache from
        // a lapsed one — the proxy is the only one that watches what the
        // provider reports — so protection here is unconditional.
        protection: kompressor_engine::messages::Protection::Always,
    }
}

/// Whether a block's exact bytes are load-bearing.
///
/// A `cache_control` block is a prompt-cache breakpoint: rewriting its text
/// moves the boundary and invalidates every cached token after it, which costs
/// far more than the compression saves. A `signature` is a provider-verified
/// hash of the block, and content that no longer matches it is rejected
/// outright. Neither is negotiable by an engine setting.
fn block_is_protected(block: &Map<String, Value>) -> bool {
    let flagged = block
        .get("protected")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    flagged
        || matches!(block.get("cache_control"), Some(value) if !value.is_null())
        || matches!(block.get("signature"), Some(value) if !value.is_null())
}

fn read_meta(object: &Map<String, Value>) -> MessageMeta {
    let role = object.get("role").and_then(Value::as_str).unwrap_or("");
    MessageMeta {
        cache_control: matches!(object.get("cache_control"), Some(value) if !value.is_null()),
        protected: object
            .get("protected")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        // `name` means the author on a user or system turn and the tool on a
        // tool turn, so it is only read as a tool name where it means one.
        tool_name: object
            .get("tool_name")
            .and_then(Value::as_str)
            .or_else(|| {
                role.eq_ignore_ascii_case("tool")
                    .then(|| object.get("name").and_then(Value::as_str))
                    .flatten()
            })
            .map(str::to_string),
        file_path: object
            .get("file_path")
            .and_then(Value::as_str)
            .map(str::to_string),
        // A message API names no provider, so it is priced as any-prefix.
        ..MessageMeta::default()
    }
}

fn write_slot(raw: &mut Value, location: SlotLoc, text: String) {
    let Some(content) = raw.get_mut("content") else {
        return;
    };
    match location {
        SlotLoc::Block(index) => {
            if let Some(block) = content.get_mut(index).and_then(Value::as_object_mut) {
                block.insert("text".to_string(), Value::String(text));
            }
        }
        SlotLoc::ToolResultText(index) => {
            if let Some(block) = content.get_mut(index).and_then(Value::as_object_mut) {
                block.insert("content".to_string(), Value::String(text));
            }
        }
        SlotLoc::NestedBlock(index, inner) => {
            if let Some(block) = content
                .get_mut(index)
                .and_then(|block| block.get_mut("content"))
                .and_then(|inner_list| inner_list.get_mut(inner))
                .and_then(Value::as_object_mut)
            {
                block.insert("text".to_string(), Value::String(text));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Conversation {
        let messages: Vec<Value> = serde_json::from_str(json).unwrap();
        Conversation::parse(messages).unwrap()
    }

    #[test]
    fn the_responses_spellings_of_a_text_block_are_read_too() {
        // `input_text` and `output_text` are what the Responses family calls a
        // text block, and they are what codex sends for a tool result. The
        // proxy's own walker has always read them; this surface did not, so an
        // MCP or API caller posting that shape got HTTP 200 and
        // `tokens_in: 0` — accepted and silently ignored, with nothing to say
        // it had happened.
        //
        // Measured against a live proxy before this arm existed: the same
        // 43,159-byte payload compressed 15,147 tokens to 1,138 over 26
        // handles as `{"type":"text"}`, and compressed nothing at all as
        // `{"type":"input_text"}`.
        for spelling in ["text", "input_text", "output_text"] {
            let conversation = parse(&format!(
                r#"[{{"role":"tool","content":[{{"type":"{spelling}","text":"the payload"}}]}}]"#
            ));
            let seen: Vec<String> = conversation
                .messages
                .iter()
                .map(|m| m.content.joined_text())
                .collect();
            assert_eq!(
                seen,
                vec!["the payload".to_string()],
                "a {spelling} block has to reach the engine"
            );
        }
    }

    #[test]
    fn a_string_content_becomes_text_and_comes_back_as_a_string() {
        let mut conversation = parse(r#"[{"role":"user","content":"hello"}]"#);
        assert!(matches!(
            conversation.messages_mut()[0].content,
            MessageContent::Text(_)
        ));
        let back = conversation.into_json();
        assert_eq!(back[0]["content"], Value::String("hello".into()));
    }

    #[test]
    fn unknown_message_fields_survive_the_round_trip() {
        let original = r#"[{"role":"tool","content":"x","tool_call_id":"call_1","name":"Bash","extra":{"nested":[1,2]}}]"#;
        let conversation = parse(original);
        let back = conversation.into_json();
        assert_eq!(back[0]["tool_call_id"], "call_1");
        assert_eq!(back[0]["extra"]["nested"][1], 2);
    }

    #[test]
    fn a_null_or_absent_content_is_opaque_rather_than_an_error() {
        let conversation = parse(r#"[{"role":"assistant","content":null},{"role":"assistant"}]"#);
        assert!(conversation.messages[0].content.is_opaque());
        assert!(conversation.messages[1].content.is_opaque());
        let back = conversation.into_json();
        assert_eq!(back[0]["content"], Value::Null);
        assert!(back[1].get("content").is_none(), "nothing is invented");
    }

    #[test]
    fn text_blocks_become_one_slot_each_and_write_back_in_place() {
        let mut conversation = parse(
            r#"[{"role":"user","content":[{"type":"text","text":"first"},{"type":"image","source":{}},{"type":"text","text":"second"}]}]"#,
        );
        let MessageContent::Blocks(slots) = &mut conversation.messages_mut()[0].content else {
            panic!("blocks");
        };
        assert_eq!(slots.len(), 2, "the image is not a slot");
        slots[1].text = "SECOND".to_string();

        let back = conversation.into_json();
        assert_eq!(back[0]["content"][0]["text"], "first");
        assert_eq!(back[0]["content"][1]["type"], "image", "untouched");
        assert_eq!(back[0]["content"][2]["text"], "SECOND");
    }

    #[test]
    fn a_cache_breakpoint_block_arrives_protected() {
        // rewriting it would move the prompt-cache boundary, which costs more
        // than the compression saves
        let conversation = parse(
            r#"[{"role":"user","content":[{"type":"text","text":"pinned","cache_control":{"type":"ephemeral"}},{"type":"text","text":"free"}]}]"#,
        );
        let MessageContent::Blocks(slots) = &conversation.messages[0].content else {
            panic!("blocks");
        };
        assert!(slots[0].protected);
        assert!(!slots[1].protected);
    }

    #[test]
    fn a_signed_block_is_protected_too() {
        // the provider verifies the signature against these exact bytes, so a
        // rewritten block is rejected outright rather than merely uncached
        let conversation = parse(
            r#"[{"role":"assistant","content":[{"type":"text","text":"reasoned","signature":"abc"}]}]"#,
        );
        let MessageContent::Blocks(slots) = &conversation.messages[0].content else {
            panic!("blocks");
        };
        assert!(slots[0].protected);
    }

    #[test]
    fn a_tool_result_is_read_as_a_string_body_or_as_nested_blocks() {
        let mut conversation = parse(
            r#"[{"role":"user","content":[
                 {"type":"tool_result","tool_use_id":"t1","content":"plain body"},
                 {"type":"tool_result","tool_use_id":"t2","content":[{"type":"text","text":"nested body"}]}
               ]}]"#,
        );
        let MessageContent::Blocks(slots) = &mut conversation.messages_mut()[0].content else {
            panic!("blocks");
        };
        assert_eq!(slots.len(), 2);
        slots[0].text = "A".to_string();
        slots[1].text = "B".to_string();

        let back = conversation.into_json();
        assert_eq!(back[0]["content"][0]["content"], "A");
        assert_eq!(back[0]["content"][0]["tool_use_id"], "t1");
        assert_eq!(back[0]["content"][1]["content"][0]["text"], "B");
    }

    #[test]
    fn a_content_array_with_nothing_readable_is_opaque() {
        let conversation = parse(r#"[{"role":"user","content":[{"type":"image","source":{}}]}]"#);
        assert!(conversation.messages[0].content.is_opaque());
    }

    #[test]
    fn a_message_that_is_not_an_object_or_has_no_role_is_refused() {
        assert!(Conversation::parse(vec![Value::String("hi".into())]).is_err());
        assert!(Conversation::parse(vec![serde_json::json!({"content": "hi"})]).is_err());
    }

    #[test]
    fn the_tool_name_is_only_read_from_name_on_a_tool_turn() {
        let conversation = parse(
            r#"[{"role":"user","content":"x","name":"vlad"},{"role":"tool","content":"x","name":"Read","file_path":"/tmp/a"}]"#,
        );
        assert_eq!(conversation.messages[0].meta.tool_name, None);
        assert_eq!(
            conversation.messages[1].meta.tool_name.as_deref(),
            Some("Read")
        );
        assert_eq!(
            conversation.messages[1].meta.file_path.as_deref(),
            Some("/tmp/a")
        );
    }
}
