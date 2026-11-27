//! Which part of a request's cacheable prefix moved, and who moved it.
//!
//! The statistics can already say that a turn failed to read back the prefix
//! the turn before left, and put the loss down to expiry or to a
//! `prefix_change`. What they could not say is *which* part changed, or
//! whether the client or this proxy changed it — the question DESIGN §20 item 8
//! needed captured bodies and a byte diff to answer once.
//!
//! So each request is reduced to a handful of hashes, one per segment the
//! provider renders into the prompt ahead of the new turn, taken twice: over
//! the body as the client sent it and over the body as it goes upstream. The
//! next request of the same conversation is compared with both. A segment that
//! differs on the way in is the client's doing; one that differs only on the
//! way out is ours.
//!
//! Nothing here changes a byte of any request, and nothing here can fail one:
//! every function returns a value for any JSON at all.
//!
//! # What is hashed, and what is deliberately not
//!
//! Key order is kept, because the provider caches the rendered prompt and a
//! reordered schema renders differently. Three things are left out because the
//! provider leaves them out too:
//!
//! - `cache_control` markers. They say where to cache, and are not part of
//!   what is cached; Claude Code moves its marker to the newest message every
//!   turn, which would otherwise read as a rewrite of the message before it.
//! - The difference between a message whose content is a string and one whose
//!   content is a single text block. Claude Code sends a notification as a
//!   block the turn it arrives and as a string on every turn after.
//! - Tools marked `defer_loading`, which are not rendered until searched for.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use http::HeaderMap;
use serde_json::Value;

use kompressor_stats::context::{
    DRIFT_BETA, DRIFT_HISTORY, DRIFT_SYSTEM, DRIFT_TOOL_DEFS, DRIFT_TOOL_ORDER, DRIFT_TOOL_SET,
    PrefixObservation, Volatile,
};
use kompressor_wire::ApiFamily;

/// Conversations remembered, the same bound the prefix tracker keeps.
const REMEMBER: usize = 4096;

/// Per-message hashes kept for one conversation. Messages past it are folded
/// into one chain, so a change there is still seen and only its position is
/// lost.
const MESSAGES_KEPT: usize = 256;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a, for the reason the prefix tracker uses it: no per-process seed, so
/// a value means the same thing after a restart.
#[derive(Clone, Copy)]
struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(FNV_OFFSET)
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }

    /// One structural byte, so `["ab"]` and `["a","b"]` hash apart.
    fn mark(&mut self, tag: u8) {
        self.bytes(&[0xff, tag]);
    }

    fn word(&mut self, word: u64) {
        self.bytes(&word.to_le_bytes());
    }
}

/// Hash `value` in document order, skipping `cache_control` markers.
///
/// `opaque` is set below a tool call's `input` or `arguments`: what is in
/// there is the agent's data, and a key it happens to call `cache_control` is
/// content like any other.
fn hash_value(hash: &mut Fnv, value: &Value, opaque: bool) {
    match value {
        Value::Null => hash.mark(b'n'),
        Value::Bool(flag) => hash.mark(if *flag { b't' } else { b'f' }),
        Value::Number(number) => {
            hash.mark(b'#');
            hash.bytes(number.to_string().as_bytes());
        }
        Value::String(text) => {
            hash.mark(b's');
            hash.bytes(text.as_bytes());
        }
        Value::Array(items) => {
            hash.mark(b'[');
            for item in items {
                hash_value(hash, item, opaque);
            }
            hash.mark(b']');
        }
        Value::Object(object) => {
            hash.mark(b'{');
            for (key, item) in object {
                // A marker, not a schema property that happens to share its
                // name: the provider's is always `{"type": "ephemeral", …}`.
                if !opaque
                    && key == "cache_control"
                    && item.get("type").and_then(Value::as_str) == Some("ephemeral")
                {
                    continue;
                }
                hash.mark(b'k');
                hash.bytes(key.as_bytes());
                hash_value(hash, item, opaque || key == "input" || key == "arguments");
            }
            hash.mark(b'}');
        }
    }
}

fn hash_of(value: &Value) -> u64 {
    let mut hash = Fnv::new();
    hash_value(&mut hash, value, false);
    hash.0
}

/// One message, with a lone text block read as the string it stands for.
fn hash_message(message: &Value) -> u64 {
    let lone_text = message
        .get("content")
        .and_then(Value::as_array)
        .filter(|blocks| blocks.len() == 1)
        .and_then(|blocks| blocks[0].as_object())
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .and_then(|block| block.get("text"))
        .filter(|text| text.is_string());
    let Some(text) = lone_text else {
        return hash_of(message);
    };
    let mut hash = Fnv::new();
    hash.mark(b'{');
    if let Some(object) = message.as_object() {
        for (key, item) in object {
            hash.mark(b'k');
            hash.bytes(key.as_bytes());
            hash_value(&mut hash, if key == "content" { text } else { item }, false);
        }
    }
    hash.mark(b'}');
    hash.0
}

/// Counts what `serde_json` would write, without writing it.
struct Counter(u64);

impl std::io::Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len() as u64;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn size_of(value: &Value) -> u64 {
    let mut counter = Counter(0);
    // The counter cannot fail, and a `Value` always serializes.
    let _ = serde_json::to_writer(&mut counter, value);
    counter.0
}

fn clamp(size: u64) -> u32 {
    u32::try_from(size).unwrap_or(u32::MAX)
}

/// The hashes two requests are compared by.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Hashes {
    system: u64,
    /// Every rendered tool definition, in order.
    tool_defs: u64,
    /// The rendered tools' names, in order.
    tool_order: u64,
    /// The same names, order removed.
    tool_set: u64,
    beta: u64,
    /// The first [`MESSAGES_KEPT`] messages, one hash each.
    messages: Vec<u64>,
    /// A chain over every message past those, in order.
    rest: u64,
    /// The same chain as it stood after the message the previous request ended
    /// on, which is what that request's `rest` is compared with. `None` when
    /// this request does not reach that far, or no previous one is known.
    rest_at_mark: Option<u64>,
    messages_count: usize,
}

/// What one body holds: its hashes, its sizes, and what a later request needs
/// to know about it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Segments {
    pub(crate) hashes: Hashes,
    body_bytes: u64,
    system_bytes: u64,
    tools_bytes: u64,
    tools_count: u32,
    tools_deferred: u32,
    /// Whether the list carries the provider's own tool-search tool.
    tool_search: bool,
    messages_bytes: u64,
    reasoning_bytes: u64,
    /// Whether any marker the provider reads asks for a one-hour entry.
    pub(crate) one_hour: bool,
    /// Conversation items the request carried, instructions left out.
    pub(crate) messages: usize,
    /// Whether the client named a `prompt_cache_key`; `None` where the family
    /// has no such field.
    client_cache_key: Option<bool>,
    volatile: Volatile,
}

fn role_of(item: &Value) -> Option<&str> {
    item.get("role").and_then(Value::as_str)
}

fn is_instruction(item: &Value) -> bool {
    matches!(role_of(item), Some("system" | "developer"))
}

/// A tool's name, wherever its family keeps it.
pub(crate) fn tool_name(tool: &Value) -> &str {
    tool.get("name")
        .and_then(Value::as_str)
        .or_else(|| tool.pointer("/function/name").and_then(Value::as_str))
        .or_else(|| tool.get("type").and_then(Value::as_str))
        .unwrap_or("")
}

/// Whether `tool` is a provider's native tool-search tool: Anthropic's
/// `tool_search_tool_regex_…` and `tool_search_tool_bm25_…`, OpenAI's
/// `tool_search`. A typed entry with no schema of its own, whose presence
/// says the client loads tools on demand even where none in this list is
/// marked `defer_loading` — an MCP toolset defers its tools by its own
/// configuration, not by a flag on each.
fn is_tool_search(tool: &Value) -> bool {
    tool.get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.starts_with("tool_search"))
}

fn tool_description(tool: &Value) -> Option<&str> {
    tool.get("description").and_then(Value::as_str).or_else(|| {
        tool.pointer("/function/description")
            .and_then(Value::as_str)
    })
}

fn asks_one_hour(object: &Value) -> bool {
    object.pointer("/cache_control/ttl").and_then(Value::as_str) == Some("1h")
}

/// Whether any marker the Messages API reads asks for an hour: the places
/// `cachettl` writes, and no others.
fn any_one_hour(body: &Value) -> bool {
    if asks_one_hour(body) {
        return true;
    }
    let blocks = |key: &str| {
        body.get(key)
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    };
    if blocks("system").iter().any(asks_one_hour) || blocks("tools").iter().any(asks_one_hour) {
        return true;
    }
    blocks("messages").iter().any(|message| {
        message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|content| {
                content.iter().any(|block| {
                    asks_one_hour(block)
                        || block
                            .get("content")
                            .and_then(Value::as_array)
                            .is_some_and(|inner| inner.iter().any(asks_one_hour))
                })
            })
    })
}

/// Whether every cache marker the Messages API reads asks for an hour, and
/// there is at least one: the request's whole cached prefix then outlives any
/// pause a keep-warm ping would cover. One five-minute marker anywhere means
/// the conversation's tail is a five-minute entry (a longer lifetime must come
/// before a shorter one), so that request is still worth keeping warm.
pub(crate) fn every_marker_one_hour(body: &Value) -> bool {
    fn note(object: &Value, markers: &mut usize, short: &mut bool) {
        if object.get("cache_control").is_some_and(|m| !m.is_null()) {
            *markers += 1;
            *short |= !asks_one_hour(object);
        }
    }
    let (mut markers, mut short) = (0usize, false);
    note(body, &mut markers, &mut short);
    for key in ["system", "tools"] {
        for block in body
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            note(block, &mut markers, &mut short);
        }
    }
    for message in body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for block in message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            note(block, &mut markers, &mut short);
            for inner in block
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                note(inner, &mut markers, &mut short);
            }
        }
    }
    markers > 0 && !short
}

/// Bytes of plain-text reasoning inside `<think>` spans.
fn think_spans(text: &str) -> u64 {
    let mut total = 0;
    let mut rest = text;
    while let Some(open) = rest.find("<think>") {
        let after = &rest[open + "<think>".len()..];
        match after.find("</think>") {
            Some(close) => {
                total += close as u64;
                rest = &after[close + "</think>".len()..];
            }
            None => {
                total += after.len() as u64;
                break;
            }
        }
    }
    total
}

/// Reasoning one conversation item re-sends: Chat's `reasoning_content` and
/// inline `<think>` spans, Anthropic's `thinking` blocks, a Responses
/// `reasoning` item.
fn reasoning_in(item: &Value) -> u64 {
    if item.get("type").and_then(Value::as_str) == Some("reasoning") {
        return size_of(item);
    }
    let mut total = item
        .get("reasoning_content")
        .and_then(Value::as_str)
        .map_or(0, |text| text.len() as u64);
    match item.get("content") {
        Some(Value::String(text)) if role_of(item) == Some("assistant") => {
            total += think_spans(text);
        }
        Some(Value::Array(blocks)) => {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("thinking") {
                    total += block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .map_or(0, |text| text.len() as u64);
                }
            }
        }
        _ => {}
    }
    total
}

/// [`beta_hash`], for the body capture: when two requests' beta sets differ,
/// so do these, and nothing of the header can be read back from one.
pub(crate) fn beta_fingerprint(headers: &HeaderMap) -> u64 {
    beta_hash(headers)
}

/// The sorted, de-duplicated beta tokens a request carries, as one hash.
fn beta_hash(headers: &HeaderMap) -> u64 {
    let mut tokens: Vec<String> = Vec::new();
    for name in ["anthropic-beta", "openai-beta"] {
        for value in headers.get_all(name) {
            let Ok(text) = value.to_str() else { continue };
            tokens.extend(
                text.split(',')
                    .map(|token| token.trim().to_ascii_lowercase())
                    .filter(|token| !token.is_empty()),
            );
        }
    }
    tokens.sort();
    tokens.dedup();
    let mut hash = Fnv::new();
    for token in &tokens {
        hash.mark(b's');
        hash.bytes(token.as_bytes());
    }
    hash.0
}

impl Segments {
    /// [`Self::sized`] for a body whose serialized length is not to hand.
    #[cfg(test)]
    pub(crate) fn of(
        body: &Value,
        family: ApiFamily,
        headers: &HeaderMap,
        mark: Option<usize>,
    ) -> Segments {
        Segments::sized(body, size_of(body), family, headers, mark)
    }

    /// Reduce one request body to what is compared and counted.
    ///
    /// `body_bytes` is the length of the bytes `body` was parsed from, which
    /// the caller holds: serializing the whole request again only to count it
    /// was a second pass over every request. `headers` are the client's: the
    /// proxy forwards the beta tokens it was sent, so they are the same on
    /// both sides of it. `mark` is how many messages the conversation's
    /// previous request carried ([`DriftTracker::previous_messages`]).
    ///
    /// The volatile scan is not made here: see [`Self::scan_volatile`].
    pub(crate) fn sized(
        body: &Value,
        body_bytes: u64,
        family: ApiFamily,
        headers: &HeaderMap,
        mark: Option<usize>,
    ) -> Segments {
        let mut segments = Segments {
            body_bytes,
            one_hour: family == ApiFamily::AnthropicMessages && any_one_hour(body),
            ..Segments::default()
        };
        segments.hashes.beta = beta_hash(headers);
        segments.client_cache_key = (family != ApiFamily::AnthropicMessages).then(|| {
            body.get("prompt_cache_key")
                .and_then(Value::as_str)
                .is_some_and(|key| !key.is_empty())
        });

        // The conversation, and the instructions some families carry in it.
        let conversation_key = match family {
            ApiFamily::OpenAiResponses => "input",
            _ => "messages",
        };
        let empty = Vec::new();
        let items = body
            .get(conversation_key)
            .and_then(Value::as_array)
            .unwrap_or(&empty);

        let mut system = Fnv::new();
        for key in ["system", "instructions"] {
            if let Some(value) = body.get(key) {
                hash_value(&mut system, value, false);
                segments.system_bytes += size_of(value);
            }
        }
        // On the OpenAI families the instructions are the conversation's
        // leading system and developer items. One that arrives later is a
        // turn like any other: appended, not a change to the opening.
        let leading = leading_instructions(items, family);
        for item in &items[..leading] {
            hash_value(&mut system, item, false);
            segments.system_bytes += size_of(item);
        }
        segments.hashes.system = system.0;

        let mut rest = Fnv::new();
        for item in &items[leading..] {
            let hash = hash_message(item);
            if segments.hashes.messages.len() < MESSAGES_KEPT {
                segments.hashes.messages.push(hash);
            } else {
                rest.word(hash);
            }
            segments.hashes.messages_count += 1;
            if mark == Some(segments.hashes.messages_count) {
                segments.hashes.rest_at_mark = Some(rest.0);
            }
            segments.messages_bytes += size_of(item);
            segments.reasoning_bytes += reasoning_in(item);
        }
        segments.hashes.rest = rest.0;
        // A Responses request may carry its whole input as one string.
        if let Some(Value::String(text)) = body.get(conversation_key) {
            let mut hash = Fnv::new();
            hash.mark(b's');
            hash.bytes(text.as_bytes());
            segments.hashes.messages.push(hash.0);
            segments.hashes.messages_count = 1;
            segments.messages_bytes = text.len() as u64;
        }

        let mut defs = Fnv::new();
        let mut order = Fnv::new();
        let mut names: Vec<&str> = Vec::new();
        for tool in body
            .get("tools")
            .and_then(Value::as_array)
            .unwrap_or(&empty)
        {
            if tool.get("defer_loading").and_then(Value::as_bool) == Some(true) {
                segments.tools_deferred += 1;
                continue;
            }
            segments.tools_count += 1;
            segments.tool_search |= is_tool_search(tool);
            segments.tools_bytes += size_of(tool);
            hash_value(&mut defs, tool, false);
            let name = tool_name(tool);
            order.mark(b's');
            order.bytes(name.as_bytes());
            names.push(name);
        }
        segments.messages = segments.hashes.messages_count;
        segments.hashes.tool_defs = defs.0;
        segments.hashes.tool_order = order.0;
        names.sort_unstable();
        let mut set = Fnv::new();
        for name in &names {
            set.mark(b's');
            set.bytes(name.as_bytes());
        }
        segments.hashes.tool_set = set.0;
        segments
    }

    /// Count the volatile-looking values in the instructions and the rendered
    /// tools' descriptions of `body`, the request these segments were made
    /// from.
    ///
    /// Apart from [`Self::sized`] because it is wanted on few requests — the
    /// first of a conversation, and one whose instructions or tools changed
    /// ([`DriftTracker::opening_is_news`]) — and walks every byte of the
    /// system prompt and tool descriptions when it runs. Made on every
    /// request, it was most of what observing one cost.
    pub(crate) fn scan_volatile(&mut self, body: &Value, family: ApiFamily) {
        let mut strings: Vec<&str> = Vec::new();
        for key in ["system", "instructions"] {
            if let Some(value) = body.get(key) {
                collect_strings(value, &mut strings);
            }
        }
        let conversation_key = match family {
            ApiFamily::OpenAiResponses => "input",
            _ => "messages",
        };
        if let Some(items) = body.get(conversation_key).and_then(Value::as_array) {
            for item in &items[..leading_instructions(items, family)] {
                if let Some(content) = item.get("content") {
                    collect_strings(content, &mut strings);
                }
            }
        }
        if let Some(tools) = body.get("tools").and_then(Value::as_array) {
            strings.extend(
                tools
                    .iter()
                    .filter(|tool| tool.get("defer_loading").and_then(Value::as_bool) != Some(true))
                    .filter_map(tool_description),
            );
        }
        let mut found = Volatile::default();
        for text in strings {
            scan_volatile(text.as_bytes(), &mut found);
        }
        self.volatile = found;
    }
}

/// How many of a conversation's leading items are its instructions: none on
/// Anthropic, which carries them beside the conversation.
fn leading_instructions(items: &[Value], family: ApiFamily) -> usize {
    if family == ApiFamily::AnthropicMessages {
        0
    } else {
        items.iter().take_while(|item| is_instruction(item)).count()
    }
}

impl Segments {
    /// What names this request's tool definitions: the same while they are.
    pub(crate) fn tool_defs(&self) -> u64 {
        self.hashes.tool_defs
    }

    /// The same request with the beta tokens that actually go upstream, when
    /// `proxy.sticky_beta` put back what the client left out.
    pub(crate) fn with_beta(&self, headers: &HeaderMap) -> Segments {
        let mut sent = self.clone();
        sent.hashes.beta = beta_hash(headers);
        sent
    }
}

/// Every string under `value`, for the volatile scan of the system prompt.
fn collect_strings<'a>(value: &'a Value, into: &mut Vec<&'a str>) {
    match value {
        Value::String(text) => into.push(text),
        Value::Array(items) => items.iter().for_each(|item| collect_strings(item, into)),
        Value::Object(object) => {
            for (key, item) in object {
                if key != "cache_control" {
                    collect_strings(item, into);
                }
            }
        }
        _ => {}
    }
}

fn digits(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(u8::is_ascii_digit)
}

fn two(bytes: &[u8]) -> u32 {
    u32::from(bytes[0] - b'0') * 10 + u32::from(bytes[1] - b'0')
}

/// `YYYY-MM-DD` at the start of `at`, with a month and a day that exist.
fn is_date(at: &[u8]) -> bool {
    at.len() >= 10
        && digits(&at[0..4])
        && at[4] == b'-'
        && digits(&at[5..7])
        && at[7] == b'-'
        && digits(&at[8..10])
        && (1..=12).contains(&two(&at[5..7]))
        && (1..=31).contains(&two(&at[8..10]))
}

/// `[Tt ]HH:MM:SS` at the start of `at`.
fn is_time(at: &[u8]) -> bool {
    at.len() >= 9
        && matches!(at[0], b'T' | b't' | b' ')
        && digits(&at[1..3])
        && at[3] == b':'
        && digits(&at[4..6])
        && at[6] == b':'
        && digits(&at[7..9])
}

/// An 8-4-4-4-12 run of hex digits at the start of `at`.
fn is_uuid(at: &[u8]) -> bool {
    at.len() >= 36
        && at[..36].iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
        && at.get(36).is_none_or(|next| !next.is_ascii_alphanumeric())
}

fn base64url(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

/// `eyJ…` followed by two more dot-separated base64url runs. Returns its
/// length, so the scan can step over it.
fn jwt_len(at: &[u8]) -> Option<usize> {
    if !at.starts_with(b"eyJ") {
        return None;
    }
    let mut position = 0;
    for part in 0..3 {
        let run = at[position..]
            .iter()
            .take_while(|byte| base64url(**byte))
            .count();
        if run < 10 {
            return None;
        }
        position += run;
        if part < 2 {
            if at.get(position) != Some(&b'.') {
                return None;
            }
            position += 1;
        }
    }
    Some(position)
}

fn bump(count: &mut u8) {
    *count = count.saturating_add(1);
}

/// Count the volatile-looking values in `text`. By byte shape rather than by
/// pattern, so the cost is one pass and nothing is compiled.
fn scan_volatile(text: &[u8], into: &mut Volatile) {
    let mut index = 0;
    while index < text.len() {
        let at = &text[index..];
        let boundary = index == 0 || !text[index - 1].is_ascii_alphanumeric();
        if boundary && at[0].is_ascii_digit() && is_date(at) {
            if is_time(&at[10..]) {
                bump(&mut into.timestamps);
                index += 19;
                continue;
            }
            if at.get(10).is_none_or(|next| !next.is_ascii_digit()) {
                bump(&mut into.dates);
                index += 10;
                continue;
            }
        }
        if boundary && at[0].is_ascii_hexdigit() && is_uuid(at) {
            bump(&mut into.uuids);
            index += 36;
            continue;
        }
        if boundary && let Some(length) = jwt_len(at) {
            bump(&mut into.jwts);
            index += length;
            continue;
        }
        index += 1;
    }
}

/// What the previous request of a conversation looked like, on both sides of
/// this proxy.
struct Entry {
    at: Instant,
    incoming: Hashes,
    outgoing: Hashes,
    one_hour: bool,
    /// What `one_hour` would have been had the request gone out as received.
    incoming_one_hour: bool,
    /// The request's arrival ticket, so a correction is applied only to the
    /// entry that request left.
    ticket: u64,
}

/// The previous request of each conversation, for the next one to be compared
/// with.
#[derive(Default)]
pub struct DriftTracker {
    entries: Mutex<HashMap<u64, Entry>>,
}

/// What the tracker knows about the request before this one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Compared {
    pub(crate) compared: bool,
    pub(crate) drift_in: u8,
    pub(crate) drift_out: u8,
    /// Segments the client changed that went upstream unchanged all the same.
    pub(crate) repaired: u8,
    pub(crate) history_in_at: Option<u32>,
    pub(crate) history_out_at: Option<u32>,
    /// Whether the previous request asked for one-hour entries.
    pub(crate) previous_one_hour: Option<bool>,
}

/// The first message of `before` that `now` no longer carries unchanged.
///
/// Only what `before` held is compared: everything past it is the new turn.
fn first_changed(before: &Hashes, now: &Hashes) -> Option<u32> {
    let shared = before.messages.len().min(now.messages.len());
    if let Some(index) = (0..shared).find(|index| before.messages[*index] != now.messages[*index]) {
        return Some(index as u32);
    }
    if now.messages.len() < before.messages.len() {
        return Some(now.messages.len() as u32);
    }
    // Past the kept hashes only the chain is left: this request's, as it stood
    // where the previous one ended.
    (before.messages_count > MESSAGES_KEPT
        && now.rest_at_mark.is_some_and(|rest| rest != before.rest))
    .then_some(MESSAGES_KEPT as u32)
}

/// The segments of `now` that differ from `before`, and where history changed.
fn changed(before: &Hashes, now: &Hashes) -> (u8, Option<u32>) {
    let mut mask = 0;
    if before.system != now.system {
        mask |= DRIFT_SYSTEM;
    }
    if before.tool_set != now.tool_set {
        mask |= DRIFT_TOOL_SET;
    } else if before.tool_order != now.tool_order {
        mask |= DRIFT_TOOL_ORDER;
    } else if before.tool_defs != now.tool_defs {
        mask |= DRIFT_TOOL_DEFS;
    }
    if before.beta != now.beta {
        mask |= DRIFT_BETA;
    }
    let at = first_changed(before, now);
    if at.is_some() {
        mask |= DRIFT_HISTORY;
    }
    (mask, at)
}

impl DriftTracker {
    /// Compare this request with the conversation's previous one, then
    /// remember it in that one's place.
    ///
    /// A request carrying fewer messages than the one remembered is a branch —
    /// a subagent summary or a side query re-sending an earlier history. It is
    /// not compared, and it does not replace what is remembered: the main line
    /// is still the one its next request should be measured against.
    pub(crate) fn observe(
        &self,
        conversation: u64,
        incoming: &Segments,
        outgoing: &Segments,
        ticket: u64,
    ) -> Compared {
        let Ok(mut entries) = self.entries.lock() else {
            return Compared::default();
        };
        let mut result = Compared::default();
        if let Some(previous) = entries.get(&conversation) {
            if incoming.hashes.messages_count < previous.incoming.messages_count {
                return result;
            }
            let (drift_in, history_in_at) = changed(&previous.incoming, &incoming.hashes);
            let (out, history_out_at) = changed(&previous.outgoing, &outgoing.hashes);
            // History is ours when it moved earlier on the way out than it did
            // on the way in; every other segment, when it moved only there.
            let history_is_ours = match (history_out_at, history_in_at) {
                (Some(out_at), Some(in_at)) => out_at < in_at,
                (Some(_), None) => true,
                _ => false,
            };
            let mut drift_out = out & !drift_in & !DRIFT_HISTORY;
            if history_is_ours {
                drift_out |= DRIFT_HISTORY;
            }
            // What the client changed and the provider never saw change: a
            // reorder pinned back, a beta token put back. History is left
            // out — nothing here restores a message the client rewrote.
            let repaired = drift_in & !out & !DRIFT_HISTORY;
            result = Compared {
                compared: true,
                drift_in,
                drift_out,
                repaired,
                history_in_at,
                history_out_at: history_is_ours.then_some(history_out_at).flatten(),
                previous_one_hour: Some(previous.one_hour),
            };
        } else if entries.len() >= REMEMBER
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.at)
                .map(|(key, _)| *key)
        {
            entries.remove(&oldest);
        }
        entries.insert(
            conversation,
            Entry {
                at: Instant::now(),
                incoming: incoming.hashes.clone(),
                outgoing: outgoing.hashes.clone(),
                one_hour: outgoing.one_hour,
                incoming_one_hour: incoming.one_hour,
                ticket,
            },
        );
        result
    }

    /// The request holding `ticket` went upstream as the client sent it after
    /// all — its rewrite could not be re-encoded — so what is remembered as
    /// sent is what was received. Nothing changes if a later request of the
    /// conversation has been observed since.
    pub(crate) fn sent_as_received(&self, conversation: u64, ticket: u64) {
        if let Ok(mut entries) = self.entries.lock()
            && let Some(entry) = entries.get_mut(&conversation)
            && entry.ticket == ticket
        {
            entry.outgoing = entry.incoming.clone();
            entry.one_hour = entry.incoming_one_hour;
        }
    }

    /// Whether a volatile scan of `incoming` would be reported: there is no
    /// earlier request to compare it with — the conversation's first, or a
    /// branch — or its instructions or tools are not the ones that request
    /// carried. The same test [`observation`] applies to what `observe`
    /// returns, asked before the scan is paid for.
    pub(crate) fn opening_is_news(&self, conversation: u64, incoming: &Segments) -> bool {
        let Ok(entries) = self.entries.lock() else {
            return false;
        };
        entries.get(&conversation).is_none_or(|previous| {
            let (before, now) = (&previous.incoming, &incoming.hashes);
            now.messages_count < before.messages_count
                || before.system != now.system
                || before.tool_set != now.tool_set
                || before.tool_order != now.tool_order
                || before.tool_defs != now.tool_defs
        })
    }

    /// How many messages the conversation's previous request carried, for
    /// [`Segments::sized`] to mark its chain at.
    pub(crate) fn previous_messages(&self, conversation: u64) -> Option<usize> {
        self.entries
            .lock()
            .ok()?
            .get(&conversation)
            .map(|entry| entry.incoming.messages_count)
    }

    /// Whether the conversation's previous request asked for one-hour cache
    /// entries: the lifetime a lapse is predicted against.
    pub(crate) fn previous_one_hour(&self, conversation: u64) -> Option<bool> {
        self.entries
            .lock()
            .ok()?
            .get(&conversation)
            .map(|entry| entry.one_hour)
    }

    /// Conversations remembered. For tests and for `/metrics`.
    pub fn tracked(&self) -> usize {
        self.entries.lock().map_or(0, |entries| entries.len())
    }
}

/// What the prefix tracker knew of the conversation as this request arrived.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Idle {
    pub(crate) idle_ms: u64,
    pub(crate) was_warm: bool,
    /// Whether the request carries more messages than any before it.
    pub(crate) continues: bool,
    /// Tokens of the conversation's first request the provider held in its
    /// cache after answering it (`PrefixTracker::opened_with`).
    pub(crate) opened_with: Option<u32>,
}

/// Put one request's observation together.
pub(crate) fn observation(
    family: ApiFamily,
    incoming: &Segments,
    outgoing: &Segments,
    compared: Compared,
    text_bytes: u64,
    idle: Option<Idle>,
) -> PrefixObservation {
    // Reported only where it is news: the first request of a conversation,
    // and one whose instructions or tools changed. The caller scans only
    // then (`DriftTracker::opening_is_news`); this is the same test, on what
    // the comparison found.
    let fresh = !compared.compared
        || compared.drift_in & (DRIFT_SYSTEM | DRIFT_TOOL_DEFS | DRIFT_TOOL_ORDER | DRIFT_TOOL_SET)
            != 0;
    PrefixObservation {
        compared: compared.compared,
        drift_in: compared.drift_in,
        drift_out: compared.drift_out,
        repaired: compared.repaired,
        history_in_at: compared.history_in_at,
        history_out_at: compared.history_out_at,
        body_bytes: clamp(incoming.body_bytes),
        system_bytes: clamp(incoming.system_bytes),
        tools_bytes: clamp(incoming.tools_bytes),
        tools_count: incoming.tools_count,
        tools_deferred: incoming.tools_deferred,
        // What the provider caches in front of the conversation is what was
        // sent, not what arrived: the retrieve tool is in it.
        opening_bytes: clamp(outgoing.system_bytes + outgoing.tools_bytes),
        tool_search: incoming.tool_search,
        messages_bytes: clamp(incoming.messages_bytes),
        messages_count: clamp(incoming.hashes.messages_count as u64),
        text_bytes: clamp(text_bytes),
        reasoning_bytes: clamp(incoming.reasoning_bytes),
        volatile: (fresh && incoming.volatile.any()).then_some(incoming.volatile),
        idle_ms: idle.map(|idle| idle.idle_ms),
        was_warm: idle.is_some_and(|idle| idle.was_warm),
        continues: idle.map(|idle| idle.continues),
        shared_read: idle.and_then(|idle| idle.opened_with).unwrap_or(0),
        lifetime_s: (family == ApiFamily::AnthropicMessages)
            .then_some(compared.previous_one_hour)
            .flatten()
            .map(|one_hour| if one_hour { 3_600 } else { 300 }),
        // Stamped by the caller, which knows what the turn did.
        recompacted: None,
        ledger_reads: 0,
        tools_compacted_bytes: 0,
        tools_proxy_deferred: 0,
        tools_proxy_deferred_bytes: 0,
        reasoning_elided_bytes: 0,
        client_cache_key: incoming.client_cache_key,
        // Stamped by the caller, which measures them.
        tools_compactable_bytes: 0,
        stale_read_bytes: 0,
        superseded_read_bytes: 0,
        quiet_read_bytes: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn anthropic(body: &Value) -> Segments {
        Segments::of(body, ApiFamily::AnthropicMessages, &HeaderMap::new(), None)
    }

    fn turn(messages: Value) -> Value {
        json!({
            "model": "claude-opus-5-5",
            "system": [{"type": "text", "text": "You are an agent."}],
            "tools": [
                {"name": "Read", "description": "Read a file", "input_schema": {"type": "object"}},
                {"name": "Bash", "description": "Run a command", "input_schema": {"type": "object"}}
            ],
            "messages": messages
        })
    }

    fn observe(tracker: &DriftTracker, incoming: &Value, outgoing: &Value) -> Compared {
        tracker.observe(7, &anthropic(incoming), &anthropic(outgoing), 0)
    }

    #[test]
    fn the_first_request_of_a_conversation_is_compared_with_nothing() {
        let tracker = DriftTracker::default();
        let body = turn(json!([{"role": "user", "content": "hello"}]));
        let seen = observe(&tracker, &body, &body);
        assert!(!seen.compared);
        assert_eq!((seen.drift_in, seen.drift_out), (0, 0));
        assert_eq!(tracker.tracked(), 1);
    }

    #[test]
    fn an_appended_turn_is_not_drift() {
        let tracker = DriftTracker::default();
        let one = turn(json!([{"role": "user", "content": "hello"}]));
        let two = turn(json!([
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "hi"},
            {"role": "user", "content": "go on"}
        ]));
        observe(&tracker, &one, &one);
        let seen = observe(&tracker, &two, &two);
        assert!(seen.compared);
        assert_eq!((seen.drift_in, seen.drift_out), (0, 0));
        assert_eq!(seen.previous_one_hour, Some(false));
    }

    /// The client changing a tool is the client's drift; the same change made
    /// between the two sides of the proxy is ours. Both directions, so neither
    /// can pass by reporting everything or nothing.
    #[test]
    fn a_changed_tool_is_put_down_to_whoever_changed_it() {
        let one = turn(json!([{"role": "user", "content": "hello"}]));
        let mut two = one.clone();
        two["tools"][0]["description"] = json!("Read a file from disk");

        let tracker = DriftTracker::default();
        observe(&tracker, &one, &one);
        let by_client = observe(&tracker, &two, &two);
        assert_eq!(by_client.drift_in, DRIFT_TOOL_DEFS);
        assert_eq!(by_client.drift_out, 0);

        let tracker = DriftTracker::default();
        observe(&tracker, &one, &one);
        let by_proxy = observe(&tracker, &one, &two);
        assert_eq!(by_proxy.drift_in, 0);
        assert_eq!(by_proxy.drift_out, DRIFT_TOOL_DEFS);
    }

    #[test]
    fn a_reorder_an_addition_and_a_redefinition_are_told_apart() {
        let one = anthropic(&turn(json!([])));
        let mut reordered = turn(json!([]));
        reordered["tools"].as_array_mut().unwrap().reverse();
        let mut grown = turn(json!([]));
        grown["tools"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name": "Grep", "description": "Search", "input_schema": {}}));
        let mut schema_keys = turn(json!([]));
        schema_keys["tools"][0]["input_schema"] = json!({"properties": {}, "type": "object"});

        assert_eq!(
            changed(&one.hashes, &anthropic(&reordered).hashes).0,
            DRIFT_TOOL_ORDER
        );
        assert_eq!(
            changed(&one.hashes, &anthropic(&grown).hashes).0,
            DRIFT_TOOL_SET
        );
        assert_eq!(
            changed(&one.hashes, &anthropic(&schema_keys).hashes).0,
            DRIFT_TOOL_DEFS
        );
    }

    /// The injected retrieve tool appearing mid-conversation is the break
    /// DESIGN §20 item 8 took a byte diff to find.
    #[test]
    fn a_tool_the_proxy_adds_mid_conversation_is_its_own_drift() {
        let tracker = DriftTracker::default();
        let one = turn(json!([{"role": "user", "content": "hello"}]));
        let mut with_tool = one.clone();
        with_tool["tools"].as_array_mut().unwrap().push(
            json!({"name": "optscale_kompressor_retrieve", "description": "Fetch", "input_schema": {}}),
        );
        observe(&tracker, &one, &one);
        let seen = observe(&tracker, &one, &with_tool);
        assert_eq!(seen.drift_in, 0);
        assert_eq!(seen.drift_out, DRIFT_TOOL_SET);
    }

    #[test]
    fn a_moved_cache_marker_is_not_a_rewritten_message() {
        let tracker = DriftTracker::default();
        let one = turn(json!([
            {"role": "user", "content": [
                {"type": "text", "text": "hello", "cache_control": {"type": "ephemeral"}}
            ]}
        ]));
        // The marker has moved on and the lone block has become a string.
        let two = turn(json!([
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "hi"},
            {"role": "user", "content": [
                {"type": "text", "text": "next", "cache_control": {"type": "ephemeral"}}
            ]}
        ]));
        observe(&tracker, &one, &one);
        let seen = observe(&tracker, &two, &two);
        assert!(seen.compared);
        assert_eq!(seen.drift_in, 0, "nothing the provider renders has changed");
    }

    #[test]
    fn a_marker_inside_a_tool_call_is_the_agents_own_data() {
        let call = |ttl: &str| {
            turn(json!([
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "Write",
                     "input": {"cache_control": {"ttl": ttl}}}
                ]}
            ]))
        };
        let tracker = DriftTracker::default();
        observe(&tracker, &call("5m"), &call("5m"));
        let seen = observe(&tracker, &call("1h"), &call("1h"));
        assert_eq!(seen.drift_in, DRIFT_HISTORY);
        assert_eq!(seen.history_in_at, Some(0));
        // And it is not read as a request for one-hour entries.
        assert!(!anthropic(&call("1h")).one_hour);
    }

    #[test]
    fn rewritten_history_is_located_and_attributed() {
        let one = turn(json!([
            {"role": "user", "content": "a"},
            {"role": "assistant", "content": "b"},
            {"role": "user", "content": "c"}
        ]));
        let mut grown = one.clone();
        grown["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role": "assistant", "content": "d"}));
        let mut client_rewrote = grown.clone();
        client_rewrote["messages"][2]["content"] = json!("C");
        let mut proxy_rewrote = grown.clone();
        proxy_rewrote["messages"][1]["content"] = json!("[elided]");

        let tracker = DriftTracker::default();
        observe(&tracker, &one, &one);
        let seen = observe(&tracker, &client_rewrote, &client_rewrote);
        assert_eq!((seen.drift_in, seen.drift_out), (DRIFT_HISTORY, 0));
        assert_eq!(seen.history_in_at, Some(2));

        let tracker = DriftTracker::default();
        observe(&tracker, &one, &one);
        let seen = observe(&tracker, &grown, &proxy_rewrote);
        assert_eq!((seen.drift_in, seen.drift_out), (0, DRIFT_HISTORY));
        assert_eq!(seen.history_out_at, Some(1));

        // Both changed it: ours only where we changed it earlier.
        let tracker = DriftTracker::default();
        observe(&tracker, &one, &one);
        let mut both = client_rewrote.clone();
        both["messages"][1]["content"] = json!("[elided]");
        let seen = observe(&tracker, &client_rewrote, &both);
        assert_eq!(seen.drift_in, DRIFT_HISTORY);
        assert_eq!(seen.drift_out, DRIFT_HISTORY);
        assert_eq!(
            (seen.history_in_at, seen.history_out_at),
            (Some(2), Some(1))
        );
    }

    #[test]
    fn a_branch_is_neither_compared_nor_remembered() {
        let tracker = DriftTracker::default();
        let long = turn(json!([
            {"role": "user", "content": "a"},
            {"role": "assistant", "content": "b"},
            {"role": "user", "content": "c"}
        ]));
        let side = turn(json!([{"role": "user", "content": "summarise"}]));
        let mut longer = long.clone();
        longer["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role": "assistant", "content": "d"}));

        observe(&tracker, &long, &long);
        assert!(!observe(&tracker, &side, &side).compared);
        let seen = observe(&tracker, &longer, &longer);
        assert!(seen.compared);
        assert_eq!(seen.drift_in, 0, "the main line is compared with itself");
    }

    #[test]
    fn deferred_tools_are_counted_and_not_compared() {
        let one = turn(json!([]));
        let mut loaded = one.clone();
        loaded["tools"].as_array_mut().unwrap().push(
            json!({"name": "mcp__x__y", "description": "Later", "input_schema": {}, "defer_loading": true}),
        );
        let (before, after) = (anthropic(&one), anthropic(&loaded));
        assert_eq!(changed(&before.hashes, &after.hashes).0, 0);
        assert_eq!((after.tools_count, after.tools_deferred), (2, 1));
        assert_eq!(after.tools_bytes, before.tools_bytes);
    }

    #[test]
    fn beta_tokens_are_compared_as_a_set() {
        let headers = |value: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert("anthropic-beta", http::HeaderValue::from_static(value));
            headers
        };
        let body = turn(json!([]));
        let of = |value| Segments::of(&body, ApiFamily::AnthropicMessages, &headers(value), None);
        assert_eq!(
            changed(&of("a-1, b-2").hashes, &of("B-2,a-1").hashes).0,
            0,
            "order and case are not a change"
        );
        assert_eq!(
            changed(&of("a-1, b-2").hashes, &of("a-1").hashes).0,
            DRIFT_BETA
        );
    }

    /// A developer message the client appends on a later turn is a turn. It
    /// must not read as the opening instructions having changed.
    #[test]
    fn an_instruction_item_that_arrives_later_is_an_appended_turn() {
        let chat = |late: bool| {
            let mut messages = vec![
                json!({"role": "system", "content": "be brief"}),
                json!({"role": "user", "content": "hello"}),
            ];
            if late {
                messages.push(json!({"role": "assistant", "content": "hi"}));
                messages.push(json!({"role": "developer", "content": "the user is on mobile"}));
            }
            json!({"model": "gpt-5.5", "messages": messages})
        };
        let of = |body: &Value| Segments::of(body, ApiFamily::OpenAiChat, &HeaderMap::new(), None);
        let (one, two) = (of(&chat(false)), of(&chat(true)));
        assert_eq!(changed(&one.hashes, &two.hashes).0, 0);
        assert_eq!(two.hashes.messages_count, 3, "counted where it sits");
    }

    /// A client drift the proxy undid is recorded as repaired: the client did
    /// change it, and the provider never saw it change.
    #[test]
    fn a_change_that_never_reached_upstream_is_repaired_not_drift_out() {
        let tracker = DriftTracker::default();
        let one = turn(json!([{"role": "user", "content": "hello"}]));
        let mut reordered = one.clone();
        reordered["tools"].as_array_mut().unwrap().reverse();
        observe(&tracker, &one, &one);
        // The client reordered; what went upstream is the first order.
        let seen = observe(&tracker, &reordered, &one);
        assert_eq!(seen.drift_in, DRIFT_TOOL_ORDER);
        assert_eq!(seen.repaired, DRIFT_TOOL_ORDER);
        assert_eq!(seen.drift_out, 0);
        // Forwarded as sent, it is plain client drift.
        let tracker = DriftTracker::default();
        observe(&tracker, &one, &one);
        let seen = observe(&tracker, &reordered, &reordered);
        assert_eq!((seen.drift_in, seen.repaired), (DRIFT_TOOL_ORDER, 0));
    }

    #[test]
    fn a_schema_property_named_like_the_marker_is_compared() {
        let tool = |kind: &str| {
            let mut body = turn(json!([]));
            body["tools"][0]["input_schema"] =
                json!({"type": "object", "properties": {"cache_control": {"type": kind}}});
            anthropic(&body)
        };
        assert_eq!(
            changed(&tool("string").hashes, &tool("integer").hashes).0,
            DRIFT_TOOL_DEFS
        );
    }

    #[test]
    fn chat_instructions_are_the_system_segment_and_not_history() {
        let chat = |system: &str| {
            json!({"model": "gpt-5.5", "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "<think>plan it</think>done",
                 "reasoning_content": "abcd"}
            ]})
        };
        let of = |body: &Value| Segments::of(body, ApiFamily::OpenAiChat, &HeaderMap::new(), None);
        let (one, two) = (of(&chat("be brief")), of(&chat("be thorough")));
        assert_eq!(changed(&one.hashes, &two.hashes).0, DRIFT_SYSTEM);
        assert_eq!(one.hashes.messages_count, 2);
        assert_eq!(one.reasoning_bytes, 4 + "plan it".len() as u64);
        assert!(one.system_bytes > 0 && one.messages_bytes > 0);
    }

    #[test]
    fn responses_input_and_instructions_are_read() {
        let body = json!({"model": "gpt-5.5", "instructions": "be brief", "input": [
            {"role": "developer", "content": "house rules"},
            {"type": "reasoning", "id": "rs_1", "summary": []},
            {"role": "user", "content": "hello"}
        ], "tools": [{"type": "function", "name": "shell", "description": "run"}]});
        let seen = Segments::of(&body, ApiFamily::OpenAiResponses, &HeaderMap::new(), None);
        assert_eq!(seen.hashes.messages_count, 2);
        assert_eq!(seen.tools_count, 1);
        assert!(seen.reasoning_bytes > 0);
        let text = json!({"model": "gpt-5.5", "input": "hello"});
        let seen = Segments::of(&text, ApiFamily::OpenAiResponses, &HeaderMap::new(), None);
        assert_eq!(seen.hashes.messages_count, 1);
    }

    #[test]
    fn one_hour_markers_are_found_where_the_api_reads_them() {
        let mut body = turn(json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "text", "text": "x", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
                ]}
            ]}
        ]));
        assert!(anthropic(&body).one_hour);
        body["messages"][0]["content"][0]["content"][0]["cache_control"]["ttl"] = json!("5m");
        assert!(!anthropic(&body).one_hour);
    }

    #[test]
    fn volatile_values_are_counted_by_shape() {
        let mut seen = Volatile::default();
        scan_volatile(
            b"Today is 2026-11-20. Started 2026-11-20T10:15:00Z, session \
              3f2b8c1e-9d4a-4b7c-8e21-0a1b2c3d4e5f, token \
              eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.c2lnbmF0dXJlLWJ5dGVz",
            &mut seen,
        );
        assert_eq!(
            (seen.dates, seen.timestamps, seen.uuids, seen.jwts),
            (1, 1, 1, 1)
        );

        // Shapes that only resemble them.
        let mut none = Volatile::default();
        scan_volatile(
            b"build 1234-56-78, tool_search_tool_regex_20251119, v2026-13-01, \
              claude-opus-4-5-20251101, range 10-20-30, eyJ.short.x, \
              deadbeef-dead-beef-dead-beefdeadbeefX",
            &mut none,
        );
        assert!(!none.any(), "{none:?}");
    }

    /// The provider caches what was sent, so the opening is sized from the
    /// outgoing body: a tool the proxy added is in it, a deferred one is not.
    #[test]
    fn the_opening_is_sized_from_what_went_upstream() {
        let incoming = turn(json!([{"role": "user", "content": "hello"}]));
        let mut outgoing = incoming.clone();
        let added = json!({
            "name": "optscale_kompressor_retrieve",
            "description": "fetch back elided content ".repeat(40),
            "input_schema": {"type": "object"}
        });
        let deferred =
            json!({"name": "Later", "description": "x".repeat(900), "defer_loading": true});
        if let Some(tools) = outgoing["tools"].as_array_mut() {
            tools.push(added.clone());
            tools.push(deferred);
        }
        let (client, sent) = (anthropic(&incoming), anthropic(&outgoing));
        let seen = observation(
            ApiFamily::AnthropicMessages,
            &client,
            &sent,
            Compared::default(),
            5,
            None,
        );
        let arrived = seen.system_bytes + seen.tools_bytes;
        assert!(arrived > 0);
        assert_eq!(
            u64::from(seen.opening_bytes),
            u64::from(arrived) + size_of(&added)
        );
        let unchanged = observation(
            ApiFamily::AnthropicMessages,
            &client,
            &client,
            Compared::default(),
            5,
            None,
        );
        assert_eq!(unchanged.opening_bytes, arrived);
    }

    #[test]
    fn volatile_values_are_reported_only_where_they_are_news() {
        let mut body = turn(json!([{"role": "user", "content": "2026-11-20"}]));
        body["system"][0]["text"] = json!("Today's date is 2026-11-20.");
        let mut segments = anthropic(&body);
        assert!(!segments.volatile.any(), "reducing a body does not scan it");
        segments.scan_volatile(&body, ApiFamily::AnthropicMessages);
        let first = observation(
            ApiFamily::AnthropicMessages,
            &segments,
            &segments,
            Compared::default(),
            10,
            None,
        );
        assert_eq!(
            first.volatile.map(|v| v.dates),
            Some(1),
            "the message is not scanned"
        );
        let settled = Compared {
            compared: true,
            previous_one_hour: Some(true),
            ..Compared::default()
        };
        let later = observation(
            ApiFamily::AnthropicMessages,
            &segments,
            &segments,
            settled,
            10,
            Some(Idle {
                idle_ms: 42,
                was_warm: true,
                continues: true,
                opened_with: Some(18_300),
            }),
        );
        assert_eq!(later.volatile, None);
        assert_eq!(
            (later.idle_ms, later.was_warm, later.lifetime_s),
            (Some(42), true, Some(3_600))
        );
        assert_eq!((first.continues, later.continues), (None, Some(true)));
        assert_eq!((first.shared_read, later.shared_read), (0, 18_300));
        assert_eq!(first.lifetime_s, None, "no earlier request to have asked");
    }

    /// The scan is paid for only on a request whose result would be
    /// reported: one with nothing before it, a branch, or one whose
    /// instructions or tools moved.
    #[test]
    fn the_volatile_scan_is_asked_for_only_where_it_is_news() {
        let tracker = DriftTracker::default();
        let one = turn(json!([{"role": "user", "content": "hello"}]));
        let two = turn(json!([
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "hi"},
            {"role": "user", "content": "again"}
        ]));
        assert!(tracker.opening_is_news(7, &anthropic(&one)), "never seen");
        observe(&tracker, &one, &one);
        assert!(
            !tracker.opening_is_news(7, &anthropic(&two)),
            "an appended turn"
        );
        observe(&tracker, &two, &two);
        assert!(!tracker.opening_is_news(7, &anthropic(&two)));
        assert!(tracker.opening_is_news(7, &anthropic(&one)), "a branch");
        let mut retooled = two.clone();
        retooled["tools"][0]["description"] = json!("Read a file at 2026-11-20T10:00:00Z");
        assert!(tracker.opening_is_news(7, &anthropic(&retooled)));
        let mut reworded = two.clone();
        reworded["system"][0]["text"] = json!("You are another agent.");
        assert!(tracker.opening_is_news(7, &anthropic(&reworded)));
        // Whatever asks, the answer agrees with what the comparison reports.
        for body in [&two, &retooled, &reworded] {
            let tracker = DriftTracker::default();
            observe(&tracker, &two, &two);
            let news = tracker.opening_is_news(7, &anthropic(body));
            let seen = observe(&tracker, body, body);
            let reported = seen.drift_in
                & (DRIFT_SYSTEM | DRIFT_TOOL_DEFS | DRIFT_TOOL_ORDER | DRIFT_TOOL_SET);
            assert_eq!(news, reported != 0);
        }
    }

    /// Instructions on every family, and descriptions of rendered tools only.
    #[test]
    fn the_volatile_scan_reads_instructions_and_rendered_tool_descriptions() {
        let scanned = |body: &Value, family| {
            let mut segments = Segments::of(body, family, &HeaderMap::new(), None);
            segments.scan_volatile(body, family);
            segments.volatile
        };
        let anthropic = json!({
            "system": "Session 123e4567-e89b-12d3-a456-426614174000.",
            "tools": [
                {"name": "a", "description": "Built 2026-11-20.", "input_schema": {}},
                {"name": "b", "description": "Built 2026-11-19.", "input_schema": {},
                 "defer_loading": true}
            ],
            "messages": [{"role": "user", "content": "2026-11-18"}]
        });
        let found = scanned(&anthropic, ApiFamily::AnthropicMessages);
        assert_eq!(
            (found.uuids, found.dates),
            (1, 1),
            "not the deferred tool, not the message"
        );
        let chat = json!({"messages": [
            {"role": "system", "content": "Today is 2026-11-20."},
            {"role": "user", "content": "2026-11-18"},
            {"role": "system", "content": "Later: 2026-11-17."}
        ], "tools": [{"type": "function",
                      "function": {"name": "f", "description": "Since 2026-11-16."}}]});
        assert_eq!(scanned(&chat, ApiFamily::OpenAiChat).dates, 2);
        let responses = json!({"instructions": "Today is 2026-11-20.", "input": "2026-11-18"});
        assert_eq!(scanned(&responses, ApiFamily::OpenAiResponses).dates, 1);
    }

    /// A request that went out as it arrived after all is remembered so.
    #[test]
    fn a_body_sent_as_received_is_what_the_next_request_is_compared_with() {
        let tracker = DriftTracker::default();
        let one = turn(json!([{"role": "user", "content": "hello"}]));
        let rewritten = turn(json!([{"role": "user", "content": "h…"}]));
        let two = turn(json!([
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "hi"}
        ]));
        let segments = |body| anthropic(body);
        // Believed sent rewritten, the next request reads as the proxy
        // putting a message back.
        tracker.observe(7, &segments(&one), &segments(&rewritten), 1);
        assert_eq!(observe(&tracker, &two, &two).drift_out, DRIFT_HISTORY);

        let tracker = DriftTracker::default();
        tracker.observe(7, &segments(&one), &segments(&rewritten), 1);
        tracker.sent_as_received(7, 2);
        tracker.sent_as_received(8, 1);
        assert_eq!(
            tracker.entries.lock().unwrap()[&7].outgoing,
            segments(&rewritten).hashes,
            "another request's correction, and another conversation's, change nothing"
        );
        tracker.sent_as_received(7, 1);
        assert_eq!(observe(&tracker, &two, &two).drift_out, 0);
    }

    /// What observing a request costs, measured on the committed Claude Code
    /// capture: reducing it to segments as every request is, against doing
    /// that with the whole-body count and the volatile scan this used to make
    /// on each one. Prints; asserts nothing. Run with
    /// `cargo test --release -p kompressor-proxy --lib observing_a_request -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement, not a check"]
    fn observing_a_request_costs_this_much() {
        use std::io::Read;
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/claude-capture.jsonl.gz"
        );
        let mut text = String::new();
        flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap())
            .read_to_string(&mut text)
            .unwrap();
        let bodies: Vec<(usize, Value)> = text
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|record| {
                let raw = record["body"].as_str()?;
                Some((raw.len(), serde_json::from_str(raw).ok()?))
            })
            .collect();
        let family = ApiFamily::AnthropicMessages;
        let headers = HeaderMap::new();
        let rounds = 20;
        let per_request =
            |took: std::time::Duration| took.as_secs_f64() * 1e6 / (rounds * bodies.len()) as f64;
        let began = Instant::now();
        for _ in 0..rounds {
            for (_, body) in &bodies {
                let mut segments = Segments::of(body, family, &headers, None);
                segments.scan_volatile(body, family);
                std::hint::black_box(segments);
            }
        }
        let before = per_request(began.elapsed());
        let began = Instant::now();
        for _ in 0..rounds {
            for (len, body) in &bodies {
                std::hint::black_box(Segments::sized(body, *len as u64, family, &headers, None));
            }
        }
        let after = per_request(began.elapsed());
        let bytes = bodies.iter().map(|(len, _)| len).sum::<usize>() / bodies.len();
        eprintln!(
            "{} requests averaging {bytes} bytes: {before:.0} µs each counted and scanned, \
             {after:.0} µs each as observed now ({:.0}% less)",
            bodies.len(),
            (1.0 - after / before) * 100.0
        );
    }

    /// A tracker that cannot be read asks for no scan and corrects nothing.
    #[test]
    fn a_poisoned_tracker_asks_for_no_scan() {
        let tracker = std::sync::Arc::new(DriftTracker::default());
        let one = turn(json!([{"role": "user", "content": "hello"}]));
        observe(&tracker, &one, &one);
        let poisoner = tracker.clone();
        let _ = std::thread::spawn(move || {
            let _held = poisoner.entries.lock().unwrap();
            panic!("poison the tracker");
        })
        .join();
        assert!(!tracker.opening_is_news(7, &anthropic(&one)));
        assert!(!tracker.opening_is_news(8, &anthropic(&one)));
        tracker.sent_as_received(7, 0);
        assert_eq!(observe(&tracker, &one, &one), Compared::default());
    }

    /// The fingerprint the body capture records is the hash drift compares:
    /// the same for the same set however it is spelt, different for another.
    #[test]
    fn the_beta_fingerprint_is_of_the_set() {
        let with = |value: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert("anthropic-beta", http::HeaderValue::from_static(value));
            beta_fingerprint(&headers)
        };
        assert_eq!(with("a-2025, b-2025"), with("B-2025,a-2025,a-2025"));
        assert_ne!(with("a-2025"), with("a-2025,b-2025"));
        assert_eq!(
            beta_fingerprint(&HeaderMap::new()),
            anthropic(&turn(json!([]))).hashes.beta
        );
    }

    #[test]
    fn a_native_tool_search_entry_is_noticed() {
        let mut body = turn(json!([]));
        assert!(!anthropic(&body).tool_search);
        body["tools"].as_array_mut().unwrap().push(
            json!({"type": "tool_search_tool_regex_20251119", "name": "tool_search_tool_regex"}),
        );
        let seen = anthropic(&body);
        assert!(seen.tool_search);
        assert_eq!(
            (seen.tools_count, seen.tools_deferred),
            (3, 0),
            "and it is rendered"
        );
        let openai = json!({"input": "go", "tools": [
            {"type": "function", "name": "shell", "parameters": {}},
            {"type": "tool_search"}
        ]});
        assert!(
            Segments::of(&openai, ApiFamily::OpenAiResponses, &HeaderMap::new(), None).tool_search
        );
        // A tool the client merely named that way is a function like any other.
        let named = json!({"tools": [{"name": "tool_search", "input_schema": {}}], "messages": []});
        assert!(!anthropic(&named).tool_search);
    }

    #[test]
    fn history_past_the_kept_hashes_is_still_compared() {
        let messages = |changed: &str, extra: usize| {
            let mut all: Vec<Value> = (0..MESSAGES_KEPT + 3)
                .map(|index| json!({"role": "user", "content": format!("m{index}")}))
                .collect();
            all.push(json!({"role": "user", "content": changed}));
            all.extend(
                (0..extra).map(|index| json!({"role": "user", "content": format!("n{index}")})),
            );
            turn(Value::Array(all))
        };
        let count = MESSAGES_KEPT + 4;
        let at = |body: &Value| {
            Segments::of(
                body,
                ApiFamily::AnthropicMessages,
                &HeaderMap::new(),
                Some(count),
            )
        };
        let one = at(&messages("x", 0));
        assert_eq!(one.hashes.messages.len(), MESSAGES_KEPT);
        // Grown by two turns, and unchanged up to where the last one ended.
        assert_eq!(changed(&one.hashes, &at(&messages("x", 2)).hashes).0, 0);
        // Grown, and changed past the kept hashes.
        assert_eq!(
            changed(&one.hashes, &at(&messages("y", 2)).hashes),
            (DRIFT_HISTORY, Some(MESSAGES_KEPT as u32))
        );
    }

    #[test]
    fn the_table_stays_bounded() {
        let tracker = DriftTracker::default();
        let segments = anthropic(&turn(json!([])));
        for conversation in 0..(REMEMBER as u64 + 16) {
            tracker.observe(conversation, &segments, &segments, 0);
        }
        assert_eq!(tracker.tracked(), REMEMBER);
    }
}
