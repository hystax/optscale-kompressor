//! Anthropic Messages: `POST /v1/messages`.
//!
//! The conversation is `messages`; `system` and `tools` are not part of it and
//! are never walked. That is not only a scope decision: both sit at the front
//! of the prompt, which is where callers put their cache breakpoints, so every
//! byte of them is part of a cache key we would be paying to invalidate.
//!
//! # Blocks whose bytes are load-bearing
//!
//! Inside a message, three markers make a block untouchable regardless of role
//! or position, and each is marked as a protected slot rather than dropped —
//! the rest of the message still compresses:
//!
//! - `cache_control`: a prompt-cache breakpoint. Its bytes are in the cache key.
//! - `citations`: character offsets into this exact text point at it from the
//!   model's own output; shortening the text moves what they point at.
//! - `signature`: a cryptographic signature over the block, which a rewrite
//!   invalidates and the provider then rejects.
//!
//! # The frozen floor
//!
//! Prompt caching pins the prefix up to and including the last breakpoint —
//! the last **block**, not the last message containing one. So the floor has
//! two parts. `frozen_message_count` is one past the last message that is
//! *wholly* inside the prefix, and where the prefix ends part-way through a
//! message the floor stops before it and the blocks up to the breakpoint are
//! protected individually, as slots.
//!
//! Freezing the whole message instead is safe and costs real compression: the
//! blocks after a breakpoint are in nobody's cache. Claude Code marks
//! `block[0]` of its final message and puts the tool output after it, so under
//! a message-granular floor every such request recorded `prefix_frozen` and
//! removed nothing at all (docs/CLAUDE-CODE.md). Rewriting a single byte inside that prefix does not
//! fail the request — it silently drops the cache hit rate to zero and raises
//! the bill, which is exactly the kind of regression nobody attributes to the
//! compressor. Markers in `system` or `tools` do not move this floor: they pin
//! a region that precedes the messages, and that region is never touched
//! anyway.

use serde_json::{Map, Value};

use kompressor_engine::messages::{MessageMeta, Protection, TextSlot};
use kompressor_engine::tokens::estimate_tokens;

use crate::path::SlotPath;
use crate::tools::{ToolCall, ToolCallIndex, file_path_in_arguments};
use crate::{ApiFamily, Walk, WalkBuilder, Walker, model_of};

/// Block keys that make a block's exact bytes load-bearing.
const PINNING_KEYS: [&str; 3] = ["cache_control", "citations", "signature"];

/// Walks `/v1/messages` bodies.
pub struct AnthropicMessagesWalker;

impl Walker for AnthropicMessagesWalker {
    fn family(&self) -> ApiFamily {
        ApiFamily::AnthropicMessages
    }

    fn walk(&self, body: &Value) -> Option<Walk> {
        let messages = body.as_object()?.get("messages")?.as_array()?;
        let uses = index_tool_uses(messages);
        let mut builder = WalkBuilder::new();
        let mut frozen_message_count = 0;

        for (index, message) in messages.iter().enumerate() {
            let message = message.as_object()?;
            let role = message.get("role")?.as_str()?;
            let here = SlotPath::root().key("messages").index(index);

            // unlike the OpenAI families, a message here always has content:
            // its absence means we are not looking at a Messages body at all
            match message.get("content")? {
                Value::String(text) => {
                    // One block to the provider's cache lookback, and never a
                    // breakpoint: a plain string has nowhere to carry one.
                    let meta = MessageMeta {
                        cache_blocks: Some(1),
                        ..MessageMeta::default()
                    };
                    builder.push_text(role, meta, text, here.key("content"));
                }
                Value::Array(blocks) => {
                    let marker = cache_marker_position(blocks);
                    let (tool_name, file_path) = tool_metadata(blocks, &uses);
                    let slots = text_slots(blocks, here.key("content"), marker)?;
                    // Whether anything in this message outlives the breakpoint.
                    // When nothing does, the message is wholly inside the cached
                    // prefix and is reported exactly as it always was — the floor
                    // moves past it and the message-level flag is set, so nothing
                    // downstream sees a change. When something does, the floor
                    // stops *before* this message and protection becomes the
                    // slots' own: a message-wide flag there would withhold
                    // precisely the blocks this exists to reach.
                    let wholly_cached =
                        marker.is_some() && slots.iter().all(|(slot, _)| slot.protected);
                    if marker.is_some() {
                        frozen_message_count = builder.next_index() + usize::from(wholly_cached);
                    }
                    let meta = MessageMeta {
                        cache_control: wholly_cached,
                        protected: false,
                        tool_name,
                        file_path,
                        // What the provider's 20-block lookback counts, so the
                        // engine can tell a rewrite it will find a cache entry
                        // behind from one it will not. Earlier turns' thinking
                        // is not counted: with it, three promotions that hit
                        // the cache 22-23 blocks back read as misses; without
                        // it, hits span 17-20 blocks and misses 29-32.
                        cache_blocks: Some(
                            u32::try_from(
                                blocks.iter().filter(|block| !is_thinking(block)).count(),
                            )
                            .unwrap_or(u32::MAX),
                        ),
                        cache_breakpoint: marker.is_some(),
                        unseen_tokens: unseen_tokens(blocks),
                        one_text_block: matches!(blocks.as_slice(), [only]
                            if only.get("type").and_then(Value::as_str) == Some("text")),
                    };
                    builder.push_blocks(role, meta, slots);
                }
                _ => return None,
            }
        }

        Some(builder.finish(
            ApiFamily::AnthropicMessages,
            frozen_message_count,
            model_of(body),
        ))
    }
}

/// Whether a key is present *and* carries a value.
///
/// The API's own responses spell an absent field as `"citations": null`, and
/// clients replay assistant turns verbatim. Treating that as a marker would
/// protect every text block in every replayed conversation and quietly turn
/// compression off.
fn is_set(block: &Map<String, Value>, key: &str) -> bool {
    block.get(key).is_some_and(|value| !value.is_null())
}

/// Whether this block's exact bytes are load-bearing.
fn is_pinned(block: &Map<String, Value>) -> bool {
    PINNING_KEYS.iter().any(|key| is_set(block, key))
}

/// Why a block's bytes are load-bearing, once they are.
///
/// Only two of the three markers survive a lapsed cache. `citations` carries
/// character offsets into this exact text and `signature` is computed over it,
/// so both are load-bearing whatever any cache is doing. `cache_control` is
/// load-bearing *because* its bytes are in a cache key — and a cache key that
/// no longer exists protects nothing.
///
/// A block pinned only by its position inside the cached prefix is the same
/// case as `cache_control`: it is being withheld to keep a cache entry alive,
/// so it is [`Protection::WhileCached`] by default here and the caller's
/// `Always` markers override it.
fn protection_of(block: &Map<String, Value>) -> Protection {
    if is_set(block, "citations") || is_set(block, "signature") {
        Protection::Always
    } else {
        Protection::WhileCached
    }
}

/// The position of the *last* block in this message carrying a cache
/// breakpoint, or `None` when it carries none.
///
/// The position, not merely the fact, because the provider caches the prefix up
/// to and including the marked **block** — not up to the message containing it.
/// Freezing the whole message is safe but pays for that safety twice: the
/// blocks after the marker are not in anyone's cache and were being withheld
/// anyway. On Claude Code that is the difference between compressing nothing
/// and compressing the tool results, because it marks `block[0]` of its final
/// message and puts the output after it (see docs/CLAUDE-CODE.md).
///
/// Nested `tool_result` content is scanned too: a marker there pins the prefix
/// just as much as one on the block that contains it.
/// What a block costs the provider that no text slot will show the engine.
///
/// An estimate, for pricing a promotion and nothing else: the walker hands the
/// engine text, and on a Claude Code turn most of what the provider bills is
/// not text — the `tool_use` input that ran a command, an image, a server tool
/// result. Earlier turns' thinking is left out because the provider does not
/// bill it as input. An image is a fixed allowance: its size in base64 says
/// nothing about its size in tokens.
fn unseen_tokens(blocks: &[Value]) -> u32 {
    const IMAGE_TOKENS: u32 = 1_600;
    fn json_tokens(value: &Value) -> u32 {
        estimate_tokens(&value.to_string(), "tool")
    }
    fn block_tokens(block: &Value) -> u32 {
        match block.get("type").and_then(Value::as_str) {
            Some("text") | Some("thinking") | Some("redacted_thinking") => 0,
            Some("image") | Some("document") => IMAGE_TOKENS,
            Some("tool_use") | Some("server_tool_use") => {
                let name = block.get("name").map_or(0, json_tokens);
                name.saturating_add(block.get("input").map_or(0, json_tokens))
            }
            // A string body is a text slot; a block list is slots for its text
            // and unseen for the rest.
            Some("tool_result") => match block.get("content") {
                Some(Value::Array(inner)) => {
                    inner.iter().map(block_tokens).fold(0, u32::saturating_add)
                }
                _ => 0,
            },
            _ => json_tokens(block),
        }
    }
    blocks.iter().map(block_tokens).fold(0, u32::saturating_add)
}

fn is_thinking(block: &Value) -> bool {
    matches!(
        block.get("type").and_then(Value::as_str),
        Some("thinking" | "redacted_thinking")
    )
}

fn cache_marker_position(blocks: &[Value]) -> Option<usize> {
    blocks
        .iter()
        .enumerate()
        .rev()
        .find_map(|(position, block)| block_carries_marker(block).then_some(position))
}

fn block_carries_marker(block: &Value) -> bool {
    (|| {
        let Some(block) = block.as_object() else {
            return false;
        };
        if is_set(block, "cache_control") {
            return true;
        }
        block
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|inner| {
                inner.iter().any(|nested| {
                    nested
                        .as_object()
                        .is_some_and(|nested| is_set(nested, "cache_control"))
                })
            })
    })()
}

/// The readable text of one message's blocks, each with its address.
///
/// `thinking`, `redacted_thinking`, `image`, `document` and `tool_use` produce
/// no slot at all: there is no text in them we could shorten, and `tool_use`
/// input is what a tool was asked to do.
/// `frozen_through` is the position of the last cache breakpoint in this
/// message, if it has one. Every block at or before it is inside the provider's
/// cached prefix and must come back byte-identical; every block after it is
/// not, whatever message it happens to share.
fn text_slots(
    blocks: &[Value],
    base: SlotPath,
    frozen_through: Option<usize>,
) -> Option<Vec<(TextSlot, SlotPath)>> {
    let mut slots = Vec::new();
    for (position, block) in blocks.iter().enumerate() {
        let block = block.as_object()?;
        let pinned = is_pinned(block) || frozen_through.is_some_and(|last| position <= last);
        let here = base.clone().index(position);
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    slots.push((
                        TextSlot {
                            text: text.to_string(),
                            protected: pinned,
                            protection: protection_of(block),
                        },
                        here.key("text"),
                    ));
                }
            }
            Some("tool_result") => match block.get("content") {
                Some(Value::String(text)) => slots.push((
                    TextSlot {
                        text: text.clone(),
                        protected: pinned,
                        protection: protection_of(block),
                    },
                    here.key("content"),
                )),
                Some(Value::Array(inner)) => {
                    for (nested_position, nested) in inner.iter().enumerate() {
                        let nested = nested.as_object()?;
                        if nested.get("type").and_then(Value::as_str) != Some("text") {
                            continue;
                        }
                        let Some(text) = nested.get("text").and_then(Value::as_str) else {
                            continue;
                        };
                        slots.push((
                            TextSlot {
                                text: text.to_string(),
                                // a marker on the enclosing block pins
                                // everything it contains
                                protected: pinned || is_pinned(nested),
                                // and the stricter of the two reasons wins: an
                                // enclosing signature is not weakened by a
                                // nested block that merely sits in the cache.
                                protection: match (protection_of(block), protection_of(nested)) {
                                    (Protection::Always, _) | (_, Protection::Always) => {
                                        Protection::Always
                                    }
                                    _ => Protection::WhileCached,
                                },
                            },
                            here.clone()
                                .key("content")
                                .index(nested_position)
                                .key("text"),
                        ));
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    Some(slots)
}

/// Every `tool_use` block in the request, keyed by the id its result quotes.
fn index_tool_uses(messages: &[Value]) -> ToolCallIndex {
    let mut index = ToolCallIndex::new();
    for message in messages {
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let Some(id) = block.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(name) = block.get("name").and_then(Value::as_str) else {
                continue;
            };
            // Anthropic sends tool input as an object, not a JSON string
            let file_path = block.get("input").and_then(file_path_in_arguments);
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

/// The tool name and file path for a message's `tool_result` blocks.
///
/// A user turn may batch several results from parallel calls, and the engine's
/// view has room for one name and one path. Agreement is required: if the
/// results in one message came from different calls, or any of them names a
/// call this request does not contain, both fields stay `None`. A wrong tool
/// name produces a wrong staleness verdict, which replaces content the model
/// still needs.
fn tool_metadata(blocks: &[Value], uses: &ToolCallIndex) -> (Option<String>, Option<String>) {
    let mut agreed: Option<&ToolCall> = None;
    let mut seen = 0usize;

    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        seen += 1;
        let resolved = block
            .get("tool_use_id")
            .and_then(Value::as_str)
            .and_then(|id| uses.get(id));
        match (resolved, agreed) {
            (Some(call), None) if seen == 1 => agreed = Some(call),
            (Some(call), Some(previous)) if call == previous => {}
            _ => return (None, None),
        }
    }

    match agreed {
        Some(call) => (Some(call.name.clone()), call.file_path.clone()),
        None => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kompressor_engine::messages::MessageContent;
    use serde_json::json;

    /// An agent turn with a cached system prompt and tool list, a thinking
    /// block, a tool call and its result behind a cache breakpoint, and a
    /// multimodal follow-up carrying a citation.
    fn body() -> Value {
        serde_json::from_str(
            r#"{
              "model": "claude-sonnet-4-5",
              "max_tokens": 1e5,
              "system": [
                {"type": "text", "text": "You are a careful engineer.",
                 "cache_control": {"type": "ephemeral"}}
              ],
              "tools": [
                {"name": "read_file", "description": "Read a file from disk",
                 "input_schema": {"type": "object", "properties": {"file_path": {"type": "string"}}},
                 "cache_control": {"type": "ephemeral"}}
              ],
              "messages": [
                {"role": "user", "content": "Why does the build fail?"},
                {"role": "assistant", "content": [
                  {"type": "thinking", "thinking": "The signature says u32.",
                   "signature": "ErUBCkYIBRgCIkDq0J"},
                  {"type": "text", "text": "Let me read the file."},
                  {"type": "tool_use", "id": "toolu_01", "name": "read_file",
                   "input": {"file_path": "src/build.rs"}}
                ]},
                {"role": "user", "content": [
                  {"type": "tool_result", "tool_use_id": "toolu_01",
                   "cache_control": {"type": "ephemeral"},
                   "content": [
                     {"type": "text", "text": "error[E0308]: mismatched types\n  --> src/build.rs:14:9\n"}
                   ]}
                ]},
                {"role": "user", "content": [
                  {"type": "text", "text": "and here is the CI screenshot"},
                  {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                   "data": "iVBORw0KGgoAAAANSUhEUg=="}},
                  {"type": "document", "source": {"type": "text", "media_type": "text/plain",
                   "data": "the build spec"}},
                  {"type": "text", "text": "the panel quotes the same trace",
                   "citations": [{"type": "char_location", "cited_text": "mismatched types",
                                  "start_char_index": 0, "end_char_index": 16}]},
                  {"type": "tool_result", "tool_use_id": "toolu_gone", "content": "stdout: ok"}
                ]}
              ]
            }"#,
        )
        .expect("fixture parses")
    }

    #[test]
    fn every_compressible_slot_is_found_with_the_role_the_wire_gave_it() {
        let walk = AnthropicMessagesWalker.walk(&body()).expect("known shape");
        let addresses: Vec<String> = walk.origins.iter().map(|o| o.path.to_string()).collect();
        assert_eq!(
            addresses,
            vec![
                "messages[0].content",
                "messages[1].content[1].text",
                "messages[2].content[0].content[0].text",
                "messages[3].content[0].text",
                "messages[3].content[3].text",
                "messages[3].content[4].content",
            ]
        );
        let roles: Vec<&str> = walk.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant", "user", "user"]);
        assert_eq!(walk.model.as_deref(), Some("claude-sonnet-4-5"));
    }

    #[test]
    fn a_tool_result_is_reached_as_a_string_and_as_a_block_list() {
        let walk = AnthropicMessagesWalker.walk(&body()).expect("known shape");
        match &walk.messages[2].content {
            MessageContent::Blocks(slots) => {
                assert!(slots[0].text.starts_with("error[E0308]"))
            }
            other => panic!("{other:?}"),
        }
        match &walk.messages[3].content {
            MessageContent::Blocks(slots) => assert_eq!(slots[2].text, "stdout: ok"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn thinking_images_documents_and_tool_use_input_are_never_offered_for_compression() {
        let walk = AnthropicMessagesWalker.walk(&body()).expect("known shape");
        for origin in &walk.origins {
            assert!(
                !origin.original.contains("The signature says")
                    && !origin.original.contains("iVBORw0")
                    && !origin.original.contains("the build spec")
                    && !origin.original.contains("src/build.rs\""),
                "{}",
                origin.path
            );
        }
        assert!(
            !walk
                .origins
                .iter()
                .any(|o| o.path.to_string().starts_with("messages[1].content[0]")
                    || o.path.to_string().starts_with("messages[1].content[2]"))
        );
    }

    #[test]
    fn system_and_tools_are_never_offered_for_compression() {
        let walk = AnthropicMessagesWalker.walk(&body()).expect("known shape");
        assert!(
            walk.origins
                .iter()
                .all(|o| o.path.to_string().starts_with("messages["))
        );
    }

    #[test]
    fn a_block_carrying_a_pinning_marker_comes_back_as_a_protected_slot() {
        let walk = AnthropicMessagesWalker.walk(&body()).expect("known shape");
        let protected: Vec<String> = walk
            .origins
            .iter()
            .filter(|o| o.protected)
            .map(|o| o.path.to_string())
            .collect();
        assert_eq!(
            protected,
            vec![
                "messages[2].content[0].content[0].text",
                "messages[3].content[3].text"
            ],
            "cache_control on the enclosing block, and citations on the text block"
        );
        match &walk.messages[3].content {
            MessageContent::Blocks(slots) => {
                assert!(slots[1].protected, "the cited block");
                assert!(
                    !slots[0].protected,
                    "the rest of the message still compresses"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_pin_says_whether_it_outlives_the_cache_it_names() {
        // Two markers pin a block and only one of them is about the cache, so
        // the walker has to record *why*: the engine cannot tell them apart
        // afterwards, and lifting both on a cold prefix would misalign every
        // citation offset.
        //
        // Two blocks because a lone one comes back as `Text`, which has
        // nowhere to carry a protection.
        let protection = |block: serde_json::Value| {
            let body = json!({"messages": [{"role": "user", "content": [
                {"type": "text", "text": "first"},
                block
            ]}]});
            match AnthropicMessagesWalker
                .walk(&body)
                .expect("a walk")
                .messages
                .remove(0)
                .content
            {
                kompressor_engine::messages::MessageContent::Blocks(mut slots) => {
                    slots.remove(1).protection
                }
                _ => panic!("blocks in, blocks out"),
            }
        };

        assert_eq!(
            protection(
                json!({"type": "text", "text": "x", "citations": [{"type": "char_location"}]})
            ),
            Protection::Always,
            "citation offsets point into these exact bytes, whatever a cache is doing"
        );
        assert_eq!(
            protection(json!({
                "type": "text", "text": "x",
                "cache_control": {"type": "ephemeral"}
            })),
            Protection::WhileCached,
            "a breakpoint is load-bearing only while the cache it names exists"
        );
    }

    #[test]
    fn a_signature_is_protected_by_never_being_walked() {
        // `signature` in practice only ever arrives on a `thinking` block, and
        // `text_slots` builds slots for `text` and `tool_result` alone — so the
        // bytes a signature was computed over are never offered to the engine
        // at all. Asserted rather than assumed: `protection_of` reads the field
        // and would answer `Always`, but that answer is unreachable today, and
        // a future block type carrying both a signature and walkable text is
        // exactly the case where this would be the only thing standing between
        // a cold prefix and an invalidated signature.
        let body = json!({"messages": [{"role": "assistant", "content": [
            {"type": "text", "text": "first"},
            {"type": "thinking", "thinking": "reasoning", "signature": "sig"}
        ]}]});
        match AnthropicMessagesWalker
            .walk(&body)
            .expect("a walk")
            .messages
            .remove(0)
            .content
        {
            kompressor_engine::messages::MessageContent::Blocks(slots) => assert_eq!(
                slots.len(),
                1,
                "the thinking block must not become a rewritable slot"
            ),
            other => panic!("expected blocks, got {other:?}"),
        }
    }

    #[test]
    fn a_signature_bearing_block_is_protected() {
        let body = json!({
          "model": "claude-sonnet-4-5",
          "messages": [{"role": "user", "content": [
            {"type": "text", "text": "replayed with its signature", "signature": "ErUBCkYIBRgC"}
          ]}]
        });
        let walk = AnthropicMessagesWalker.walk(&body).expect("known shape");
        assert!(walk.origins[0].protected);
        assert!(walk.is_empty(), "nothing here is compressible");
    }

    #[test]
    fn a_null_citations_field_is_not_a_marker() {
        // the API's own response blocks carry `"citations": null`, and clients
        // replay them verbatim
        let body = json!({
          "model": "claude-sonnet-4-5",
          "messages": [{"role": "user", "content": [
            {"type": "text", "text": "replayed verbatim", "citations": null}
          ]}]
        });
        let walk = AnthropicMessagesWalker.walk(&body).expect("known shape");
        assert!(!walk.origins[0].protected);
    }

    #[test]
    fn apply_never_rewrites_a_protected_slot() {
        let mut body = body();
        let mut walk = AnthropicMessagesWalker.walk(&body).expect("known shape");
        // something upstream mangled protected slots; write-back must refuse
        if let MessageContent::Blocks(slots) = &mut walk.messages[2].content {
            slots[0].text = "[... elided]".into();
        }
        if let MessageContent::Blocks(slots) = &mut walk.messages[3].content {
            slots[1].text = "[... elided]".into();
        }
        assert_eq!(AnthropicMessagesWalker.apply(&mut body, &walk), 0);
        assert_eq!(
            body["messages"][2]["content"][0]["content"][0]["text"],
            json!("error[E0308]: mismatched types\n  --> src/build.rs:14:9\n")
        );
        assert_eq!(
            body["messages"][3]["content"][3]["text"],
            json!("the panel quotes the same trace")
        );
    }

    #[test]
    fn the_frozen_floor_is_the_index_after_the_last_marked_message() {
        let walk = AnthropicMessagesWalker.walk(&body()).expect("known shape");
        assert_eq!(walk.frozen_message_count, 3);
    }

    #[test]
    fn markers_in_system_and_tools_do_not_move_the_frozen_floor() {
        let mut body = body();
        // strip the only marker inside `messages`; the ones on system and tools
        // stay exactly where they were
        body["messages"][2]["content"][0]
            .as_object_mut()
            .unwrap()
            .remove("cache_control");
        let walk = AnthropicMessagesWalker.walk(&body).expect("known shape");
        assert_eq!(walk.frozen_message_count, 0);
        assert!(body["system"][0]["cache_control"].is_object());
        assert!(body["tools"][0]["cache_control"].is_object());
    }

    #[test]
    fn a_conversation_with_no_markers_has_no_frozen_floor() {
        let body = json!({
          "model": "claude-sonnet-4-5",
          "messages": [{"role": "user", "content": "hello"}]
        });
        let walk = AnthropicMessagesWalker.walk(&body).expect("known shape");
        assert_eq!(walk.frozen_message_count, 0);
    }

    #[test]
    fn a_marker_nested_in_a_tool_result_still_freezes_its_message() {
        let body = json!({
          "model": "claude-sonnet-4-5",
          "messages": [
            {"role": "user", "content": [
              {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                {"type": "text", "text": "output", "cache_control": {"type": "ephemeral"}}
              ]}
            ]},
            {"role": "user", "content": "and now?"}
          ]
        });
        let walk = AnthropicMessagesWalker.walk(&body).expect("known shape");
        assert_eq!(walk.frozen_message_count, 1);
        assert!(walk.origins[0].protected);
    }

    #[test]
    fn the_tool_name_and_file_path_come_from_the_matching_tool_use() {
        let walk = AnthropicMessagesWalker.walk(&body()).expect("known shape");
        assert_eq!(
            walk.messages[2].meta.tool_name.as_deref(),
            Some("read_file")
        );
        assert_eq!(
            walk.messages[2].meta.file_path.as_deref(),
            Some("src/build.rs")
        );
    }

    #[test]
    fn tool_metadata_is_none_when_the_call_is_not_in_this_request() {
        let walk = AnthropicMessagesWalker.walk(&body()).expect("known shape");
        assert_eq!(walk.messages[3].meta.tool_name, None);
        assert_eq!(walk.messages[3].meta.file_path, None);
    }

    #[test]
    fn batched_results_from_different_calls_leave_the_metadata_unset() {
        let body = json!({
          "model": "claude-sonnet-4-5",
          "messages": [
            {"role": "assistant", "content": [
              {"type": "tool_use", "id": "t1", "name": "read_file", "input": {"file_path": "a.rs"}},
              {"type": "tool_use", "id": "t2", "name": "run_tests", "input": {}}
            ]},
            {"role": "user", "content": [
              {"type": "tool_result", "tool_use_id": "t1", "content": "fn main() {}"},
              {"type": "tool_result", "tool_use_id": "t2", "content": "41 passed"}
            ]}
          ]
        });
        let walk = AnthropicMessagesWalker.walk(&body).expect("known shape");
        assert_eq!(walk.messages[1].meta.tool_name, None);
        assert_eq!(walk.messages[1].meta.file_path, None);
    }

    #[test]
    fn a_walk_and_an_apply_that_changes_nothing_leave_the_body_byte_identical() {
        let original = body();
        let mut body = original.clone();
        let walk = AnthropicMessagesWalker.walk(&body).expect("known shape");
        assert_eq!(AnthropicMessagesWalker.apply(&mut body, &walk), 0);
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            serde_json::to_string(&original).unwrap()
        );
    }

    #[test]
    fn key_order_and_numeric_literals_survive_the_round_trip() {
        let source = r#"{"model":"claude-sonnet-4-5","max_tokens":64000,"temperature":0.0,"top_k":250,"top_p":0.9999999999999999,"messages":[{"role":"user","content":[{"type":"text","text":"hello","cache_control":null}]}],"metadata":{"user_id":"u_1"}}"#;
        let mut body: Value = serde_json::from_str(source).expect("fixture parses");
        let mut walk = AnthropicMessagesWalker.walk(&body).expect("known shape");
        if let MessageContent::Blocks(slots) = &mut walk.messages[0].content {
            slots[0].text = "hi".into();
        }
        assert_eq!(AnthropicMessagesWalker.apply(&mut body, &walk), 1);
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            source.replace("\"hello\"", "\"hi\""),
            "key order, the null marker field and every literal are as they arrived"
        );
    }

    #[test]
    fn apply_rewrites_only_the_slots_the_engine_changed() {
        let mut body = body();
        let mut walk = AnthropicMessagesWalker.walk(&body).expect("known shape");
        if let MessageContent::Blocks(slots) = &mut walk.messages[3].content {
            slots[2].text = "stdout: [... 1 line omitted]".into();
        }
        assert_eq!(AnthropicMessagesWalker.apply(&mut body, &walk), 1);
        assert_eq!(
            body["messages"][3]["content"][4]["content"],
            json!("stdout: [... 1 line omitted]")
        );
        assert_eq!(
            body["messages"][0]["content"],
            json!("Why does the build fail?")
        );
        assert_eq!(
            body["messages"][3]["content"][0]["text"],
            json!("and here is the CI screenshot")
        );
        assert!(
            body["messages"][3]["content"][4]["content"].is_string(),
            "a string tool_result stays a string"
        );
    }

    #[test]
    fn an_unrecognised_body_shape_yields_none_rather_than_a_partial_walk() {
        assert!(
            AnthropicMessagesWalker
                .walk(&json!({"model": "claude-sonnet-4-5", "system": "hi"}))
                .is_none()
        );
        assert!(
            AnthropicMessagesWalker
                .walk(&json!({"messages": {"role": "user"}}))
                .is_none()
        );
        assert!(
            AnthropicMessagesWalker
                .walk(&json!({"messages": [{"role": "user"}]}))
                .is_none()
        );
        assert!(
            AnthropicMessagesWalker
                .walk(&json!({"messages": [{"role": "user", "content": 7}]}))
                .is_none()
        );
        assert!(
            AnthropicMessagesWalker
                .walk(&json!({"messages": [{"role": "user", "content": ["bare string block"]}]}))
                .is_none()
        );
    }

    /// The engine prices a promotion against the provider's 20-block cache
    /// lookback, which it can only do if the walk counts blocks and says where
    /// the breakpoints are.
    #[test]
    fn every_message_reports_its_blocks_and_whether_it_carries_a_breakpoint() {
        let body = json!({"messages": [
            {"role": "user", "content": "plain"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "plan", "signature": "sig"},
                {"type": "text", "text": "calling"},
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1",
                 "content": [{"type": "text", "text": "a.rs", "cache_control": {"type": "ephemeral"}}]}
            ]}
        ]});
        let walk = AnthropicMessagesWalker.walk(&body).expect("known shape");
        let seen: Vec<_> = walk
            .messages
            .iter()
            .map(|m| (m.meta.cache_blocks, m.meta.cache_breakpoint))
            .collect();
        assert_eq!(
            seen,
            vec![(Some(1), false), (Some(2), false), (Some(1), true)]
        );
    }

    /// Promotion pricing needs what the provider bills, and on agent traffic
    /// most of that is not text.
    #[test]
    fn billed_content_outside_text_slots_is_counted_and_thinking_is_not() {
        let body = json!({"messages": [
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "a long plan ".repeat(200), "signature": "sig"},
            ]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "running it"},
                {"type": "tool_use", "id": "t1", "name": "Bash",
                 "input": {"command": "make -C build all 2>&1 | tail -n 200 ".repeat(20)}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "text", "text": "ok"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0".repeat(5000)}}
                ]}
            ]}
        ]});
        let walk = AnthropicMessagesWalker.walk(&body).expect("known shape");
        let unseen: Vec<u32> = walk.messages.iter().map(|m| m.meta.unseen_tokens).collect();
        assert_eq!(
            unseen[0], 0,
            "earlier turns' thinking is not billed as input"
        );
        assert!(unseen[1] > 100, "the tool call's input is: {}", unseen[1]);
        assert_eq!(
            unseen[2], 1_600,
            "an image is an allowance, not its base64 length"
        );
    }
}
