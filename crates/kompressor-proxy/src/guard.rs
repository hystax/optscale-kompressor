//! Checks on the body going upstream, against the body the client sent.
//!
//! Every rewrite this proxy makes is meant to leave certain things alone, and
//! the provider refuses the whole request with a 400 when one of them moves:
//! a signed `thinking` block whose bytes changed, a fifth `cache_control`
//! breakpoint, a one-hour breakpoint behind a five-minute one, a `tool_result`
//! whose `tool_use` is gone. Each rewrite is tested on its own; these are the
//! assertion that their sum, re-serialisation included, still holds — made
//! once, on the bytes about to be sent, after every rewrite.
//!
//! # Only what the rewrite introduced
//!
//! A guard compares the outgoing body with the client's and trips only on a
//! fault the client's body did not already have. A client that sent five
//! breakpoints gets a 400 whether or not this proxy touched the body, and
//! forwarding its original instead would not prevent it; those are counted
//! apart, as `client_invalid`, and never trip.
//!
//! # Modes
//!
//! Each guard has its own setting ([`GuardMode`]): `observe`, the default,
//! counts and logs a trip and sends the rewritten body anyway, so a guard
//! that is wrong cannot turn a request the provider would have accepted into
//! a fail-open; `enforce` sends the client's original instead, as every other
//! fail-open does, and counts it on [`crate::Stage::Guard`] too; `off` skips
//! the check. The counts are on [`Guards::snapshot`], on the Maintenance
//! page's memory section, and in the warning each trip logs.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

use kompressor_core::config::GuardMode;
use kompressor_wire::ApiFamily;
use serde_json::Value;

/// One of the checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Guard {
    /// `proxy.guard_thinking`: signed reasoning goes out as it came.
    Thinking,
    /// `proxy.guard_upstream_body`: breakpoints and tool pairing.
    UpstreamBody,
}

impl Guard {
    /// The label this guard carries in counts and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Guard::Thinking => "thinking",
            Guard::UpstreamBody => "upstream_body",
        }
    }
}

/// Trip counts for one proxy, since it started.
#[derive(Debug, Default)]
pub struct Guards {
    thinking: AtomicU64,
    upstream_body: AtomicU64,
    /// Requests whose client body already had a fault a guard looks for.
    client_invalid: AtomicU64,
    /// Trips under `enforce`, which sent the client's original.
    enforced: AtomicU64,
}

impl Guards {
    /// Count a trip and say why.
    pub fn trip(&self, guard: Guard, mode: GuardMode, reason: &str) {
        let counter = match guard {
            Guard::Thinking => &self.thinking,
            Guard::UpstreamBody => &self.upstream_body,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        let enforced = mode == GuardMode::Enforce;
        if enforced {
            self.enforced.fetch_add(1, Ordering::Relaxed);
        }
        tracing::warn!(
            guard = guard.as_str(),
            enforced,
            reason,
            "a stability guard tripped on the rewritten body; {}",
            if enforced {
                "forwarding the request as it arrived"
            } else {
                "observe-only, so the rewritten body goes out"
            }
        );
    }

    /// Count a client body that already had a fault, which does not trip.
    pub fn client_invalid(&self, reason: &str) {
        self.client_invalid.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            reason,
            "the client's own body already fails a stability guard"
        );
    }

    /// Every count, by name.
    pub fn snapshot(&self) -> BTreeMap<&'static str, u64> {
        BTreeMap::from([
            ("thinking", self.thinking.load(Ordering::Relaxed)),
            ("upstream_body", self.upstream_body.load(Ordering::Relaxed)),
            (
                "client_invalid",
                self.client_invalid.load(Ordering::Relaxed),
            ),
            ("enforced", self.enforced.load(Ordering::Relaxed)),
        ])
    }

    /// Trips of one guard.
    pub fn trips(&self, guard: Guard) -> u64 {
        match guard {
            Guard::Thinking => self.thinking.load(Ordering::Relaxed),
            Guard::UpstreamBody => self.upstream_body.load(Ordering::Relaxed),
        }
    }

    /// Trips of every guard together.
    pub fn total(&self) -> u64 {
        self.trips(Guard::Thinking) + self.trips(Guard::UpstreamBody)
    }

    /// Trips that sent the client's original.
    pub fn enforced(&self) -> u64 {
        self.enforced.load(Ordering::Relaxed)
    }
}

/// What one check found.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing the rewrite introduced.
    Pass,
    /// The client's body already had the fault, so the rewrite is not to
    /// blame for it; the reason names it.
    ClientInvalid(String),
    /// The rewrite introduced a fault; the reason names it.
    Trip(String),
}

// ------------------------------------------------------------------ thinking

/// Signed reasoning, in prompt order, as the canonical JSON of each block.
///
/// Anthropic `thinking` and `redacted_thinking` content blocks, signature and
/// all; Responses `reasoning` input items that carry `encrypted_content`. The
/// canonical form is `serde_json`'s, which keeps key order and numbers exact,
/// so two blocks compare equal exactly when every field does.
fn signed_reasoning(body: &Value, family: ApiFamily) -> Vec<String> {
    let mut found = Vec::new();
    match family {
        ApiFamily::AnthropicMessages => {
            for message in array(body, "messages") {
                let Some(Value::Array(blocks)) = message.get("content") else {
                    continue;
                };
                for block in blocks {
                    if matches!(
                        block.get("type").and_then(Value::as_str),
                        Some("thinking" | "redacted_thinking")
                    ) {
                        found.push(block.to_string());
                    }
                }
            }
        }
        ApiFamily::OpenAiResponses => {
            for item in array(body, "input") {
                if item.get("type").and_then(Value::as_str) == Some("reasoning")
                    && item.get("encrypted_content").is_some()
                {
                    found.push(item.to_string());
                }
            }
        }
        ApiFamily::OpenAiChat => {}
        // A Gemini part with a `thoughtSignature`: verified against its bytes.
        ApiFamily::GeminiGenerateContent => {
            for content in array(body, "contents") {
                for part in content
                    .get("parts")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if part.get("thoughtSignature").is_some()
                        || part.get("thought_signature").is_some()
                    {
                        found.push(part.to_string());
                    }
                }
            }
        }
        // Converse `reasoningContent` carries a signature as Anthropic's does.
        ApiFamily::BedrockConverse => {
            for message in array(body, "messages") {
                for block in message
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if block.get("reasoningContent").is_some() {
                        found.push(block.to_string());
                    }
                }
            }
        }
    }
    found
}

/// Whether every signed reasoning block of `original` goes out in `sent`,
/// byte for byte and in order, and nothing was added.
pub fn check_thinking(original: &Value, sent: &Value, family: ApiFamily) -> Verdict {
    let before = signed_reasoning(original, family);
    let after = signed_reasoning(sent, family);
    if before == after {
        return Verdict::Pass;
    }
    if before.len() != after.len() {
        return Verdict::Trip(format!(
            "{} signed reasoning blocks arrived and {} would go out",
            before.len(),
            after.len()
        ));
    }
    let at = before
        .iter()
        .zip(&after)
        .position(|(a, b)| a != b)
        .unwrap_or(0);
    Verdict::Trip(format!(
        "signed reasoning block {at} differs from the client's"
    ))
}

// ------------------------------------------------------------- upstream body

/// The faults [`check_upstream_body`] looks for in one Anthropic body.
#[derive(Debug, Default, PartialEq, Eq)]
struct Faults {
    /// Breakpoints, when there are more than the four the API allows.
    too_many: Option<usize>,
    /// A one-hour breakpoint after a five-minute one.
    ttl_out_of_order: bool,
    /// `tool_use_id`s with no `tool_use` in the assistant message before.
    unpaired: BTreeSet<String>,
}

/// The most `cache_control` breakpoints one Anthropic request may carry.
const MAX_BREAKPOINTS: usize = 4;

/// Every breakpoint's TTL, in the order the provider renders the prompt:
/// tools, system, messages — and a top-level marker, which applies to the
/// last block, at the end. `true` is one hour; anything else, five minutes.
fn breakpoints(body: &Value) -> Vec<bool> {
    fn marker(block: &Value, out: &mut Vec<bool>) {
        if let Some(marker) = block.get("cache_control").filter(|m| m.is_object()) {
            out.push(marker.get("ttl").and_then(Value::as_str) == Some("1h"));
        }
    }
    let mut out = Vec::new();
    for tool in array(body, "tools") {
        marker(tool, &mut out);
    }
    for block in array(body, "system") {
        marker(block, &mut out);
    }
    for message in array(body, "messages") {
        let Some(Value::Array(blocks)) = message.get("content") else {
            continue;
        };
        for block in blocks {
            marker(block, &mut out);
            if block.get("type").and_then(Value::as_str) == Some("tool_result")
                && let Some(Value::Array(inner)) = block.get("content")
            {
                for inner in inner {
                    marker(inner, &mut out);
                }
            }
        }
    }
    marker(body, &mut out);
    out
}

/// `tool_result` ids with no `tool_use` of that id in the assistant message
/// directly before theirs.
fn unpaired_tool_results(body: &Value) -> BTreeSet<String> {
    let mut unpaired = BTreeSet::new();
    let mut offered: BTreeSet<&str> = BTreeSet::new();
    for message in array(body, "messages") {
        let blocks = match message.get("content") {
            Some(Value::Array(blocks)) => blocks.as_slice(),
            _ => &[],
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                let id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !offered.contains(id) {
                    unpaired.insert(id.to_string());
                }
            }
        }
        // What the next message may answer: this one's calls, if it is the
        // assistant's, and nothing otherwise.
        offered = if message.get("role").and_then(Value::as_str) == Some("assistant") {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
                .filter_map(|block| block.get("id").and_then(Value::as_str))
                .collect()
        } else {
            BTreeSet::new()
        };
    }
    unpaired
}

fn faults(body: &Value) -> Faults {
    let ttls = breakpoints(body);
    // Once a five-minute breakpoint has been seen, a one-hour one after it is
    // out of order.
    let ttl_out_of_order = ttls
        .iter()
        .skip_while(|one_hour| **one_hour)
        .any(|one_hour| *one_hour);
    Faults {
        too_many: (ttls.len() > MAX_BREAKPOINTS).then_some(ttls.len()),
        ttl_out_of_order,
        unpaired: unpaired_tool_results(body),
    }
}

/// Whether the Anthropic Messages body going upstream has a fault its
/// original did not. Bodies of other families always pass.
pub fn check_upstream_body(original: &Value, sent: &Value, family: ApiFamily) -> Verdict {
    if family != ApiFamily::AnthropicMessages {
        return Verdict::Pass;
    }
    let before = faults(original);
    let after = faults(sent);
    let mut introduced = Vec::new();
    let mut inherited = Vec::new();
    match (before.too_many, after.too_many) {
        (None, Some(count)) => introduced.push(format!(
            "{count} cache_control breakpoints, over the {MAX_BREAKPOINTS} allowed"
        )),
        (Some(count), Some(_)) => inherited.push(format!("{count} cache_control breakpoints")),
        _ => {}
    }
    match (before.ttl_out_of_order, after.ttl_out_of_order) {
        (false, true) => {
            introduced.push("a 1h cache_control breakpoint after a 5m one".to_string())
        }
        (true, true) => inherited.push("a 1h breakpoint after a 5m one".to_string()),
        _ => {}
    }
    let new: Vec<&String> = after.unpaired.difference(&before.unpaired).collect();
    if !new.is_empty() {
        introduced.push(format!("tool_result without its tool_use: {new:?}"));
    }
    if !before.unpaired.is_empty() && !after.unpaired.is_empty() {
        inherited.push(format!(
            "tool_result without its tool_use: {:?}",
            before.unpaired
        ));
    }
    if !introduced.is_empty() {
        Verdict::Trip(introduced.join("; "))
    } else if !inherited.is_empty() {
        Verdict::ClientInvalid(inherited.join("; "))
    } else {
        Verdict::Pass
    }
}

fn array<'a>(body: &'a Value, key: &str) -> &'a [Value] {
    match body.get(key) {
        Some(Value::Array(items)) => items,
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ANTHROPIC: ApiFamily = ApiFamily::AnthropicMessages;

    fn with_thinking() -> Value {
        json!({
            "model": "claude",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "let me see", "signature": "c2lnbmVk"},
                    {"type": "redacted_thinking", "data": "b3BhcXVl"},
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "a.txt\nb.txt"}
                ]}
            ]
        })
    }

    #[test]
    fn unchanged_thinking_passes_whatever_else_was_rewritten() {
        let original = with_thinking();
        let mut sent = original.clone();
        sent["messages"][2]["content"][0]["content"] = json!("[... compressed]");
        assert_eq!(check_thinking(&original, &sent, ANTHROPIC), Verdict::Pass);
    }

    #[test]
    fn a_changed_signature_or_thinking_text_trips() {
        let original = with_thinking();
        for (pointer, value) in [
            ("/messages/1/content/0/signature", json!("Zm9yZ2Vk")),
            ("/messages/1/content/0/thinking", json!("let me see ")),
            ("/messages/1/content/1/data", json!("")),
        ] {
            let mut sent = original.clone();
            *sent.pointer_mut(pointer).unwrap() = value;
            assert!(
                matches!(
                    check_thinking(&original, &sent, ANTHROPIC),
                    Verdict::Trip(_)
                ),
                "{pointer}"
            );
        }
    }

    #[test]
    fn a_dropped_reordered_or_added_thinking_block_trips() {
        let original = with_thinking();
        let mut dropped = original.clone();
        dropped["messages"][1]["content"]
            .as_array_mut()
            .unwrap()
            .remove(1);
        let mut swapped = original.clone();
        swapped["messages"][1]["content"]
            .as_array_mut()
            .unwrap()
            .swap(0, 1);
        let mut added = original.clone();
        added["messages"][1]["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "redacted_thinking", "data": "eA=="}));
        for sent in [dropped, swapped, added] {
            assert!(matches!(
                check_thinking(&original, &sent, ANTHROPIC),
                Verdict::Trip(_)
            ));
        }
    }

    #[test]
    fn responses_reasoning_with_encrypted_content_is_held_to_the_same_rule() {
        let original = json!({"input": [
            {"type": "message", "role": "user", "content": "hi"},
            {"type": "reasoning", "summary": [], "encrypted_content": "gAAAA"},
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "plain"}]}
        ]});
        let mut sent = original.clone();
        sent["input"][2]["summary"] = json!([]);
        assert_eq!(
            check_thinking(&original, &sent, ApiFamily::OpenAiResponses),
            Verdict::Pass,
            "an unsigned item is not held"
        );
        sent["input"][1]["encrypted_content"] = json!("gAAAB");
        assert!(matches!(
            check_thinking(&original, &sent, ApiFamily::OpenAiResponses),
            Verdict::Trip(_)
        ));
    }

    fn marked(ttl: Option<&str>) -> Value {
        match ttl {
            Some(ttl) => json!({"type": "ephemeral", "ttl": ttl}),
            None => json!({"type": "ephemeral"}),
        }
    }

    /// Tools, system and two messages, each marked with the TTL given.
    fn with_breakpoints(ttls: [Option<Option<&str>>; 4]) -> Value {
        let mut body = json!({
            "model": "claude",
            "tools": [{"name": "Bash", "input_schema": {"type": "object"}}],
            "system": [{"type": "text", "text": "rules"}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "one"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "two"}]}
            ]
        });
        let slots = [
            "/tools/0",
            "/system/0",
            "/messages/0/content/0",
            "/messages/1/content/0",
        ];
        for (slot, ttl) in slots.iter().zip(ttls) {
            if let Some(ttl) = ttl {
                body.pointer_mut(slot).unwrap()["cache_control"] = marked(ttl);
            }
        }
        body
    }

    #[test]
    fn a_valid_body_passes() {
        let body = with_breakpoints([Some(Some("1h")), Some(None), Some(None), Some(None)]);
        assert_eq!(check_upstream_body(&body, &body, ANTHROPIC), Verdict::Pass);
        let paired = with_thinking();
        assert_eq!(
            check_upstream_body(&paired, &paired, ANTHROPIC),
            Verdict::Pass
        );
    }

    #[test]
    fn a_fifth_breakpoint_the_rewrite_added_trips() {
        let original = with_breakpoints([Some(None), Some(None), Some(None), Some(None)]);
        let mut sent = original.clone();
        sent["cache_control"] = marked(None);
        let Verdict::Trip(reason) = check_upstream_body(&original, &sent, ANTHROPIC) else {
            panic!("five breakpoints must trip");
        };
        assert!(reason.contains("5 cache_control"), "{reason}");
    }

    #[test]
    fn a_one_hour_breakpoint_after_a_five_minute_one_trips() {
        let original = with_breakpoints([Some(Some("1h")), Some(None), None, Some(None)]);
        let mut sent = original.clone();
        sent["messages"][1]["content"][0]["cache_control"] = marked(Some("1h"));
        assert!(matches!(
            check_upstream_body(&original, &sent, ANTHROPIC),
            Verdict::Trip(reason) if reason.contains("1h")
        ));
        // Every breakpoint at one hour is in order.
        let all = with_breakpoints([Some(Some("1h")); 4]);
        assert_eq!(
            check_upstream_body(&original, &all, ANTHROPIC),
            Verdict::Pass
        );
    }

    #[test]
    fn a_tool_result_whose_tool_use_went_missing_trips() {
        let original = with_thinking();
        let mut sent = original.clone();
        sent["messages"][1]["content"][2]["id"] = json!("toolu_other");
        assert!(matches!(
            check_upstream_body(&original, &sent, ANTHROPIC),
            Verdict::Trip(reason) if reason.contains("toolu_1")
        ));
        // A tool_use two messages back does not pair either.
        let mut late = original.clone();
        late["messages"]
            .as_array_mut()
            .unwrap()
            .insert(2, json!({"role": "user", "content": "wait"}));
        assert!(matches!(
            check_upstream_body(&original, &late, ANTHROPIC),
            Verdict::Trip(_)
        ));
    }

    #[test]
    fn a_fault_the_client_sent_does_not_trip() {
        // Five breakpoints and a 1h after a 5m, as the client sent them, and
        // a rewrite that kept both: forwarding the original would not help.
        let mut original = with_breakpoints([Some(None), Some(None), Some(None), Some(Some("1h"))]);
        original["cache_control"] = marked(None);
        let mut sent = original.clone();
        sent["messages"][0]["content"][0]["text"] = json!("compressed");
        assert!(matches!(
            check_upstream_body(&original, &sent, ANTHROPIC),
            Verdict::ClientInvalid(_)
        ));
        // An orphaned tool_result the client sent, likewise.
        let mut orphan = with_thinking();
        orphan["messages"][1]["content"][2]["id"] = json!("toolu_other");
        assert!(matches!(
            check_upstream_body(&orphan, &orphan.clone(), ANTHROPIC),
            Verdict::ClientInvalid(_)
        ));
        // But a new fault on top of an inherited one still trips.
        let mut worse = orphan.clone();
        worse["messages"][0]["content"] = json!([
            {"type": "tool_result", "tool_use_id": "toolu_new", "content": "x"}
        ]);
        assert!(matches!(
            check_upstream_body(&orphan, &worse, ANTHROPIC),
            Verdict::Trip(reason) if reason.contains("toolu_new")
        ));
    }

    #[test]
    fn other_families_are_not_checked_for_breakpoints() {
        let original = json!({"messages": []});
        let sent = json!({"messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "x"}
        ]}]});
        assert_eq!(
            check_upstream_body(&original, &sent, ApiFamily::OpenAiChat),
            Verdict::Pass
        );
    }

    #[test]
    fn trips_are_counted_per_guard_and_enforcement_apart() {
        let guards = Guards::default();
        guards.trip(Guard::Thinking, GuardMode::Observe, "x");
        guards.trip(Guard::UpstreamBody, GuardMode::Enforce, "y");
        guards.client_invalid("z");
        let snapshot = guards.snapshot();
        assert_eq!(snapshot["thinking"], 1);
        assert_eq!(snapshot["upstream_body"], 1);
        assert_eq!(snapshot["client_invalid"], 1);
        assert_eq!(snapshot["enforced"], 1);
        assert_eq!(guards.total(), 2);
    }
}
