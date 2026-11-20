//! What the proxy observed about each request's cacheable prefix, added up.
//!
//! The cache report says a turn lost its prefix and puts the loss down to
//! expiry or to a `prefix_change`. This one says what that change was. Four
//! questions, each answered from `proxy_requests.prefix_json`
//! ([`PrefixObservation`], schema v15) and nothing else:
//!
//! - **Which segment moved, and on which side of the proxy.** Every loss the
//!   cache walk finds is joined to the observation of the request it happened
//!   on and put down to one segment and one side.
//! - **What a request is made of.** Bytes of instructions, tool definitions,
//!   conversation, engine-visible text and re-sent reasoning, per model.
//! - **Whether the instructions carry values that change by themselves.**
//! - **How well a lapse can be predicted on the request that finds it**, by
//!   comparing the idle time recorded as each request arrived with what the
//!   provider then reported reading.
//!
//! # Reading it honestly
//!
//! A row with no observation is *unknown*: it was written before v15, or it is
//! a keep-warm ping, a relayed frame or a body the proxy could not parse. Such
//! rows are counted apart and never read as "nothing drifted".
//!
//! Sizes are bytes of request JSON, not tokens, and shares are shares of
//! bytes. They rank segments; they do not price them.
//!
//! A loss is put down to the *earliest* segment that changed, in the order a
//! provider renders a prompt, because everything after the first changed byte
//! is lost whatever else changed with it. When several changed at once the
//! later ones are not counted against that loss.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::StatsError;
use crate::cache::{Cause, MAX_ROWS};
use crate::context::{
    DRIFT_BETA, DRIFT_HISTORY, DRIFT_SYSTEM, DRIFT_TOOL_DEFS, DRIFT_TOOL_ORDER, DRIFT_TOOL_SET,
    LapseOutcome, PrefixObservation,
};
use crate::reader::{StatsReader, ratio};

/// Margins a predicted lapse is scored at, in seconds past the lifetime.
pub const MARGINS_S: [u32; 4] = [0, 30, 60, 120];
/// Lifetimes scored where the request does not name one. OpenAI documents an
/// in-memory entry as lasting five to ten minutes idle and up to an hour.
pub const ASSUMED_LIFETIMES_S: [u32; 3] = [300, 600, 3_600];
/// From this many rendered tools up, a request that defers none is counted as
/// carrying its tool list whole.
pub const MANY_TOOLS: u32 = 12;

/// The order a provider renders the segments in, earliest first.
const RENDER_ORDER: [(u8, &str); 6] = [
    (DRIFT_BETA, "beta"),
    (DRIFT_TOOL_SET, "tool_set"),
    (DRIFT_TOOL_ORDER, "tool_order"),
    (DRIFT_TOOL_DEFS, "tool_defs"),
    (DRIFT_SYSTEM, "system"),
    (DRIFT_HISTORY, "history"),
];

/// What the requests of one model were made of.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Composition {
    pub api_family: String,
    pub model: String,
    /// Observed client requests.
    pub requests: u64,
    pub body_bytes: u64,
    pub system_bytes: u64,
    pub tools_bytes: u64,
    pub messages_bytes: u64,
    /// Conversation text the engine can see; the rest of `messages_bytes` is
    /// opaque to it.
    pub text_bytes: u64,
    pub reasoning_bytes: u64,
    pub tools_share: f64,
    pub system_share: f64,
    pub reasoning_share: f64,
    /// The part of the conversation the engine cannot see.
    pub opaque_share: f64,
    /// Rendered tools per request, on average.
    pub tools_per_request: f64,
    /// Requests that load tools on demand: at least one tool marked
    /// `defer_loading`, or the provider's own tool-search tool in the list.
    pub deferring_requests: u64,
    /// Of those, the ones carrying the tool-search tool.
    #[serde(default)]
    pub tool_search_requests: u64,
    /// Requests rendering [`MANY_TOOLS`] or more that defer none and carry no
    /// tool-search tool.
    pub undeferred_requests: u64,
    /// Tool-definition bytes those requests carried.
    pub undeferred_tools_bytes: u64,
    /// Bytes `proxy.tools_compact` took out of tool definitions, and the
    /// requests it took them from.
    pub tools_compacted_bytes: u64,
    pub tools_compacted_requests: u64,
    /// Bytes of earlier-turn reasoning `proxy.reasoning_elide_models` took
    /// out, summed over the requests that carried it.
    pub reasoning_elided_bytes: u64,
}

/// One segment's drift, and the losses put down to it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SegmentDrift {
    pub segment: String,
    /// Compared requests on which the client changed it.
    pub client_turns: u64,
    /// Of `client_turns`, those the proxy put back before they went upstream.
    pub repaired_turns: u64,
    /// Compared requests on which only the proxy changed it.
    pub proxy_turns: u64,
    pub client_losses: u64,
    pub client_lost_tokens: i64,
    pub client_cost: f64,
    pub proxy_losses: u64,
    pub proxy_lost_tokens: i64,
    pub proxy_cost: f64,
}

/// Losses the observations cannot put down to a segment, by why not.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LossBucket {
    pub events: u64,
    pub lost_tokens: i64,
    /// Input-equivalent tokens, as the cache report prices a loss.
    pub cost: f64,
}

impl LossBucket {
    fn add(&mut self, tokens: i64, cost: f64) {
        self.events += 1;
        self.lost_tokens += tokens;
        self.cost += cost;
    }
}

/// Every `prefix_change` loss in the window, by what explains it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LossAttribution {
    pub total: LossBucket,
    /// A segment the client changed.
    pub client: LossBucket,
    /// A segment only the proxy changed.
    pub proxy: LossBucket,
    /// Compared with the request before it, and no hashed segment differed:
    /// the provider dropped the entry, or something outside the hashes moved.
    pub none_seen: LossBucket,
    /// The request had nothing to be compared with — the first this process
    /// saw of the conversation, or a branch.
    pub not_compared: LossBucket,
    /// The row carries no observation: a keep-warm ping, a relayed frame, or a
    /// row from before schema v15.
    pub unobserved: LossBucket,
}

/// Instructions and tool descriptions carrying volatile-looking values.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VolatileSummary {
    /// Requests that reported any.
    pub requests: u64,
    /// Conversations that reported any.
    pub conversations: u64,
    pub with_timestamps: u64,
    pub with_dates: u64,
    pub with_uuids: u64,
    pub with_jwts: u64,
}

/// How a predicted lapse scored against what the provider reported.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ColdScore {
    pub api_family: String,
    /// The lifetime predicted against, in seconds.
    pub lifetime_s: u32,
    /// Whether the previous request asked for that lifetime (`true`) or it is
    /// one of [`ASSUMED_LIFETIMES_S`].
    pub asked: bool,
    pub margin_s: u32,
    /// Requests the runtime could have predicted on, idle time aside — a
    /// warm conversation, continued ([`PrefixObservation::could_predict`]) —
    /// that reported usage and were not themselves recompacted.
    pub scored: u64,
    /// Predicted cold: idle longer than the lifetime plus the margin.
    pub predicted: u64,
    /// Predicted cold, and the conversation's own messages were not read
    /// back at all with nothing seen to change: an expiry.
    pub confirmed: u64,
    /// Predicted cold, and the prefix was read back after all.
    pub wrong: u64,
    /// Predicted cold, and part of the conversation was read back. Not an
    /// expiry, which takes all of it.
    #[serde(default)]
    pub partial: u64,
    /// Predicted cold, and lost on a request whose prefix had changed: the
    /// change explains the loss, the idle time need not.
    #[serde(default)]
    pub drift: u64,
    /// Predicted cold, and lost with nothing to compare the request with.
    #[serde(default)]
    pub unknown: u64,
    /// Not predicted, and the conversation's messages were lost whole with
    /// nothing seen to change.
    pub missed: u64,
    /// `confirmed` over `predicted`: a partial, drift-associated or unknown
    /// loss counts against a prediction as a read-back does.
    pub precision: f64,
    pub recall: f64,
}

/// Turns that found the provider's cache gone and were recompacted
/// (`proxy.cold_recompact`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Recompactions {
    /// On the provider's own answer: it had read nothing and written nothing.
    pub observed: u64,
    /// On idle time past the cache lifetime.
    pub predicted: u64,
    /// Predicted, and the provider then read from cache: a live prefix
    /// rewritten.
    pub predicted_wrong: u64,
    /// Conversations recompacted at least once.
    pub conversations: u64,
    /// Stale reads replaced, summed over each conversation's ledger.
    pub stale_reads: u64,
}

/// Everything the prefix view shows, for one tenant and one window.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PrefixReport {
    pub tenant: String,
    pub from_ms: i64,
    pub to_ms: i64,
    /// False when the database predates schema v15 and holds no observations.
    pub available: bool,
    /// Client requests in the window, pings left out.
    pub requests: u64,
    /// Of those, the ones carrying an observation.
    pub observed: u64,
    /// Of those, the ones compared with an earlier request.
    pub compared: u64,
    /// Compared requests on which nothing reached the provider changed.
    pub stable: u64,
    /// More rows matched than were read; only the newest were.
    pub truncated: bool,
    pub composition: Vec<Composition>,
    pub drift: Vec<SegmentDrift>,
    pub losses: LossAttribution,
    pub volatile: VolatileSummary,
    pub cold: Vec<ColdScore>,
    pub recompactions: Recompactions,
}

/// The earliest changed segment and the side that changed it.
///
/// History is the proxy's only where it moved earlier on the way out than on
/// the way in, which is what `drift_out` already says; for every other segment
/// the two masks cannot both be set.
fn blame(observation: &PrefixObservation) -> Option<(&'static str, bool)> {
    // A change the proxy put back never reached the provider, and so cannot
    // be what it failed to read.
    let by_client = observation.drift_in & !observation.repaired;
    RENDER_ORDER.iter().find_map(|(bit, name)| {
        if observation.drift_out & bit != 0 {
            Some((*name, true))
        } else if by_client & bit != 0 {
            Some((*name, false))
        } else {
            None
        }
    })
}

pub(crate) struct Observed {
    pub(crate) request_id: String,
    pub(crate) api_family: String,
    pub(crate) model: String,
    pub(crate) conversation: Option<String>,
    /// The provider's cache read, when it reported usage at all.
    pub(crate) read: Option<i64>,
    /// The prompt tokens the provider reported, as it reports them.
    pub(crate) prompt: i64,
    /// Retrieve rounds the request spent.
    pub(crate) rounds: i64,
    pub(crate) observation: PrefixObservation,
}

fn score(api_family: &str, lifetime_s: u32, asked: bool, margin_s: u32) -> ColdScore {
    ColdScore {
        api_family: api_family.to_string(),
        lifetime_s,
        asked,
        margin_s,
        ..ColdScore::default()
    }
}

impl StatsReader {
    /// The prefix report for `tenant` over `[from_ms, to_ms)`.
    pub fn prefix(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<PrefixReport, StatsError> {
        let _snapshot = self.conn.unchecked_transaction()?;
        let mut report = PrefixReport {
            tenant: tenant.to_string(),
            from_ms,
            to_ms,
            ..PrefixReport::default()
        };
        // The reader never migrates, so a database no current writer has
        // opened has no such column. That is "nothing recorded", not an error.
        report.available = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('proxy_requests')
                 WHERE name = 'prefix_json'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        report.drift = RENDER_ORDER
            .iter()
            .map(|(_, name)| SegmentDrift {
                segment: name.to_string(),
                ..SegmentDrift::default()
            })
            .collect();
        if !report.available {
            return Ok(report);
        }

        report.requests = self.conn.query_row(
            "SELECT COUNT(*) FROM proxy_requests
             WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3 AND keep_warm = 0",
            (tenant, from_ms, to_ms),
            |row| row.get::<_, i64>(0),
        )? as u64;

        let (observed, truncated) = self.prefix_rows(tenant, from_ms, to_ms)?;
        report.truncated = truncated;
        report.observed = observed.len() as u64;

        // What the cache walk makes of each request, taken first: whether a
        // request lost its prefix is the truth a predicted lapse is scored
        // against.
        let (losses, losses_truncated) = self.cache_losses(tenant, from_ms, to_ms)?;
        report.truncated |= losses_truncated;
        let lost: HashSet<&str> = losses
            .iter()
            .filter(|loss| !loss.keep_warm)
            .map(|loss| loss.request_id.as_str())
            .collect();

        let mut composition: BTreeMap<(String, String), Composition> = BTreeMap::new();
        let mut tools_seen: HashMap<(String, String), u64> = HashMap::new();
        let mut volatile_conversations: HashSet<&str> = HashSet::new();
        let mut scores: BTreeMap<(String, u32, bool, u32), ColdScore> = BTreeMap::new();
        let mut by_request: HashMap<&str, &PrefixObservation> = HashMap::new();
        // The largest ledger each conversation reported: a ledger only grows.
        let mut ledgers: HashMap<&str, u32> = HashMap::new();

        for row in &observed {
            let seen = &row.observation;
            by_request.insert(row.request_id.as_str(), seen);

            let key = (row.api_family.clone(), row.model.clone());
            let entry = composition
                .entry(key.clone())
                .or_insert_with(|| Composition {
                    api_family: row.api_family.clone(),
                    model: row.model.clone(),
                    ..Composition::default()
                });
            entry.requests += 1;
            entry.body_bytes += u64::from(seen.body_bytes);
            entry.system_bytes += u64::from(seen.system_bytes);
            entry.tools_bytes += u64::from(seen.tools_bytes);
            entry.messages_bytes += u64::from(seen.messages_bytes);
            entry.text_bytes += u64::from(seen.text_bytes);
            entry.reasoning_bytes += u64::from(seen.reasoning_bytes);
            entry.reasoning_elided_bytes += u64::from(seen.reasoning_elided_bytes);
            entry.tools_compacted_bytes += u64::from(seen.tools_compacted_bytes);
            entry.tools_compacted_requests += u64::from(seen.tools_compacted_bytes > 0);
            *tools_seen.entry(key).or_default() += u64::from(seen.tools_count);
            entry.tool_search_requests += u64::from(seen.tool_search);
            if seen.tools_deferred > 0 || seen.tool_search {
                entry.deferring_requests += 1;
            } else if seen.tools_count >= MANY_TOOLS {
                entry.undeferred_requests += 1;
                entry.undeferred_tools_bytes += u64::from(seen.tools_bytes);
            }

            if seen.compared {
                report.compared += 1;
                if (seen.drift_in & !seen.repaired) | seen.drift_out == 0 {
                    report.stable += 1;
                }
                for (slot, (bit, _)) in report.drift.iter_mut().zip(RENDER_ORDER) {
                    if seen.drift_in & bit != 0 {
                        slot.client_turns += 1;
                    }
                    if seen.repaired & bit != 0 {
                        slot.repaired_turns += 1;
                    }
                    if seen.drift_out & bit != 0 {
                        slot.proxy_turns += 1;
                    }
                }
            }

            if let Some(kind) = seen.recompacted.as_deref() {
                if kind == "predicted" {
                    report.recompactions.predicted += 1;
                    // Counted as the proxy's breaker counted it when the
                    // answer arrived: the same classifier, on the same row.
                    let read = row.read.map(|read| u32::try_from(read).unwrap_or(u32::MAX));
                    let rounds = u32::try_from(row.rounds).unwrap_or(u32::MAX);
                    if seen.prediction_was_wrong(read, rounds) {
                        report.recompactions.predicted_wrong += 1;
                    }
                } else {
                    report.recompactions.observed += 1;
                }
                let held = ledgers
                    .entry(row.conversation.as_deref().unwrap_or(""))
                    .or_default();
                *held = (*held).max(seen.ledger_reads);
            }

            if let Some(volatile) = seen.volatile {
                report.volatile.requests += 1;
                report.volatile.with_timestamps += u64::from(volatile.timestamps > 0);
                report.volatile.with_dates += u64::from(volatile.dates > 0);
                report.volatile.with_uuids += u64::from(volatile.uuids > 0);
                report.volatile.with_jwts += u64::from(volatile.jwts > 0);
                if let Some(conversation) = row.conversation.as_deref() {
                    volatile_conversations.insert(conversation);
                }
            }

            // A lapse is scored on the requests the runtime could predict
            // on — a warm conversation, continued by this request — where
            // the provider said what it read, and not on a turn that was
            // itself recompacted, whose read says what was rewritten rather
            // than what had expired.
            //
            // Whether the prefix was lost is the cache walk's to say: the
            // request failed to read back what the one before it left. What
            // kind of loss is this row's: only one that took the whole of
            // the conversation's own messages, with no segment seen to
            // change, is what an expiry does. A loss that left part of the
            // conversation readable, or fell on a request whose prefix had
            // changed, would have happened whatever the idle time, and
            // counting it would credit the prediction with a rewrite.
            // On Anthropic the instructions and tools are often cached by
            // other conversations too, so a bare zero read is not the test.
            if let (true, Some(idle_ms), Some(read), None) = (
                seen.could_predict(),
                seen.idle_ms,
                row.read,
                seen.recompacted.as_ref(),
            ) {
                let outcome = seen.lapse_outcome(
                    lost.contains(row.request_id.as_str()),
                    u32::try_from(read).unwrap_or(u32::MAX),
                );
                let asked = seen.lifetime_s.map(|lifetime| vec![(lifetime, true)]);
                let lifetimes = asked.unwrap_or_else(|| {
                    ASSUMED_LIFETIMES_S
                        .iter()
                        .map(|lifetime| (*lifetime, false))
                        .collect()
                });
                for (lifetime_s, asked) in lifetimes {
                    for margin_s in MARGINS_S {
                        let slot = scores
                            .entry((row.api_family.clone(), lifetime_s, asked, margin_s))
                            .or_insert_with(|| score(&row.api_family, lifetime_s, asked, margin_s));
                        let predicted = idle_ms
                            > u64::from(lifetime_s.saturating_add(margin_s)).saturating_mul(1_000);
                        slot.scored += 1;
                        slot.predicted += u64::from(predicted);
                        match (predicted, outcome) {
                            (true, LapseOutcome::Full) => slot.confirmed += 1,
                            (true, LapseOutcome::ReadBack) => slot.wrong += 1,
                            (true, LapseOutcome::Partial) => slot.partial += 1,
                            (true, LapseOutcome::Drift) => slot.drift += 1,
                            (true, LapseOutcome::Unknown) => slot.unknown += 1,
                            (false, LapseOutcome::Full) => slot.missed += 1,
                            (false, _) => {}
                        }
                    }
                }
            }
        }
        report.volatile.conversations = volatile_conversations.len() as u64;
        report.recompactions.conversations = ledgers.len() as u64;
        report.recompactions.stale_reads = ledgers.values().map(|held| u64::from(*held)).sum();

        for loss in losses
            .iter()
            .filter(|loss| loss.cause == Cause::PrefixChange)
        {
            let attribution = &mut report.losses;
            attribution.total.add(loss.lost_tokens, loss.cost);
            let seen = (!loss.keep_warm)
                .then(|| by_request.get(loss.request_id.as_str()))
                .flatten();
            let Some(seen) = seen else {
                attribution.unobserved.add(loss.lost_tokens, loss.cost);
                continue;
            };
            if !seen.compared {
                attribution.not_compared.add(loss.lost_tokens, loss.cost);
                continue;
            }
            let Some((segment, by_proxy)) = blame(seen) else {
                attribution.none_seen.add(loss.lost_tokens, loss.cost);
                continue;
            };
            let slot = report
                .drift
                .iter_mut()
                .find(|slot| slot.segment == segment)
                .expect("every rendered segment has a row");
            if by_proxy {
                attribution.proxy.add(loss.lost_tokens, loss.cost);
                slot.proxy_losses += 1;
                slot.proxy_lost_tokens += loss.lost_tokens;
                slot.proxy_cost += loss.cost;
            } else {
                attribution.client.add(loss.lost_tokens, loss.cost);
                slot.client_losses += 1;
                slot.client_lost_tokens += loss.lost_tokens;
                slot.client_cost += loss.cost;
            }
        }

        report.composition = composition
            .into_iter()
            .map(|(key, mut entry)| {
                let body = entry.body_bytes as i64;
                entry.tools_share = ratio(entry.tools_bytes as i64, body);
                entry.system_share = ratio(entry.system_bytes as i64, body);
                entry.reasoning_share = ratio(entry.reasoning_bytes as i64, body);
                entry.opaque_share = ratio(
                    entry.messages_bytes.saturating_sub(entry.text_bytes) as i64,
                    entry.messages_bytes as i64,
                );
                entry.tools_per_request = ratio(
                    tools_seen.get(&key).copied().unwrap_or(0) as i64,
                    entry.requests as i64,
                );
                entry
            })
            .collect();
        report
            .composition
            .sort_by_key(|entry| std::cmp::Reverse(entry.body_bytes));
        report.cold = scores
            .into_values()
            .map(|mut slot| {
                slot.precision = ratio(slot.confirmed as i64, slot.predicted as i64);
                slot.recall = ratio(slot.confirmed as i64, (slot.confirmed + slot.missed) as i64);
                slot
            })
            .collect();
        Ok(report)
    }

    pub(crate) fn prefix_rows(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<(Vec<Observed>, bool), StatsError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT request_id, api_family, COALESCE(model, ''), conversation,
                    CASE WHEN prompt_tokens IS NULL AND cache_read_tokens IS NULL
                              AND cache_write_tokens IS NULL
                         THEN NULL ELSE COALESCE(cache_read_tokens, 0) END,
                    prefix_json, COALESCE(prompt_tokens, 0), retrieve_rounds
             FROM proxy_requests
             WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3
               AND keep_warm = 0 AND prefix_json IS NOT NULL
             ORDER BY time_ms DESC, id DESC
             LIMIT ?4",
        )?;
        let limit = (MAX_ROWS + 1) as i64;
        let mut rows = Vec::new();
        let mut found = stmt.query((tenant, from_ms, to_ms, limit))?;
        while let Some(row) = found.next()? {
            let json: String = row.get(5)?;
            // A row this build cannot read is skipped, not fatal: the column
            // outlives the struct that wrote it.
            let Ok(observation) = serde_json::from_str::<PrefixObservation>(&json) else {
                continue;
            };
            rows.push(Observed {
                request_id: row.get(0)?,
                api_family: row.get(1)?,
                model: row.get(2)?,
                conversation: row.get(3)?,
                read: row.get(4)?,
                prompt: row.get(6)?,
                rounds: row.get(7)?,
                observation,
            });
        }
        let truncated = rows.len() > MAX_ROWS;
        rows.truncate(MAX_ROWS);
        Ok((rows, truncated))
    }
}
