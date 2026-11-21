//! Reproducible experiment identity and measured proxy phases. Neither stores
//! prompts, credentials, response bodies, or provider authorization settings.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunContext {
    pub build_version: String,
    pub config_fingerprint: String,
    pub experiment_id: Option<String>,
    pub run_id: String,
    /// An explicit allowlist of compression/cache/retrieval policy settings.
    pub settings: serde_json::Value,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProxyTiming {
    pub first_head_ms: Option<f64>,
    pub attempts_total: u64,
    pub attempts: Vec<UpstreamAttempt>,
    pub retrieval_calls: u64,
    pub retrieval_execution_ms: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpstreamAttempt {
    pub duration_ms: f64,
    pub status: Option<u16>,
    pub failed: bool,
}
impl ProxyTiming {
    pub fn record_attempt(
        &mut self,
        duration_ms: f64,
        elapsed_ms: f64,
        status: Option<u16>,
        failed: bool,
    ) {
        self.attempts_total += 1;
        if self.first_head_ms.is_none() && status.is_some() {
            self.first_head_ms = Some(elapsed_ms);
        }
        // A looping or failing upstream must not make telemetry unbounded.
        if self.attempts.len() < 64 {
            self.attempts.push(UpstreamAttempt {
                duration_ms,
                status,
                failed,
            });
        }
    }
    pub fn record_retrieval(&mut self, duration_ms: f64) {
        self.retrieval_calls += 1;
        self.retrieval_execution_ms += duration_ms;
    }
}

/// What the proxy observed about one request's cacheable prefix. Sizes, hashes,
/// verdicts and counts only: no prompt text, no header values.
///
/// Filed as `proxy_requests.prefix_json` (schema v15). A row without it was
/// written before v15, or is one the proxy could not parse; either way it is
/// "unknown", never "nothing drifted".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrefixObservation {
    /// Whether there was an earlier request of this conversation to compare
    /// with. False on a conversation's first request and on a branch (a
    /// request carrying fewer messages than the one before it).
    pub compared: bool,
    /// Segments the client sent differently from its previous request: a
    /// bitmask of the `DRIFT_*` constants.
    pub drift_in: u8,
    /// Segments that went upstream differently from the previous request
    /// although the client had not changed them: the proxy's own doing.
    pub drift_out: u8,
    /// Segments in `drift_in` that went upstream unchanged all the same,
    /// because the proxy put them back: a pinned tool order, a beta token
    /// kept. The client changed them; the provider never saw it.
    #[serde(skip_serializing_if = "is_zero_u8")]
    pub repaired: u8,
    /// The first earlier message the client changed, when it changed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_in_at: Option<u32>,
    /// The first earlier message that went upstream changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_out_at: Option<u32>,
    /// Serialized size of the whole request the client sent.
    pub body_bytes: u32,
    pub system_bytes: u32,
    /// Tool definitions the model is shown, deferred ones left out.
    pub tools_bytes: u32,
    pub tools_count: u32,
    /// Tools marked `defer_loading`, which the provider does not render.
    pub tools_deferred: u32,
    /// Instructions and rendered tool definitions as they went upstream,
    /// which is what the provider caches in front of the conversation: the
    /// client's own plus whatever the proxy added (its retrieve tool) or took
    /// out. Zero on a row written before this was recorded, where
    /// `system_bytes + tools_bytes` stands in for it.
    #[serde(skip_serializing_if = "is_zero")]
    pub opening_bytes: u32,
    /// Whether the rendered list carries the provider's own tool-search tool.
    /// A client that sends one loads tools on demand, whether or not any tool
    /// in this list is itself marked `defer_loading`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub tool_search: bool,
    pub messages_bytes: u32,
    pub messages_count: u32,
    /// Text in the conversation the engine can see. The rest of
    /// `messages_bytes` is opaque to it: tool-call arguments, images,
    /// structured results, signatures.
    pub text_bytes: u32,
    /// Plain-text reasoning the client re-sent.
    pub reasoning_bytes: u32,
    /// Volatile-looking values in the system prompt and tool descriptions,
    /// counted when those segments were new or had changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volatile: Option<Volatile>,
    /// Idle time since the conversation's last response or keep-warm read,
    /// measured as this request arrived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_ms: Option<u64>,
    /// Whether the provider had ever served this conversation from cache.
    pub was_warm: bool,
    /// Whether this request carried more messages than any before it under
    /// its conversation, as it arrived: the half of the runtime's test for a
    /// predicted lapse that idle time does not show. One that does not is
    /// another conversation opening with the same bytes, which finds no
    /// lapse of this one's. `None` on a row written before this was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continues: Option<bool>,
    /// The cache lifetime the previous request asked the provider for, in
    /// seconds, where the request says: Anthropic's five minutes or an hour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifetime_s: Option<u32>,
    /// Set when this turn found the provider's cache gone and the whole
    /// conversation was recompacted (`proxy.cold_recompact`): `observed` when
    /// the provider's last answer said so, `predicted` when the idle time did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recompacted: Option<String>,
    /// Stale reads the conversation's ledger held after that recompaction.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ledger_reads: u32,
    /// Bytes `proxy.tools_compact` took out of this request's tool
    /// definitions. Zero when the conversation is not compacted.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tools_compacted_bytes: u32,
    /// Bytes of earlier-turn reasoning `proxy.reasoning_elide_models` took
    /// out of this request.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reasoning_elided_bytes: u32,
    /// Whether the client named a `prompt_cache_key` itself. `None` on a
    /// family that has no such field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_cache_key: Option<bool>,
    /// What `proxy.tools_compact` would take out of this request's tool
    /// definitions, measured without doing it. Zero where it is doing it.
    #[serde(skip_serializing_if = "is_zero")]
    pub tools_compactable_bytes: u32,
    /// Bytes of file reads a recompaction would rewrite at this point in the
    /// conversation, by what it would decide about them: invalidated by a
    /// later write, contained in a later read, or left alone long enough to
    /// compress as source. Measured on every request, acted on by none.
    #[serde(skip_serializing_if = "is_zero")]
    pub stale_read_bytes: u32,
    #[serde(skip_serializing_if = "is_zero")]
    pub superseded_read_bytes: u32,
    #[serde(skip_serializing_if = "is_zero")]
    pub quiet_read_bytes: u32,
}

fn is_false(flag: &bool) -> bool {
    !*flag
}

fn is_zero(count: &u32) -> bool {
    *count == 0
}

fn is_zero_u8(mask: &u8) -> bool {
    *mask == 0
}

/// What a turn scored for a predicted lapse turned out to be, from what the
/// provider reported reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LapseOutcome {
    /// The prefix the request before it left was read back.
    ReadBack,
    /// The conversation's own messages were not read at all: what an expired
    /// entry looks like, and the only outcome that confirms a prediction.
    Full,
    /// Part of the conversation was read and the rest was not. An expiry
    /// takes all of it, so something past the first messages changed.
    Partial,
    /// A hashed segment of the prefix changed on this request, which explains
    /// the loss whatever the idle time was.
    Drift,
    /// Lost, with no earlier request to compare this one with.
    Unknown,
}

impl PrefixObservation {
    /// Whether a turn recompacted on a *predicted* lapse was served from
    /// cache after all: the one classifier for a wrong prediction, used by
    /// the proxy's breaker as the answer arrives and by the report afterwards.
    ///
    /// Having rewritten the conversation, the provider can read back only
    /// what precedes the first rewritten message, so a read that reaches past
    /// the instructions and tools is the one sign left. A turn with retrieve
    /// rounds is left out: its usage sums a later round's read of what an
    /// earlier one wrote.
    pub fn prediction_was_wrong(&self, read: Option<u32>, rounds: u32) -> bool {
        rounds == 0 && read.is_some_and(|read| self.read_reaches_messages(read))
    }

    /// Whether this request met the runtime's conditions for predicting a
    /// lapse, idle time aside: a conversation the provider had served from
    /// cache, continued by this request. On a row from before `continues`
    /// was recorded, a request that was compared — one carrying no fewer
    /// messages than the one before it — stands in for it.
    pub fn could_predict(&self) -> bool {
        self.was_warm && self.idle_ms.is_some() && self.continues.unwrap_or(self.compared)
    }

    /// What became of the prefix on a turn the cache walk says did
    /// (`lost`) or did not lose it, given what the provider read.
    pub fn lapse_outcome(&self, lost: bool, read: u32) -> LapseOutcome {
        if !lost {
            LapseOutcome::ReadBack
        } else if !self.compared {
            LapseOutcome::Unknown
        } else if (self.drift_in & !self.repaired) | self.drift_out != 0 {
            LapseOutcome::Drift
        } else if self.read_reaches_messages(read) {
            LapseOutcome::Partial
        } else {
            LapseOutcome::Full
        }
    }

    /// Whether a cache read of `read` tokens reached into the conversation,
    /// past the instructions and tool definitions in front of it.
    ///
    /// On Anthropic that opening is often shared: other conversations keep it
    /// cached, so a conversation whose own entries have lapsed still reads it.
    /// A read is evidence that *this conversation's* cache was alive only if
    /// it goes further.
    ///
    /// The opening is sized from what went upstream (`opening_bytes`), at two
    /// tokens per five bytes of its JSON, plus the smallest prefix a provider
    /// caches. Claude Code's opening was measured at a token per 2.8 bytes;
    /// the first version of this sized the client's bytes at a token per
    /// three, came out below the real opening, and so called three correct
    /// predictions wrong and ended prediction for the run. Erring high costs
    /// only sight of a read that stops within the first few messages.
    pub fn read_reaches_messages(&self, read: u32) -> bool {
        u64::from(read) > self.opening_tokens() + 1_024
    }

    /// An upper estimate of the tokens the provider caches in front of the
    /// conversation.
    fn opening_tokens(&self) -> u64 {
        let bytes = match self.opening_bytes {
            0 => u64::from(self.system_bytes) + u64::from(self.tools_bytes),
            sent => u64::from(sent),
        };
        bytes * 2 / 5
    }
}

/// The client changed its system prompt.
pub const DRIFT_SYSTEM: u8 = 1;
/// A tool's definition changed while the list of names did not.
pub const DRIFT_TOOL_DEFS: u8 = 1 << 1;
/// The same tools arrived in another order.
pub const DRIFT_TOOL_ORDER: u8 = 1 << 2;
/// Tools were added or removed.
pub const DRIFT_TOOL_SET: u8 = 1 << 3;
/// The `anthropic-beta` / `openai-beta` tokens changed.
pub const DRIFT_BETA: u8 = 1 << 4;
/// A message already sent on the previous request came back different.
pub const DRIFT_HISTORY: u8 = 1 << 5;

/// Every drift bit with the name reports use for it.
pub const DRIFT_NAMES: [(u8, &str); 6] = [
    (DRIFT_SYSTEM, "system"),
    (DRIFT_TOOL_DEFS, "tool_defs"),
    (DRIFT_TOOL_ORDER, "tool_order"),
    (DRIFT_TOOL_SET, "tool_set"),
    (DRIFT_BETA, "beta"),
    (DRIFT_HISTORY, "history"),
];

/// The names of the bits set in `mask`, in [`DRIFT_NAMES`] order.
pub fn drift_names(mask: u8) -> Vec<&'static str> {
    DRIFT_NAMES
        .iter()
        .filter(|(bit, _)| mask & bit != 0)
        .map(|(_, name)| *name)
        .collect()
}

/// Counts of values that look as if they change from one request to the next.
/// Each count stops at 255.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volatile {
    /// `2026-11-20T10:15:00`-shaped date-times.
    pub timestamps: u8,
    /// `2026-11-20`-shaped dates with no time after them.
    pub dates: u8,
    pub uuids: u8,
    /// Three dot-separated base64url runs opening with `eyJ`.
    pub jwts: u8,
}

impl Volatile {
    pub fn any(self) -> bool {
        self.timestamps > 0 || self.dates > 0 || self.uuids > 0 || self.jwts > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn first_head_excludes_failed_connects_and_attempt_details_are_bounded() {
        let mut t = ProxyTiming::default();
        t.record_attempt(10.0, 10.0, None, true);
        assert_eq!(t.first_head_ms, None);
        t.record_attempt(20.0, 35.0, Some(429), false);
        t.record_attempt(5.0, 42.0, Some(200), false);
        assert_eq!(t.first_head_ms, Some(35.0));
        for _ in 0..100 {
            t.record_attempt(1.0, 100.0, Some(200), false);
        }
        assert_eq!(t.attempts_total, 103);
        assert_eq!(t.attempts.len(), 64);
        t.record_retrieval(2.0);
        t.record_retrieval(3.0);
        assert_eq!(t.retrieval_execution_ms, 5.0);
        assert_eq!(t.retrieval_calls, 2);
    }
}
