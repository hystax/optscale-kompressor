//! Asking for shorter answers, as an experiment with a holdout.
//!
//! Output is about a sixth of the bill on Anthropic and nothing else in this
//! proxy touches it. `proxy.output_shaping` appends one fixed instruction —
//! answer concisely, do not restate — to the request's `system` prompt
//! (Anthropic), `instructions` (OpenAI Responses) or leading system message
//! (OpenAI Chat); see [`append_instruction_for`]. It
//! changes what the model does, which nothing else here does, so it is built
//! as an experiment: off by default, and with a share of conversations held out
//! so `/v1/stats/analysis/compare` can set treated against untreated over the
//! same window. Headroom ships the same idea (`proxy/output_shaper.py`), with
//! no published measurement.
//!
//! # Where the block goes, and why it never moves a cached byte
//!
//! Anthropic renders `tools`, then `system`, then `messages`, and a cache
//! entry is the prefix up to a block carrying `cache_control`. The block is
//! appended as the **last** `system` block, with no `cache_control` of its own:
//!
//! - Every breakpoint inside `tools` or `system` comes before it, so those
//!   entries — Claude Code's system-prompt entry is shared by every
//!   conversation on the machine, treated or not — keep their exact bytes.
//! - Every breakpoint in `messages` comes after it, so those entries include
//!   it. It is the same bytes on every request of a treated conversation, from
//!   the first, so each such entry is written with it and read back with it.
//!
//! The tail of the last user turn would also sit after every breakpoint, but
//! it is a different message on every turn: the turn that carried the block
//! becomes an earlier turn the client re-sends without it, and the entry
//! written over it no longer matches. The system position has no such turn.
//!
//! A `system` string is extended in place (a blank line, then the text) rather
//! than converted to blocks, so a client that sends a string keeps sending the
//! provider the same kind of value. `max_tokens`, thinking and effort are not
//! touched.
//!
//! # Decided once per conversation
//!
//! The arm is fixed on the first request this process sees of a conversation
//! and held for its life, the text with it, whatever the configuration later
//! says. A conversation first seen part-way through — under way before the
//! setting was on, or before a restart — is never treated, because the provider
//! already caches its prefix without the block; one treated before a restart
//! keeps its decision when `proxy.state_path` saves it (`persist`), and without
//! that is seen part-way through afterwards and loses the block, which costs
//! that conversation one cache write. Turning the setting off likewise leaves
//! conversations already treated as they are until they end.
//!
//! The arm itself is a hash of the conversation's opening, which is the same on
//! every turn and on every proxy, so a holdout conversation is a holdout
//! wherever it lands.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{Value, json};

use crate::toolcompact::OPENING_MESSAGES;

/// Conversations remembered, the same bound the tool policies keep.
const REMEMBER: usize = 4096;

/// Which side of the experiment a conversation is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arm {
    /// Sent the instruction.
    Treated,
    /// Would have been treated, and was held out to measure against.
    Holdout,
}

impl Arm {
    /// The name the statistics row carries.
    pub fn as_str(self) -> &'static str {
        match self {
            Arm::Treated => "treated",
            Arm::Holdout => "holdout",
        }
    }
}

/// The arm for a conversation, by a hash of its opening: held out when the
/// hash lands in the first `holdout_pct` of a hundred buckets.
///
/// The opening is already a hash, but of text, and other features key on it;
/// mixing it once more keeps this split independent of anything that groups by
/// the raw value.
pub fn arm_for(opening: u64, holdout_pct: u8) -> Arm {
    if splitmix64(opening ^ 0x6f75_7470_7574_7368) % 100 < u64::from(holdout_pct) {
        Arm::Holdout
    } else {
        Arm::Treated
    }
}

/// SplitMix64's finaliser: a fixed, well-mixed bijection on `u64`.
fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// What a conversation was decided: its arm and, when treated, the exact text
/// it is sent on every turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub arm: Arm,
    pub text: Option<Arc<str>>,
}

/// Each conversation's decision, taken on its first request.
#[derive(Default)]
pub struct OutputShaping {
    /// `None` for a conversation outside the experiment.
    decided: Mutex<HashMap<u64, (Instant, Option<Decision>)>>,
}

impl OutputShaping {
    /// The decision for this conversation: taken now if this is the first
    /// request seen of it, and returned unchanged on every later one.
    ///
    /// `conversation` keys the memory; `opening` picks the arm. `None` when
    /// the conversation is outside the experiment: the setting was off when it
    /// was first seen, or it arrived already `messages` long.
    pub fn decide(
        &self,
        conversation: u64,
        opening: u64,
        messages: usize,
        enabled: bool,
        holdout_pct: u8,
        text: &str,
    ) -> Option<Decision> {
        let Ok(mut decided) = self.decided.lock() else {
            return None;
        };
        if let Some((used, decision)) = decided.get_mut(&conversation) {
            // Refreshed on every use, so the cap evicts the conversation idle
            // longest, never one under way.
            *used = Instant::now();
            return decision.clone();
        }
        if decided.len() >= REMEMBER
            && let Some(oldest) = decided
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(key, _)| *key)
        {
            decided.remove(&oldest);
        }
        let decision = (enabled && messages <= OPENING_MESSAGES).then(|| {
            let arm = arm_for(opening, holdout_pct);
            Decision {
                arm,
                text: (arm == Arm::Treated).then(|| Arc::from(text)),
            }
        });
        decided.insert(conversation, (Instant::now(), decision.clone()));
        decision
    }

    /// Conversations with a decision. For tests and the Maintenance page.
    pub fn tracked(&self) -> usize {
        self.decided.lock().map_or(0, |decided| decided.len())
    }

    /// Every decision, for the state file. A treated conversation's text is
    /// the operator's, not the conversation's, and is written once per distinct
    /// text, so the file does not hold thousands of copies of one setting.
    pub(crate) fn snapshot(&self, clock: &crate::persist::Clock) -> ShapingRecord {
        let Ok(decided) = self.decided.lock() else {
            return ShapingRecord::default();
        };
        let mut record = ShapingRecord::default();
        for (&conversation, (at, decision)) in decided.iter() {
            let (arm, text) = match decision {
                None => (None, None),
                Some(Decision { arm, text }) => {
                    let text = text.as_ref().map(|text| {
                        match record.texts.iter().position(|kept| **kept == **text) {
                            Some(index) => index,
                            None => {
                                record.texts.push(text.to_string());
                                record.texts.len() - 1
                            }
                        }
                    });
                    (Some(arm.as_str().to_string()), text)
                }
            };
            record.decisions.push(ShapingDecisionRecord {
                conversation,
                age_ms: clock.age(*at),
                arm,
                text,
            });
        }
        record
    }

    /// Replace every decision with `record`'s, newest first up to the cap. A
    /// decision naming an arm or a text this file does not hold is dropped,
    /// which leaves that conversation to be seen part-way and go untreated:
    /// the outcome a restart had before decisions were saved.
    pub(crate) fn restore(&self, record: ShapingRecord, clock: &crate::persist::Clock) {
        let Ok(mut decided) = self.decided.lock() else {
            return;
        };
        let texts: Vec<Arc<str>> = record.texts.into_iter().map(Arc::from).collect();
        *decided = crate::prefixcache::newest(record.decisions, REMEMBER, |entry| entry.age_ms)
            .into_iter()
            .filter_map(|entry| {
                let decision = match entry.arm.as_deref() {
                    None => None,
                    Some("holdout") => Some(Decision {
                        arm: Arm::Holdout,
                        text: None,
                    }),
                    Some("treated") => Some(Decision {
                        arm: Arm::Treated,
                        text: Some(texts.get(entry.text?)?.clone()),
                    }),
                    Some(_) => return None,
                };
                Some((entry.conversation, (clock.at(entry.age_ms), decision)))
            })
            .collect();
    }
}

/// The output-shaping decisions in the state file.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct ShapingRecord {
    decisions: Vec<ShapingDecisionRecord>,
    /// The distinct instruction texts treated conversations were sent.
    texts: Vec<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ShapingDecisionRecord {
    conversation: u64,
    age_ms: u64,
    /// `treated`, `holdout`, or absent for a conversation outside the
    /// experiment, which must stay outside it.
    arm: Option<String>,
    /// Index into [`ShapingRecord::texts`] for a treated conversation.
    text: Option<usize>,
}

/// Append `text` to the request's system prompt, after every block that can
/// carry a breakpoint. Returns whether the body changed.
pub fn append_instruction(body: &mut Value, text: &str) -> bool {
    let Some(object) = body.as_object_mut() else {
        return false;
    };
    match object.get_mut("system") {
        None | Some(Value::Null) => {
            object.insert(
                "system".to_string(),
                json!([{"type": "text", "text": text}]),
            );
            true
        }
        Some(Value::String(system)) => {
            if !system.is_empty() {
                system.push_str("\n\n");
            }
            system.push_str(text);
            true
        }
        Some(Value::Array(blocks)) => {
            blocks.push(json!({"type": "text", "text": text}));
            true
        }
        // A shape the API does not accept; leave it for the provider to refuse.
        Some(_) => false,
    }
}

/// Append `text` where `family` puts the instructions that precede the
/// conversation. Returns whether the body changed.
///
/// - Anthropic Messages: [`append_instruction`].
/// - OpenAI Responses: the tail of `instructions`, which renders before
///   `input` and which Codex re-sends verbatim every turn; created when absent.
/// - OpenAI Chat: the first message, when it is `system` or `developer`
///   (string content gets a blank line and the text, part-list content a new
///   text part); otherwise a `system` message inserted at the front. The
///   decision is made on a conversation's first request, so every turn of a
///   treated conversation has the same first message.
pub fn append_instruction_for(
    body: &mut Value,
    family: kompressor_wire::ApiFamily,
    text: &str,
) -> bool {
    use kompressor_wire::ApiFamily;
    match family {
        ApiFamily::OpenAiResponses => append_to_instructions(body, text),
        ApiFamily::OpenAiChat => append_to_chat(body, text),
        ApiFamily::AnthropicMessages => append_instruction(body, text),
        ApiFamily::GeminiGenerateContent => {
            let key = if body.get("systemInstruction").is_none()
                && body.get("system_instruction").is_some()
            {
                "system_instruction"
            } else {
                "systemInstruction"
            };
            append_part(body, key, text)
        }
        ApiFamily::BedrockConverse => append_converse_system(body, text),
    }
}

/// Gemini: one more text part at the end of `systemInstruction`.
fn append_part(body: &mut Value, key: &str, text: &str) -> bool {
    let Some(object) = body.as_object_mut() else {
        return false;
    };
    let instruction = object
        .entry(key.to_string())
        .or_insert_with(|| json!({"parts": []}));
    let Some(instruction) = instruction.as_object_mut() else {
        return false;
    };
    match instruction
        .entry("parts".to_string())
        .or_insert_with(|| json!([]))
    {
        Value::Array(parts) => {
            parts.push(json!({"text": text}));
            true
        }
        _ => false,
    }
}

/// Bedrock Converse: one more `{"text"}` block at the end of `system`, after
/// any `cachePoint`, so no cached byte moves.
fn append_converse_system(body: &mut Value, text: &str) -> bool {
    let Some(object) = body.as_object_mut() else {
        return false;
    };
    match object
        .entry("system".to_string())
        .or_insert_with(|| json!([]))
    {
        Value::Array(blocks) => {
            blocks.push(json!({"text": text}));
            true
        }
        _ => false,
    }
}

fn append_to_instructions(body: &mut Value, text: &str) -> bool {
    let Some(object) = body.as_object_mut() else {
        return false;
    };
    match object.get_mut("instructions") {
        None | Some(Value::Null) => {
            object.insert("instructions".to_string(), Value::String(text.to_string()));
            true
        }
        Some(Value::String(instructions)) => {
            if !instructions.is_empty() {
                instructions.push_str("\n\n");
            }
            instructions.push_str(text);
            true
        }
        Some(_) => false,
    }
}

fn append_to_chat(body: &mut Value, text: &str) -> bool {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return false;
    };
    let leads = messages.first().is_some_and(|first| {
        matches!(
            first.get("role").and_then(Value::as_str),
            Some("system" | "developer")
        )
    });
    if !leads {
        messages.insert(0, json!({"role": "system", "content": text}));
        return true;
    }
    match messages[0].get_mut("content") {
        Some(Value::String(content)) => {
            if !content.is_empty() {
                content.push_str("\n\n");
            }
            content.push_str(text);
            true
        }
        Some(Value::Array(parts)) => {
            parts.push(json!({"type": "text", "text": text}));
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "Be concise.";

    /// A restart with saved state keeps every arm, and the exact text, so a
    /// treated conversation is not seen part-way and stripped of its block.
    #[test]
    fn decisions_survive_the_state_file() {
        let before = OutputShaping::default();
        let treated = (0u64..).find(|o| arm_for(*o, 50) == Arm::Treated).unwrap();
        let held = (0u64..).find(|o| arm_for(*o, 50) == Arm::Holdout).unwrap();
        let a = before.decide(1, treated, 1, true, 50, TEXT);
        let b = before.decide(2, held, 1, true, 50, TEXT);
        let c = before.decide(3, treated, 9, true, 50, TEXT);
        let d = before.decide(4, treated, 1, true, 50, TEXT);
        let clock = crate::persist::Clock::writing();
        let json = serde_json::to_string(&before.snapshot(&clock)).unwrap();
        assert_eq!(json.matches(TEXT).count(), 1, "one copy per distinct text");
        let after = OutputShaping::default();
        after.restore(serde_json::from_str(&json).unwrap(), &clock);
        // Off now, and every conversation is far along: only the saved
        // decisions can produce these answers.
        assert_eq!(after.decide(1, treated, 40, false, 50, "other"), a);
        assert_eq!(after.decide(2, held, 40, false, 50, "other"), b);
        assert_eq!(after.decide(3, treated, 40, true, 50, "other"), c);
        assert_eq!(after.decide(4, treated, 40, false, 50, "other"), d);
        assert!(a.unwrap().text.is_some() && c.is_none());
    }

    #[test]
    fn the_arm_is_a_pure_function_of_the_opening() {
        for opening in [0u64, 1, 42, u64::MAX, 0xdead_beef] {
            assert_eq!(arm_for(opening, 50), arm_for(opening, 50));
        }
        assert!((0..1000).all(|o| arm_for(o, 100) == Arm::Holdout));
        assert!((0..1000).all(|o| arm_for(o, 0) == Arm::Treated));
    }

    #[test]
    fn the_holdout_share_is_close_to_the_configured_one() {
        for pct in [30u8, 50, 70] {
            let n = 20_000u64;
            let held = (0..n)
                .map(|i| i.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0x1234)
                .filter(|&opening| arm_for(opening, pct) == Arm::Holdout)
                .count() as f64;
            let share = held / n as f64 * 100.0;
            assert!(
                (share - f64::from(pct)).abs() < 1.5,
                "{pct}% configured, {share:.2}% held out"
            );
        }
    }

    #[test]
    fn decided_on_the_first_request_and_held() {
        let shaping = OutputShaping::default();
        // Find an opening that is treated at 30%.
        let opening = (0..).find(|&o| arm_for(o, 30) == Arm::Treated).unwrap();
        let first = shaping.decide(1, opening, 1, true, 30, TEXT).unwrap();
        assert_eq!(first.arm, Arm::Treated);
        assert_eq!(first.text.as_deref(), Some(TEXT));
        // The setting is turned off, and the text changed: the conversation
        // keeps what it had.
        assert_eq!(
            shaping.decide(1, opening, 5, false, 30, "other"),
            Some(first.clone())
        );
        assert_eq!(
            shaping.decide(1, opening, 7, true, 90, "other"),
            Some(first)
        );
    }

    #[test]
    fn a_conversation_first_seen_off_or_under_way_stays_untreated() {
        let shaping = OutputShaping::default();
        // First seen with the setting off; it is turned on later.
        assert_eq!(shaping.decide(1, 7, 1, false, 30, TEXT), None);
        assert_eq!(shaping.decide(1, 7, 3, true, 30, TEXT), None);
        // First seen five messages in, with the setting on.
        assert_eq!(shaping.decide(2, 7, 5, true, 30, TEXT), None);
        assert_eq!(shaping.decide(2, 7, 7, true, 30, TEXT), None);
        assert_eq!(shaping.tracked(), 2);
    }

    #[test]
    fn a_holdout_carries_no_text() {
        let shaping = OutputShaping::default();
        let opening = (0..).find(|&o| arm_for(o, 30) == Arm::Holdout).unwrap();
        let decision = shaping.decide(9, opening, 1, true, 30, TEXT).unwrap();
        assert_eq!(decision.arm, Arm::Holdout);
        assert_eq!(decision.text, None);
    }

    #[test]
    fn the_block_goes_after_every_system_block_and_a_string_stays_a_string() {
        let mut blocks = json!({"system": [
            {"type": "text", "text": "You are an agent."},
            {"type": "text", "text": "Rules.", "cache_control": {"type": "ephemeral"}}
        ], "messages": []});
        assert!(append_instruction(&mut blocks, TEXT));
        assert_eq!(
            blocks["system"],
            json!([
                {"type": "text", "text": "You are an agent."},
                {"type": "text", "text": "Rules.", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": TEXT}
            ])
        );
        let mut string = json!({"system": "You are an agent.", "messages": []});
        assert!(append_instruction(&mut string, TEXT));
        assert_eq!(string["system"], json!("You are an agent.\n\nBe concise."));
        let mut none = json!({"messages": []});
        assert!(append_instruction(&mut none, TEXT));
        assert_eq!(none["system"], json!([{"type": "text", "text": TEXT}]));
        let mut odd = json!({"system": 3});
        assert!(!append_instruction(&mut odd, TEXT));
    }

    use kompressor_wire::ApiFamily;

    #[test]
    fn responses_instructions_are_extended_or_created() {
        let mut body = json!({"instructions": "You are Codex.", "input": []});
        assert!(append_instruction_for(
            &mut body,
            ApiFamily::OpenAiResponses,
            TEXT
        ));
        assert_eq!(body["instructions"], "You are Codex.\n\nBe concise.");
        let mut bare = json!({"input": []});
        assert!(append_instruction_for(
            &mut bare,
            ApiFamily::OpenAiResponses,
            TEXT
        ));
        assert_eq!(bare["instructions"], TEXT);
        let mut odd = json!({"instructions": [1]});
        assert!(!append_instruction_for(
            &mut odd,
            ApiFamily::OpenAiResponses,
            TEXT
        ));
    }

    #[test]
    fn chat_extends_a_leading_system_or_developer_message() {
        let mut string = json!({"messages": [
            {"role": "developer", "content": "Rules."},
            {"role": "user", "content": "hi"}
        ]});
        assert!(append_instruction_for(
            &mut string,
            ApiFamily::OpenAiChat,
            TEXT
        ));
        assert_eq!(string["messages"][0]["content"], "Rules.\n\nBe concise.");
        assert_eq!(string["messages"].as_array().unwrap().len(), 2);

        let mut parts = json!({"messages": [
            {"role": "system", "content": [{"type": "text", "text": "Rules."}]},
            {"role": "user", "content": "hi"}
        ]});
        assert!(append_instruction_for(
            &mut parts,
            ApiFamily::OpenAiChat,
            TEXT
        ));
        assert_eq!(
            parts["messages"][0]["content"],
            json!([{"type": "text", "text": "Rules."}, {"type": "text", "text": TEXT}])
        );
    }

    #[test]
    fn chat_without_a_leading_system_message_gets_one() {
        let mut body = json!({"messages": [{"role": "user", "content": "hi"}]});
        assert!(append_instruction_for(
            &mut body,
            ApiFamily::OpenAiChat,
            TEXT
        ));
        assert_eq!(
            body["messages"],
            json!([{"role": "system", "content": TEXT}, {"role": "user", "content": "hi"}])
        );
        let mut none = json!({"model": "x"});
        assert!(!append_instruction_for(
            &mut none,
            ApiFamily::OpenAiChat,
            TEXT
        ));
    }

    #[test]
    fn gemini_snake_case_instructions_are_extended_without_a_duplicate_field() {
        let mut body = json!({"system_instruction": {"parts": [{"text": "Rules."}]}});
        assert!(append_instruction_for(
            &mut body,
            ApiFamily::GeminiGenerateContent,
            TEXT
        ));
        assert!(body.get("systemInstruction").is_none());
        assert_eq!(
            body["system_instruction"]["parts"],
            json!([{"text": "Rules."}, {"text": TEXT}])
        );
    }

    #[test]
    fn gemini_and_converse_get_a_system_part_of_their_own_shape() {
        let mut gemini =
            json!({"systemInstruction": {"parts": [{"text": "Rules."}]}, "contents": []});
        assert!(append_instruction_for(
            &mut gemini,
            ApiFamily::GeminiGenerateContent,
            TEXT
        ));
        assert_eq!(
            gemini["systemInstruction"]["parts"],
            json!([{"text": "Rules."}, {"text": TEXT}])
        );
        let mut bare = json!({"contents": []});
        assert!(append_instruction_for(
            &mut bare,
            ApiFamily::GeminiGenerateContent,
            TEXT
        ));
        assert_eq!(
            bare["systemInstruction"],
            json!({"parts": [{"text": TEXT}]})
        );

        let mut converse =
            json!({"system": [{"text": "Rules."}, {"cachePoint": {"type": "default"}}]});
        assert!(append_instruction_for(
            &mut converse,
            ApiFamily::BedrockConverse,
            TEXT
        ));
        assert_eq!(
            converse["system"][2],
            json!({"text": TEXT}),
            "after the cache point"
        );
    }

    #[test]
    fn anthropic_is_unchanged_by_the_family_dispatch() {
        let mut body = json!({"system": "You are an agent.", "messages": []});
        assert!(append_instruction_for(
            &mut body,
            ApiFamily::AnthropicMessages,
            TEXT
        ));
        assert_eq!(body["system"], "You are an agent.\n\nBe concise.");
    }
}
