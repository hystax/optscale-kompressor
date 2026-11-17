//! Whether compression costs the prompt cache more than it saves.
//!
//! The cache report says a prefix was lost and whether the conversation had
//! been idle past the cache lifetime (`ttl_expiry`) or not (`prefix_change`).
//! A prefix change on a proxied conversation means something rewrote bytes the
//! provider had cached — the client, or the engine compressing a message
//! inside the cached prefix. This view separates the two as far as the rows
//! allow.
//!
//! The rows do not say which message a run rewrote, so the attribution is
//! statistical, with one direct signal:
//!
//! - every compared client turn (a `hit`, `partial` or `miss` in the cache
//!   walk — the walk is the cache report's own) is put in one of three
//!   groups by the engine run filed under its request id: `compressed` (the
//!   run rewrote at least one message), `unchanged` (it ran and rewrote
//!   nothing) or `no_engine` (no run: the proxy relayed it as it came);
//! - a prefix-change loss on a `compressed` turn is *with compression*: the
//!   upper bound on what compression could have caused;
//! - the loss rate of the turns that compression did not touch is the
//!   *baseline* — what clients and providers lose by themselves — and the
//!   compressed turns' losses above that rate are the *excess*, the estimate;
//! - a compressed turn whose run froze fewer messages than the conversation's
//!   previous engine run had the freeze lifted (the prefix tracker
//!   judged the cache cold) and so was free to rewrite cached messages. A loss
//!   there is the *likely* case.
//!
//! TTL losses are left out: an idle cache expires whatever the engine did.
//! Costs are the cache report's input-equivalent tokens, so the removed
//! tokens and the losses can be set against each other.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use super::{Cause, TurnKind, Walk, loss_cost, step};
use crate::live::bucket_for;
use crate::{StatsError, StatsReader};

/// Conversations listed when the caller names no count.
pub const DEFAULT_CONVERSATIONS: usize = 50;
/// Suspected losses listed, newest first.
pub const MAX_LOSSES: usize = 50;

/// The engine run filed under a request, as far as this view needs it.
#[derive(Clone, Copy, Debug, Default)]
struct Run {
    compressed: i64,
    frozen: Option<i64>,
    removed: i64,
}

/// Which of the three groups a compared client turn falls in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Group {
    Compressed,
    Unchanged,
    NoEngine,
}

/// The compared client turns of one group, and their prefix-change losses.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TurnGroup {
    pub group: Group,
    pub turns: u64,
    pub losses: u64,
    pub lost_tokens: i64,
    /// Input-equivalent tokens the losses cost over reading the prefix back.
    pub loss_cost: f64,
    /// `losses / turns`.
    pub loss_rate: Option<f64>,
    /// Tokens the engine removed from these turns' prompts.
    pub removed_tokens: i64,
}

impl TurnGroup {
    fn new(group: Group) -> Self {
        TurnGroup {
            group,
            turns: 0,
            losses: 0,
            lost_tokens: 0,
            loss_cost: 0.0,
            loss_rate: None,
            removed_tokens: 0,
        }
    }

    fn finish(&mut self) {
        self.loss_rate = (self.turns > 0).then(|| self.losses as f64 / self.turns as f64);
    }
}

/// Losses counted together, with what they cost.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LossTotal {
    pub events: f64,
    pub tokens: f64,
    pub cost: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CompressionCachePoint {
    pub bucket_ms: i64,
    pub compressed_turns: u64,
    pub compressed_losses: u64,
    pub other_turns: u64,
    pub other_losses: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CompressionCacheConversation {
    pub conversation: String,
    pub model: Option<String>,
    pub turns: u64,
    pub compressed_turns: u64,
    pub freeze_lifts: u64,
    /// Prefix-change losses on compressed turns.
    pub losses: u64,
    pub lost_tokens: i64,
    pub loss_cost: f64,
    pub removed_tokens: i64,
}

/// One prefix-change loss on a compressed turn.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SuspectLoss {
    pub time_ms: i64,
    pub request_id: String,
    pub conversation: String,
    pub model: Option<String>,
    pub messages: Option<i64>,
    pub compressed_messages: i64,
    pub frozen: Option<i64>,
    pub previous_frozen: Option<i64>,
    pub freeze_lifted: bool,
    pub expected: Option<i64>,
    pub read: Option<i64>,
    pub lost_tokens: i64,
    pub loss_cost: f64,
    pub removed_tokens: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CompressionCacheReport {
    pub tenant: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub bucket_ms: i64,
    pub rows: u64,
    pub truncated: bool,
    /// Compressed, unchanged and no-engine turns, in that order.
    pub groups: Vec<TurnGroup>,
    /// TTL losses on client turns: not compression's doing, shown for scale.
    pub ttl_losses: u64,
    pub ttl_loss_cost: f64,
    /// The loss rate of the turns compression did not touch.
    pub baseline_rate: Option<f64>,
    /// Every prefix-change loss on a compressed turn: the upper bound.
    pub with_compression: LossTotal,
    /// The compressed turns' losses above the baseline rate: the estimate.
    pub excess: LossTotal,
    /// Losses on compressed turns whose freeze had been lifted: the likely
    /// cases.
    pub freeze_lifted_turns: u64,
    pub likely: LossTotal,
    /// Tokens the engine removed from compared client turns.
    pub removed_tokens: i64,
    /// What those removed tokens would have cost had each been read from
    /// cache, the cheapest way the provider bills a prompt token: the
    /// least the compression saved, in input-equivalent tokens.
    pub removed_cost_floor: f64,
    pub timeline: Vec<CompressionCachePoint>,
    pub conversations: Vec<CompressionCacheConversation>,
    pub conversations_total: u64,
    pub losses: Vec<SuspectLoss>,
}

#[derive(Default)]
struct Track {
    walk: Walk,
    summary: CompressionCacheConversation,
    previous_frozen: Option<i64>,
}

impl StatsReader {
    /// The engine runs filed under requests in `[from_ms, to_ms)`, by request
    /// id. A request with more than one run keeps the one that removed most.
    fn runs(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<HashMap<String, Run>, StatsError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT request_id, messages_compressed, frozen_messages, tokens_in - tokens_out
               FROM optimization_events
              WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3",
        )?;
        let mut runs: HashMap<String, Run> = HashMap::new();
        let rows = stmt.query_map((tenant, from_ms, to_ms), |row| {
            Ok((
                row.get::<_, String>(0)?,
                Run {
                    compressed: row.get(1)?,
                    frozen: row.get(2)?,
                    removed: row.get::<_, i64>(3)?.max(0),
                },
            ))
        })?;
        for row in rows {
            let (id, run) = row?;
            runs.entry(id)
                .and_modify(|kept| {
                    if run.removed > kept.removed {
                        *kept = run;
                    }
                })
                .or_insert(run);
        }
        Ok(runs)
    }

    /// Prefix-change losses set against compression for `tenant` over
    /// `[from_ms, to_ms)`, listing at most `conversations` conversations.
    pub fn compression_cache(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
        conversations: usize,
    ) -> Result<CompressionCacheReport, StatsError> {
        let _snapshot = self.conn.unchecked_transaction()?;
        let (rows, truncated) = self.cache_rows(tenant, from_ms, to_ms)?;
        // A run is filed when the engine finishes, a little before the
        // request it belongs to; widen the window by a minute to catch it.
        let runs = self.runs(tenant, from_ms - 60_000, to_ms)?;
        let bucket_ms = bucket_for(to_ms.saturating_sub(from_ms));
        let mut groups = [
            TurnGroup::new(Group::Compressed),
            TurnGroup::new(Group::Unchanged),
            TurnGroup::new(Group::NoEngine),
        ];
        let mut report = CompressionCacheReport {
            tenant: tenant.to_string(),
            from_ms,
            to_ms,
            bucket_ms,
            rows: rows.len() as u64,
            truncated,
            ..Default::default()
        };
        let mut timeline: BTreeMap<i64, CompressionCachePoint> = BTreeMap::new();
        let mut tracks: HashMap<String, Track> = HashMap::new();
        let mut losses: Vec<SuspectLoss> = Vec::new();
        let mut floor = 0.0;

        for row in &rows {
            let Some(conversation) = row.conversation.as_deref() else {
                continue;
            };
            let usage = row.usage();
            let track = tracks
                .entry(conversation.to_string())
                .or_insert_with(|| Track {
                    walk: Walk {
                        summary: super::ConversationCache {
                            conversation: conversation.to_string(),
                            api_family: row.api_family.clone(),
                            first_ms: row.time_ms,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    summary: CompressionCacheConversation {
                        conversation: conversation.to_string(),
                        ..Default::default()
                    },
                    ..Default::default()
                });
            let turn = step(&mut track.walk, row, usage);
            if row.keep_warm {
                // The same bookkeeping the cache walk does between steps.
                track.walk.pings_since_client += 1;
                track.walk.pinged = true;
                track.walk.lost_since_client |= turn.lost_tokens > 0;
                continue;
            }
            if turn.kind != TurnKind::NoUsage {
                track.walk.last_client_end = Some(row.end_ms());
                track.walk.pings_since_client = 0;
                track.walk.lost_since_client = false;
            }
            if track.summary.model.is_none() {
                track.summary.model = row.model.clone();
            }
            if !matches!(
                turn.kind,
                TurnKind::Hit | TurnKind::Partial | TurnKind::Miss
            ) {
                continue;
            }
            if turn.cause == Some(Cause::TtlExpiry) {
                report.ttl_losses += 1;
                report.ttl_loss_cost += loss_cost(row, turn.lost_tokens);
                continue;
            }

            let run = runs.get(&row.request_id).copied();
            let group = match run {
                Some(run) if run.compressed > 0 => Group::Compressed,
                Some(_) => Group::Unchanged,
                None => Group::NoEngine,
            };
            let removed = run.map_or(0, |r| r.removed);
            let previous_frozen = track.previous_frozen;
            let frozen = run.and_then(|r| r.frozen);
            let lifted =
                matches!((frozen, previous_frozen), (Some(now), Some(before)) if now < before);
            if frozen.is_some() {
                track.previous_frozen = frozen;
            }
            let lost = turn.lost_tokens;
            let cost = if lost > 0 { loss_cost(row, lost) } else { 0.0 };
            let g = &mut groups[group as usize];
            g.turns += 1;
            g.removed_tokens += removed;
            if lost > 0 {
                g.losses += 1;
                g.lost_tokens += lost;
                g.loss_cost += cost;
            }
            report.removed_tokens += removed;
            floor += removed as f64 * row.rates().read;

            let point = timeline
                .entry(from_ms + ((row.time_ms - from_ms) / bucket_ms) * bucket_ms)
                .or_default();
            track.summary.turns += 1;
            if group != Group::Compressed {
                point.other_turns += 1;
                point.other_losses += u64::from(lost > 0);
                continue;
            }

            let run = run.expect("compressed turns have a run");
            point.compressed_turns += 1;
            track.summary.compressed_turns += 1;
            track.summary.removed_tokens += removed;
            if lifted {
                report.freeze_lifted_turns += 1;
                track.summary.freeze_lifts += 1;
            }
            if lost > 0 {
                point.compressed_losses += 1;
                track.summary.losses += 1;
                track.summary.lost_tokens += lost;
                track.summary.loss_cost += cost;
                if lifted {
                    report.likely.events += 1.0;
                    report.likely.tokens += lost as f64;
                    report.likely.cost += cost;
                }
                losses.push(SuspectLoss {
                    time_ms: row.time_ms,
                    request_id: row.request_id.clone(),
                    conversation: conversation.to_string(),
                    model: row.model.clone(),
                    messages: row.messages,
                    compressed_messages: run.compressed,
                    frozen: run.frozen,
                    previous_frozen,
                    freeze_lifted: lifted,
                    expected: turn.expected,
                    read: turn.read,
                    lost_tokens: lost,
                    loss_cost: cost,
                    removed_tokens: removed,
                });
            }
        }

        for g in &mut groups {
            g.finish();
        }
        let [compressed, unchanged, no_engine] = &groups;
        let untouched_turns = unchanged.turns + no_engine.turns;
        let untouched_losses = unchanged.losses + no_engine.losses;
        report.baseline_rate =
            (untouched_turns > 0).then(|| untouched_losses as f64 / untouched_turns as f64);
        report.with_compression = LossTotal {
            events: compressed.losses as f64,
            tokens: compressed.lost_tokens as f64,
            cost: compressed.loss_cost,
        };
        if compressed.losses > 0 {
            let expected = report.baseline_rate.unwrap_or(0.0) * compressed.turns as f64;
            let events = (compressed.losses as f64 - expected).max(0.0);
            let share = events / compressed.losses as f64;
            report.excess = LossTotal {
                events,
                tokens: compressed.lost_tokens as f64 * share,
                cost: compressed.loss_cost * share,
            };
        }
        report.removed_cost_floor = floor;
        report.groups = groups.to_vec();
        report.timeline = timeline
            .into_iter()
            .map(|(bucket_ms, point)| CompressionCachePoint { bucket_ms, ..point })
            .collect();
        let mut list: Vec<CompressionCacheConversation> = tracks
            .into_values()
            .map(|t| t.summary)
            .filter(|s| s.compressed_turns > 0)
            .collect();
        list.sort_by(|a, b| {
            b.loss_cost
                .total_cmp(&a.loss_cost)
                .then(b.removed_tokens.cmp(&a.removed_tokens))
                .then(a.conversation.cmp(&b.conversation))
        });
        report.conversations_total = list.len() as u64;
        list.truncate(conversations);
        report.conversations = list;
        losses.reverse();
        losses.truncate(MAX_LOSSES);
        report.losses = losses;
        Ok(report)
    }
}

#[cfg(test)]
#[path = "compression_tests.rs"]
mod tests;
