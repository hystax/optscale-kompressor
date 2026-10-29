//! Provider request bodies in, the engine's message view out, compressed text
//! back in place.
//!
//! The engine reads text and decides what that text is; it knows nothing about
//! wire formats. This crate is the other half: for each API family it finds the
//! text a provider request actually carries, presents it as
//! [`Message`]s, and — after the engine has rewritten some of it — puts the
//! result back exactly where it came from.
//!
//! # In place is the whole job
//!
//! A walked body is re-serialized and forwarded to the provider, so every byte
//! we did not deliberately change is a byte we must not change at all. Three
//! rules follow, and the tests assert all three:
//!
//! 1. **Only changed slots are rewritten.** A message the engine left alone
//!    keeps its exact bytes *and its structure* — including the difference
//!    between a string `content` and a one-element block array, which the
//!    providers treat as equivalent and their prompt caches do not.
//! 2. **An unrecognised shape is not a partial walk.** [`Walker::walk`] returns
//!    `None`, and the caller forwards the body untouched. Guessing at a shape
//!    we do not understand risks corrupting a request we could have simply
//!    relayed.
//! 3. **Structure decides protection, not luck.** A block whose bytes are
//!    load-bearing (a cache breakpoint, a citation anchor, a thinking
//!    signature) is handed to the engine as a protected slot and is never
//!    written back, whatever the engine's configuration says about roles.
//!
//! What holds this together is `serde_json`'s `preserve_order` and
//! `arbitrary_precision`: object keys keep their order and numeric literals are
//! carried as written, so a 30-digit id does not come back rounded through an
//! `f64`. One respelling survives them, and is deliberate rather than fixed:
//! the parser normalises an exponent, `1e5` to `1e+5`. The value and the
//! provider's token stream are identical, and providers cache on tokens rather
//! than on request bytes.
//!
//! # What this crate deliberately does not decide
//!
//! Role policy. A walker reports the role the wire gave it and lets
//! [`kompressor_engine::messages::compress_messages`] apply the protected-role
//! rules, so there is one implementation of "the assistant's turns are the
//! agent's working memory" rather than one per family. The exception is
//! structural protection above, which no engine setting may override.

#![forbid(unsafe_code)]

pub mod anthropic;
pub mod openai_chat;
pub mod openai_responses;
pub mod path;
mod tools;

use serde_json::Value;

use kompressor_engine::messages::{Message, MessageContent, MessageMeta, TextSlot};

use crate::path::SlotPath;

/// The request shapes this crate understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ApiFamily {
    /// OpenAI Chat Completions: `POST /v1/chat/completions`.
    OpenAiChat,
    /// OpenAI Responses: `POST /v1/responses`.
    OpenAiResponses,
    /// Anthropic Messages: `POST /v1/messages`.
    AnthropicMessages,
}

impl ApiFamily {
    /// The provider a request in this family belongs to, as
    /// `proxy.providers` names it: `anthropic` or `openai`.
    pub fn provider(self) -> &'static str {
        match self {
            ApiFamily::AnthropicMessages => "anthropic",
            ApiFamily::OpenAiChat | ApiFamily::OpenAiResponses => "openai",
        }
    }

    /// A stable identifier for logs, metrics and config.
    pub fn as_str(self) -> &'static str {
        match self {
            ApiFamily::OpenAiChat => "openai_chat",
            ApiFamily::OpenAiResponses => "openai_responses",
            ApiFamily::AnthropicMessages => "anthropic_messages",
        }
    }
}

/// Recognise a family from the request path, or `None` for every other
/// endpoint.
///
/// Matching is on the path *suffix* because an upstream may be mounted under a
/// prefix (`/openai/v1/chat/completions`), and on the whole final segment so
/// that neighbours like `/v1/messages/count_tokens` — a different API with a
/// different body — do not match `/v1/messages`.
///
/// # Codex under a ChatGPT subscription
///
/// A subscription credential moves the Responses endpoint off `/v1`: the
/// request goes to `chatgpt.com/backend-api/codex/responses`, and older builds
/// use `/backend-api/responses`. The body is an ordinary Responses body, so the
/// only thing standing between it and the compressor is this function — and an
/// unrecognised path is forwarded untouched rather than refused, which is the
/// failure that looks like success. Both spellings are listed explicitly rather
/// than matched as "anything ending in `/responses`", because the point of
/// matching whole segments is to not guess.
pub fn detect_family(path: &str) -> Option<ApiFamily> {
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let path = path.trim_end_matches('/');
    if path.ends_with("/v1/chat/completions") || path == "/chat/completions" {
        return Some(ApiFamily::OpenAiChat);
    }
    if path.ends_with("/v1/responses")
        || path == "/responses"
        || path.ends_with("/codex/responses")
        || path.ends_with("/backend-api/responses")
    {
        return Some(ApiFamily::OpenAiResponses);
    }
    if path.ends_with("/v1/messages") || path == "/messages" {
        return Some(ApiFamily::AnthropicMessages);
    }
    None
}

/// One text slot's origin in the request body.
///
/// `message`/`slot` address it in the engine's view, `path` addresses it in the
/// JSON tree, and `original` is what the walk read there — which is how `apply`
/// can tell a slot the engine rewrote from one it declined.
#[derive(Clone, Debug)]
pub struct SlotOrigin {
    /// Index into [`Walk::messages`].
    pub message: usize,
    /// Index into that message's [`MessageContent::Blocks`], or 0 for
    /// [`MessageContent::Text`].
    pub slot: usize,
    /// Where the string lives in the body.
    pub path: SlotPath,
    /// The text as the walk found it.
    pub original: String,
    /// The surrounding structure says these exact bytes must survive.
    pub protected: bool,
}

/// A walk of one request body: the compressible view, plus what it takes to
/// write the result back.
#[derive(Clone, Debug)]
pub struct Walk {
    /// Which walker produced this.
    pub family: ApiFamily,
    /// The conversation as the engine sees it. Compression mutates this in
    /// place; [`Walker::apply`] then transfers the changes to the body.
    pub messages: Vec<Message>,
    /// Messages `[0, frozen_message_count)` are inside a provider-cached prefix
    /// and must come back byte-identical.
    pub frozen_message_count: usize,
    /// The requested model, when the body names one. Used for per-model
    /// accounting, never for routing decisions inside this crate.
    pub model: Option<String>,
    /// Every text slot's origin, in walk order.
    pub origins: Vec<SlotOrigin>,
}

impl Walk {
    /// The slot's current text, after whatever the engine did to it.
    ///
    /// `None` when the message's content no longer has the shape the walk
    /// recorded, which is a caller bug rather than a wire condition — and is
    /// reported as "nothing to write" rather than by writing something wrong.
    pub fn current_text(&self, origin: &SlotOrigin) -> Option<&str> {
        match self.messages.get(origin.message).map(|m| &m.content) {
            Some(MessageContent::Text(text)) if origin.slot == 0 => Some(text.as_str()),
            Some(MessageContent::Blocks(slots)) => slots.get(origin.slot).map(|s| s.text.as_str()),
            _ => None,
        }
    }

    /// Total characters this walk is offering the engine, protected slots
    /// excluded. The proxy's minimum-size gate reads this.
    pub fn compressible_chars(&self) -> usize {
        self.origins
            .iter()
            .filter(|origin| !origin.protected)
            .map(|origin| origin.original.chars().count())
            .sum()
    }

    /// Whether this walk found anything worth handing to the engine.
    pub fn is_empty(&self) -> bool {
        self.origins.iter().all(|origin| origin.protected)
    }
}

/// Maps one API family's request body to and from the engine's message view.
pub trait Walker {
    /// Which family this walker speaks.
    fn family(&self) -> ApiFamily;

    /// Build the compressible view.
    ///
    /// Returns `None` when the body is not a shape this walker understands,
    /// which must make the caller pass the request through untouched rather
    /// than guess at it.
    fn walk(&self, body: &Value) -> Option<Walk>;

    /// Write compressed messages back into the body, returning how many slots
    /// were rewritten.
    ///
    /// Only slots the engine actually changed are touched. The default
    /// implementation is the whole story for every family: once a walk has
    /// recorded an address per slot, write-back has nothing family-specific
    /// left to decide, and a per-family implementation could only introduce a
    /// way for the two halves to disagree.
    fn apply(&self, body: &mut Value, walk: &Walk) -> usize {
        apply_walk(body, walk)
    }
}

/// Write a walk's changed slots back into the body it came from.
///
/// A slot is rewritten only when all of these hold: the engine changed its
/// text, the walk did not mark it protected, and the body still holds the exact
/// bytes the walk read there. The last condition is what makes a concurrent or
/// mistaken mutation of the body between `walk` and `apply` fail closed — we
/// would be overwriting something we never inspected.
pub fn apply_walk(body: &mut Value, walk: &Walk) -> usize {
    let mut rewritten = 0;
    for origin in &walk.origins {
        if origin.protected {
            continue;
        }
        let Some(current) = walk.current_text(origin) else {
            continue;
        };
        if current == origin.original {
            continue;
        }
        let replacement = current.to_string();
        let Some(target) = origin.path.resolve_mut(body) else {
            continue;
        };
        if target.as_str() != Some(origin.original.as_str()) {
            continue;
        }
        *target = Value::String(replacement);
        rewritten += 1;
    }
    rewritten
}

/// The walker for a family.
pub fn walker_for(family: ApiFamily) -> Box<dyn Walker> {
    match family {
        ApiFamily::OpenAiChat => Box::new(openai_chat::OpenAiChatWalker),
        ApiFamily::OpenAiResponses => Box::new(openai_responses::OpenAiResponsesWalker),
        ApiFamily::AnthropicMessages => Box::new(anthropic::AnthropicMessagesWalker),
    }
}

/// Accumulates messages and their slot origins so each walker states only what
/// its wire format says, and none of them repeats the index bookkeeping that
/// keeps `origins` and `messages` in step.
#[derive(Default)]
pub(crate) struct WalkBuilder {
    messages: Vec<Message>,
    origins: Vec<SlotOrigin>,
}

impl WalkBuilder {
    pub(crate) fn new() -> Self {
        WalkBuilder::default()
    }

    /// A message whose whole body is one string.
    pub(crate) fn push_text(
        &mut self,
        role: impl Into<String>,
        meta: MessageMeta,
        text: &str,
        path: SlotPath,
    ) {
        let message = self.messages.len();
        self.origins.push(SlotOrigin {
            message,
            slot: 0,
            path,
            original: text.to_string(),
            protected: false,
        });
        self.messages.push(Message {
            role: role.into(),
            content: MessageContent::Text(text.to_string()),
            meta,
        });
    }

    /// A message made of blocks, each with its own address.
    ///
    /// A message with no readable block at all becomes
    /// [`MessageContent::Opaque`] rather than an empty block list: an image-only
    /// turn has nothing to compress, and saying so keeps it out of the engine's
    /// accounting instead of reporting a zero-length compression.
    pub(crate) fn push_blocks(
        &mut self,
        role: impl Into<String>,
        meta: MessageMeta,
        slots: Vec<(TextSlot, SlotPath)>,
    ) {
        if slots.is_empty() {
            self.push_opaque(role, meta);
            return;
        }
        let message = self.messages.len();
        let mut content = Vec::with_capacity(slots.len());
        for (slot_index, (slot, path)) in slots.into_iter().enumerate() {
            self.origins.push(SlotOrigin {
                message,
                slot: slot_index,
                path,
                original: slot.text.clone(),
                protected: slot.protected,
            });
            content.push(slot);
        }
        self.messages.push(Message {
            role: role.into(),
            content: MessageContent::Blocks(content),
            meta,
        });
    }

    /// A message with nothing this engine can read.
    pub(crate) fn push_opaque(&mut self, role: impl Into<String>, meta: MessageMeta) {
        self.messages.push(Message {
            role: role.into(),
            content: MessageContent::Opaque,
            meta,
        });
    }

    /// The index the next pushed message will get, which is what a
    /// frozen-floor computation counts in.
    pub(crate) fn next_index(&self) -> usize {
        self.messages.len()
    }

    pub(crate) fn finish(
        self,
        family: ApiFamily,
        frozen_message_count: usize,
        model: Option<String>,
    ) -> Walk {
        Walk {
            family,
            messages: self.messages,
            frozen_message_count,
            model,
            origins: self.origins,
        }
    }
}

/// The `model` field, when the body names one.
pub(crate) fn model_of(body: &Value) -> Option<String> {
    body.get("model")?.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_families_endpoint_is_recognised_under_any_mount_prefix() {
        assert_eq!(
            detect_family("/v1/chat/completions"),
            Some(ApiFamily::OpenAiChat)
        );
        assert_eq!(
            detect_family("/openai/v1/chat/completions"),
            Some(ApiFamily::OpenAiChat)
        );
        assert_eq!(
            detect_family("/v1/responses?stream=true"),
            Some(ApiFamily::OpenAiResponses)
        );
        assert_eq!(
            detect_family("/anthropic/v1/messages/"),
            Some(ApiFamily::AnthropicMessages)
        );
    }

    #[test]
    fn the_codex_subscription_responses_paths_are_the_responses_family() {
        // A ChatGPT subscription moves Responses off /v1 entirely. These are
        // ordinary Responses bodies; failing to recognise them forwards a
        // full-size prompt and reports success, so each spelling is pinned.
        for path in [
            "/backend-api/codex/responses",
            "/backend-api/responses",
            "/v1/codex/responses",
            "/backend-api/codex/responses?stream=true",
        ] {
            assert_eq!(
                detect_family(path),
                Some(ApiFamily::OpenAiResponses),
                "{path} should be recognised as OpenAI Responses"
            );
        }
    }

    #[test]
    fn the_rest_of_the_codex_backend_api_surface_is_not_a_family() {
        // Subscription-mode Codex polls rate limits, identity and JWT refresh
        // under the same prefix. Those carry no conversation, and claiming them
        // would hand a non-completion body to the compressor.
        for path in [
            "/backend-api/models",
            "/backend-api/codex/models",
            "/backend-api/accounts/check",
            "/backend-api/codex/responses/resp_123/cancel",
        ] {
            assert_eq!(detect_family(path), None, "{path} should not be a family");
        }
    }

    #[test]
    fn a_neighbouring_endpoint_with_a_different_body_is_not_a_family() {
        // count_tokens takes a Messages-shaped body but is not a completion;
        // /v1/embeddings and /v1/models carry no conversation at all
        assert_eq!(detect_family("/v1/messages/count_tokens"), None);
        assert_eq!(detect_family("/v1/messages/batches"), None);
        assert_eq!(detect_family("/v1/embeddings"), None);
        assert_eq!(detect_family("/v1/models"), None);
        assert_eq!(detect_family("/v1/completions"), None);
    }

    #[test]
    fn a_real_compression_pass_reaches_the_body_and_stops_at_the_pinned_prefix() {
        use kompressor_engine::EngineOptions;
        use kompressor_engine::messages::{RequestOptions, compress_messages};

        let log = (0..400)
            .map(|i| {
                format!(
                    "2026-01-02T03:04:{:02}Z INFO worker heartbeat seq={i}",
                    i % 60
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut body = serde_json::json!({
          "model": "claude-sonnet-4-5",
          "messages": [
            {"role": "user", "content": [
              {"type": "tool_result", "tool_use_id": "t1",
               "cache_control": {"type": "ephemeral"}, "content": log}]},
            {"role": "assistant", "content": [{"type": "text", "text": log}]},
            {"role": "user", "content": [
              {"type": "tool_result", "tool_use_id": "t2", "content": log}]}
          ]
        });

        let walker = walker_for(ApiFamily::AnthropicMessages);
        let mut walk = walker.walk(&body).expect("known shape");
        assert_eq!(walk.frozen_message_count, 1);

        let options = EngineOptions::default();
        let mut refs = options.collector();
        let stats = compress_messages(
            &mut walk.messages,
            &options,
            &RequestOptions {
                frozen_message_count: walk.frozen_message_count,
                context: "",
                ..Default::default()
            },
            &mut refs,
        );
        assert!(
            stats.tokens_out < stats.tokens_in,
            "the engine did shrink it"
        );

        assert_eq!(walker.apply(&mut body, &walk), 1);
        assert_eq!(
            body["messages"][0]["content"][0]["content"], log,
            "frozen and pinned"
        );
        assert_eq!(body["messages"][1]["content"][0]["text"], log, "assistant");
        let compressed = body["messages"][2]["content"][0]["content"]
            .as_str()
            .expect("still a string");
        assert!(compressed.len() < log.len());
    }

    #[test]
    fn apply_refuses_a_slot_whose_bytes_changed_since_the_walk() {
        // between walk and apply the body is ours alone, so this cannot happen
        // by accident — which is the reason to make it impossible rather than
        // to trust it: whatever produced the new bytes, we never inspected them
        let mut body = serde_json::json!({
          "model": "gpt-4o",
          "messages": [{"role": "user", "content": "the text the walk read"}]
        });
        let mut walk = openai_chat::OpenAiChatWalker
            .walk(&body)
            .expect("known shape");
        walk.messages[0].content = MessageContent::Text("compressed".into());
        body["messages"][0]["content"] = Value::String("something else entirely".into());
        assert_eq!(openai_chat::OpenAiChatWalker.apply(&mut body, &walk), 0);
        assert_eq!(body["messages"][0]["content"], "something else entirely");
    }

    #[test]
    fn the_size_gate_counts_only_what_the_engine_may_touch() {
        let body = serde_json::json!({
          "model": "claude-sonnet-4-5",
          "messages": [{"role": "user", "content": [
            {"type": "text", "text": "0123456789"},
            {"type": "text", "text": "pinned", "cache_control": {"type": "ephemeral"}}
          ]}]
        });
        let walk = anthropic::AnthropicMessagesWalker
            .walk(&body)
            .expect("known shape");
        assert_eq!(
            walk.compressible_chars(),
            0,
            "the breakpoint is on the last block, so the whole message is inside \
             the cached prefix — including the unmarked block before it"
        );
        assert!(
            walk.is_empty(),
            "every origin is protected, so there is nothing to hand the engine"
        );
    }

    /// The reason the floor is per block and not per message.
    ///
    /// A breakpoint on an early block leaves everything after it outside the
    /// provider's cache, whatever message it shares. Claude Code's shape
    /// exactly: it marks `block[0]` of its final message and puts the tool
    /// output after it, which under a message-granular floor was withheld and
    /// compressed nothing at all.
    #[test]
    fn blocks_after_the_breakpoint_are_still_on_offer() {
        let body = serde_json::json!({
          "model": "claude-sonnet-4-5",
          "messages": [{"role": "user", "content": [
            {"type": "text", "text": "cached context", "cache_control": {"type": "ephemeral"}},
            {"type": "tool_result", "tool_use_id": "t1", "content": "0123456789"}
          ]}]
        });
        let walk = anthropic::AnthropicMessagesWalker
            .walk(&body)
            .expect("known shape");
        assert_eq!(
            walk.compressible_chars(),
            10,
            "the tool result follows the breakpoint and is not cached"
        );
        assert_eq!(
            walk.frozen_message_count, 0,
            "and the floor stops before a message that still has something to give"
        );
    }

    #[test]
    fn walker_for_returns_the_walker_that_speaks_that_family() {
        for family in [
            ApiFamily::OpenAiChat,
            ApiFamily::OpenAiResponses,
            ApiFamily::AnthropicMessages,
        ] {
            assert_eq!(walker_for(family).family(), family);
        }
    }
}

#[cfg(test)]
mod claude_code_shape {
    use super::*;
    use kompressor_engine::EngineOptions;
    use kompressor_engine::messages::{RequestOptions, compress_messages};

    /// End to end on the shape that produced no compression at all.
    ///
    /// Claude Code marks `block[0]` of its final message and puts the tool
    /// output after it. Under a message-granular floor the whole message was
    /// withheld and the request removed nothing; the recorded outcome was
    /// `prefix_frozen` and the measurement was zero.
    #[test]
    fn the_shape_that_used_to_compress_nothing_now_compresses() {
        let log = (0..400)
            .map(|i| {
                format!(
                    "2026-09-25T10:{:02}:{:02}Z INFO [req-{i:05}] handled /api/v2/items/{} in {}ms",
                    i / 60,
                    i % 60,
                    i % 7,
                    i % 40
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut body = serde_json::json!({
          "model": "claude-sonnet-4-5",
          "messages": [
            {"role": "user", "content": [{"type": "text", "text": "earlier turn"}]},
            {"role": "user", "content": [
              {"type": "text", "text": "cached context", "cache_control": {"type": "ephemeral"}},
              {"type": "tool_result", "tool_use_id": "t1", "content": log}
            ]}
          ]
        });

        let walker = walker_for(ApiFamily::AnthropicMessages);
        let mut walk = walker.walk(&body).expect("known shape");
        let options = EngineOptions::default();
        let mut refs = options.collector();
        let stats = compress_messages(
            &mut walk.messages,
            &options,
            &RequestOptions {
                frozen_message_count: walk.frozen_message_count,
                ..Default::default()
            },
            &mut refs,
        );
        assert!(
            stats.chars_out < stats.chars_in,
            "the tool result after the breakpoint has to compress: {} -> {}",
            stats.chars_in,
            stats.chars_out
        );
        assert_eq!(walker.apply(&mut body, &walk), 1, "one slot rewritten");
        assert_eq!(
            body["messages"][1]["content"][0]["text"],
            serde_json::json!("cached context"),
            "and the cached block is byte-identical"
        );
    }
}
