//! Answering a `optscale_kompressor_retrieve` call, and the budget that bounds doing so.
//!
//! Every answer here is content the model pays for again on the next round, and
//! the round after that re-sends it along with everything else. A loop with no
//! ceiling can re-inject far more than the compression ever removed — measured
//! at roughly 67K tokens re-injected against 7K saved, on one request — so the
//! ceiling is not a safety valve, it is part of the feature.
//!
//! The other rule is that a miss is a miss. A handle that does not resolve is
//! answered with an instruction to regenerate the content, never with something
//! plausible: a model that fills the gap from memory produces an answer that
//! looks identical to a correct one and is not.

use std::sync::Arc;

use serde_json::{Value, json};

use kompressor_core::auth::Identity;
use kompressor_core::retrieve::{RangeParse, RetrieveRange};
use kompressor_core::{AppCore, RequestInfo};
use kompressor_engine::estimate_tokens;
use kompressor_engine::refs::RefId;
use kompressor_stats::events::Surface;

use crate::tool::arguments_object;
use crate::turn::ModelToolCall;

/// What the loop has spent so far, and what it may spend.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    /// Rounds already run.
    pub rounds: u32,
    /// Tokens already handed back to the model.
    pub tokens: u32,
    max_rounds: u32,
    max_tokens: u32,
}

impl Budget {
    pub fn new(max_rounds: u32, max_tokens: u32) -> Budget {
        Budget {
            rounds: 0,
            tokens: 0,
            max_rounds,
            max_tokens,
        }
    }

    /// Whether the next call must be refused.
    pub fn is_spent(&self) -> bool {
        self.rounds >= self.max_rounds || self.tokens >= self.max_tokens
    }

    /// Tokens still available to answer with.
    ///
    /// The loop charges a whole round after building it, so the ceiling used to
    /// bound only the *next* round: one call could return a fragment many times
    /// the configured allowance and the request went upstream carrying it.
    /// Measured with `retrieve_max_tokens_per_request = 1`, a single round
    /// injected 12,999 tokens and succeeded. Answers are now reserved against
    /// this before they are added to the next request.
    pub fn remaining(&self) -> u32 {
        self.max_tokens.saturating_sub(self.tokens)
    }

    /// Record a round's cost.
    pub fn spend(&mut self, tokens: u32) {
        self.rounds += 1;
        self.tokens = self.tokens.saturating_add(tokens);
    }
}

/// Runs retrieve calls for one request.
pub struct Retriever {
    core: Arc<AppCore>,
    identity: Identity,
    request_id: String,
    api_family: String,
    model: Option<String>,
    /// Where a hit is filed, so the engine stops eliding what was fetched.
    prefixes: Arc<crate::prefixcache::PrefixTracker>,
    /// The conversation this request belongs to. `None` when the request
    /// carries no identity, in which case a hit is served and not remembered:
    /// there is nothing to remember it against.
    conversation: Option<u64>,
}

impl Retriever {
    pub fn new(
        core: Arc<AppCore>,
        identity: Identity,
        request_id: String,
        api_family: String,
        model: Option<String>,
        prefixes: Arc<crate::prefixcache::PrefixTracker>,
        conversation: Option<u64>,
    ) -> Retriever {
        Retriever {
            core,
            identity,
            request_id,
            api_family,
            model,
            prefixes,
            conversation,
        }
    }

    /// Answer one call, returning the tool-result payload and what it costs the
    /// next round in tokens.
    pub fn answer(&self, call: &ModelToolCall) -> (String, u32) {
        let payload = self.result_for(call);
        let text = payload.to_string();
        let tokens = estimate_tokens(&text, "tool");
        (text, tokens)
    }

    /// Answer a call the budget will not pay for.
    ///
    /// Still a tool result rather than a dropped call: the provider rejects the
    /// next request if a call goes unanswered, and the model needs to be told
    /// to finish with what it has rather than to keep asking.
    ///
    /// Recorded as well as answered. Both HTTP paths funnel their refusals
    /// through here — `spent` before the round and over-budget within it — so
    /// this is the one place that can file them, and until it did the budget
    /// was the only part of the retrieve loop with no trace in the database.
    /// The handle is taken from the call rather than from the caller: a refusal
    /// is worth nothing to an operator without the handle it was asked for, and
    /// `"ref"` is the only place it exists. A call whose arguments do not parse
    /// is filed under the empty handle, which is honest — the model asked for
    /// something unreadable and was refused for an unrelated reason, and
    /// dropping the row would lose the refusal to keep a field tidy.
    pub fn refuse(&self, call: &ModelToolCall) -> (String, u32) {
        let handle = arguments_object(&call.arguments)
            .and_then(|arguments| {
                arguments
                    .get("ref")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default();
        self.core.record_refusal(
            &self.identity,
            &handle,
            &RequestInfo {
                request_id: &self.request_id,
                api_family: Some(&self.api_family),
                model: self.model.as_deref(),
                provider: None,
            },
            Surface::Proxy,
        );
        let payload = json!({
            "found": false,
            "error": "retrieval budget exhausted",
            "hint": "no further fragments can be fetched for this request; answer from what the \
                    conversation already contains, and say what you could not see rather than \
                    reconstructing it",
        });
        let text = payload.to_string();
        let tokens = estimate_tokens(&text, "tool");
        (text, tokens)
    }

    fn result_for(&self, call: &ModelToolCall) -> Value {
        let Some(arguments) = arguments_object(&call.arguments) else {
            return json!({
                "found": false,
                "error": "arguments were not valid JSON",
                "hint": "call optscale_kompressor_retrieve with {\"ref\": \"k_…\"} taken verbatim from an \
                        omission marker",
            });
        };
        let Some(handle) = arguments.get("ref").and_then(Value::as_str) else {
            return json!({
                "found": false,
                "error": "no ref argument",
                "hint": "the handle is the k_… inside the omission marker",
            });
        };
        // A range we cannot read is treated as no range at all: the cap on a
        // whole retrieval still applies, so the failure mode is a larger answer
        // rather than a wrong one. But "no range" is the expensive answer — the
        // whole fragment is re-injected and paid for — so a request that is
        // merely over-specified is salvaged rather than dropped, and whatever
        // happens is said out loud. This used to be a `debug!`, and two paid
        // runs narrowed nothing at all without anyone noticing.
        let range = arguments
            .get("range")
            .filter(|range| !range.is_null())
            .and_then(|range| {
                let (parsed, how) = RetrieveRange::parse_loose(range);
                match how {
                    RangeParse::Exact => {}
                    RangeParse::Salvaged { offered } => tracing::warn!(
                        offered,
                        chose = ?parsed,
                        "the model asked for several ranges at once; kept the only substantive one"
                    ),
                    RangeParse::Unusable => tracing::warn!(
                        range = %range,
                        "ignoring a range that names no usable window; sending the fragment whole"
                    ),
                }
                parsed
            });

        let got = self.core.retrieve(
            &self.identity,
            handle,
            range.as_ref(),
            &RequestInfo {
                request_id: &self.request_id,
                api_family: Some(&self.api_family),
                model: self.model.as_deref(),
                provider: None,
            },
            Surface::Proxy,
        );

        if !got.found {
            return json!({
                "found": false,
                "ref": handle,
                "expired": got.expired,
                "hint": got.miss_hint(),
            });
        }
        // Filed against the conversation. A *second* fetch from a different
        // request is what makes the engine keep this content inline: the round
        // that answers a retrieval never reaches the client's history, so a
        // handle wanted across turns is asked for again and again, while one
        // fetched once is simply a fragment the model happened to need.
        if let (Some(conversation), Some(id)) = (self.conversation, RefId::parse(handle)) {
            self.prefixes
                .record_retrieved(conversation, &self.request_id, &[id]);
        }
        let mut payload = json!({
            "found": true,
            "ref": handle,
            "content": got.content.unwrap_or_default(),
            "ranged": got.ranged,
            "truncated": got.truncated,
            "total_lines": got.total_lines,
            "total_chars": got.total_chars,
        });
        // A range that matched nothing is a hit with empty content, which on
        // its own tells the model only that it wasted a round. Saying how the
        // range missed is what lets it spend the next one better.
        if let Some(hint) = got.range_hint {
            payload["hint"] = Value::String(hint);
        }
        payload
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use kompressor_core::Config;
    use kompressor_store::memory::MemoryRefStore;

    pub(crate) fn retriever() -> Retriever {
        let core = AppCore::new(
            Config::default(),
            Arc::new(MemoryRefStore::new()),
            kompressor_stats::writer::null_sink(),
        );
        Retriever::new(
            Arc::new(core),
            Identity::anonymous(),
            "req-1".into(),
            "openai_chat".into(),
            None,
            std::sync::Arc::new(crate::prefixcache::PrefixTracker::new(
                std::time::Duration::ZERO,
            )),
            Some(1),
        )
    }

    /// A retriever whose events can be read back.
    fn recording_retriever() -> (Retriever, kompressor_stats::writer::CapturedEvents) {
        let (sink, events) = kompressor_stats::writer::capturing_sink(64);
        let core = AppCore::new(Config::default(), Arc::new(MemoryRefStore::new()), sink);
        (
            Retriever::new(
                Arc::new(core),
                Identity::anonymous(),
                "req-1".into(),
                "openai_chat".into(),
                None,
                std::sync::Arc::new(crate::prefixcache::PrefixTracker::new(
                    std::time::Duration::ZERO,
                )),
                Some(1),
            ),
            events,
        )
    }

    fn call(arguments: Value) -> ModelToolCall {
        ModelToolCall {
            id: "call_1".into(),
            name: crate::tool::RETRIEVE_TOOL_NAME.into(),
            arguments,
        }
    }

    #[test]
    fn a_handle_that_does_not_resolve_tells_the_model_to_regenerate_rather_than_invent() {
        let (payload, _) = retriever().answer(&call(json!({"ref": "k_AbC123dEf456"})));
        let parsed: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["found"], false);
        assert!(parsed["hint"].as_str().unwrap().contains("Re-run"));
        assert!(parsed.get("content").is_none(), "nothing is invented");
    }

    #[test]
    fn malformed_arguments_are_answered_rather_than_dropped() {
        // an unanswered call makes the provider reject the next request
        let (payload, _) = retriever().answer(&call(json!("{\"ref\":")));
        let parsed: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["found"], false);
        assert!(parsed["error"].as_str().unwrap().contains("valid JSON"));

        let (payload, _) = retriever().answer(&call(json!({"reference": "k_x"})));
        let parsed: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["error"], "no ref argument");
    }

    /// The payload the model actually reads. A hit whose range selected
    /// nothing used to arrive as `found: true` with empty content and no other
    /// signal — indistinguishable, from the model's side, from a fragment that
    /// genuinely holds nothing. It answered by spending another round.
    #[test]
    fn an_empty_ranged_hit_tells_the_model_how_the_range_missed() {
        use kompressor_store::types::{PutMeta, RefStore, Tenant};

        let store = MemoryRefStore::new();
        let fragment = "alpha one\nbravo two\ncharlie three";
        let id = kompressor_engine::refs::ref_for(fragment);
        store
            .put_many(
                &Tenant::new(""),
                &[(id, fragment.to_string())],
                &PutMeta::new(kompressor_engine::types::Kind::Text),
            )
            .unwrap();

        let core = AppCore::new(
            Config::default(),
            Arc::new(store),
            kompressor_stats::writer::null_sink(),
        );
        let retriever = Retriever::new(
            Arc::new(core),
            Identity::anonymous(),
            "req-1".into(),
            "openai_chat".into(),
            None,
            std::sync::Arc::new(crate::prefixcache::PrefixTracker::new(
                std::time::Duration::ZERO,
            )),
            Some(1),
        );

        let (payload, _) = retriever.answer(&call(json!({
            "ref": id.to_string(),
            "range": {"grep": {"pattern": "seq=1978"}},
        })));
        let parsed: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["found"], true, "the handle resolved");
        assert_eq!(parsed["content"], "", "and the range selected nothing");
        let hint = parsed["hint"]
            .as_str()
            .expect("an empty range explains itself");
        assert!(hint.contains("literally"), "{hint}");

        // a range that does select something carries no hint at all
        let (ok, _) = retriever.answer(&call(json!({
            "ref": id.to_string(),
            "range": {"grep": {"pattern": "bravo"}},
        })));
        let parsed: Value = serde_json::from_str(&ok).unwrap();
        assert!(parsed["content"].as_str().unwrap().contains("bravo"));
        assert!(parsed.get("hint").is_none(), "nothing went wrong");
    }

    #[test]
    fn a_spent_budget_is_refused_in_words_the_model_can_act_on() {
        let (payload, tokens) = retriever().refuse(&call(json!({"ref": "k_AbC123dEf456"})));
        let parsed: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["error"], "retrieval budget exhausted");
        assert!(tokens > 0);
    }

    /// Both HTTP paths refuse through `refuse`, so this is the row they file.
    /// Without it the budget is the only part of the retrieve loop that leaves
    /// no trace, and the hit rate is computed entirely over the asks that got
    /// past it.
    #[test]
    fn a_refusal_is_recorded_against_the_handle_that_was_asked_for() {
        let (retriever, events) = recording_retriever();
        retriever.refuse(&call(json!({"ref": "k_AbC123dEf456"})));

        let filed: Vec<_> = events
            .drain()
            .into_iter()
            .filter_map(|event| match event {
                kompressor_stats::events::Event::Retrieval(e) => Some(e),
                _ => None,
            })
            .collect();
        assert_eq!(filed.len(), 1, "one refusal, one row");
        assert!(filed[0].withheld, "filed, but not as a withholding");
        assert!(
            !filed[0].found,
            "a refusal must not read as the store hitting"
        );
        assert!(
            !filed[0].expired,
            "nor as an expiry: nothing was looked up to expire"
        );
        assert_eq!(filed[0].reference, "k_AbC123dEf456");
        assert_eq!(filed[0].returned_tokens, 0);
        assert_eq!(filed[0].request_id.as_deref(), Some("req-1"));
    }

    /// A refused call with unreadable arguments is still a refusal. Dropping
    /// the row to avoid an empty handle would lose the one event the budget
    /// produces, to keep a field tidy.
    #[test]
    fn a_refusal_with_no_readable_handle_is_still_filed() {
        let (retriever, events) = recording_retriever();
        retriever.refuse(&call(json!("{\"ref\":")));
        let filed: Vec<_> = events
            .drain()
            .into_iter()
            .filter_map(|event| match event {
                kompressor_stats::events::Event::Retrieval(e) => Some(e),
                _ => None,
            })
            .collect();
        assert_eq!(filed.len(), 1);
        assert!(filed[0].withheld);
        assert_eq!(filed[0].reference, "");
    }

    #[test]
    fn the_budget_is_spent_by_either_rounds_or_tokens() {
        let mut rounds = Budget::new(2, 10_000);
        assert!(!rounds.is_spent());
        rounds.spend(10);
        rounds.spend(10);
        assert!(rounds.is_spent(), "the round cap trips on its own");

        let mut tokens = Budget::new(10, 100);
        tokens.spend(150);
        assert!(tokens.is_spent(), "so does the token cap");
    }
}
