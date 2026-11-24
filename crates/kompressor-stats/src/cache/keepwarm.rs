//! What the keep-warm pings bought, priced against what they cost.
//!
//! The cache report counts pings and says whether a long pause was bridged.
//! This view prices both sides in the same input-equivalent tokens:
//!
//! - **ping cost**: every ping's own usage at its model's rates.
//! - **avoided cost**: for a pause longer than the cache lifetime that pings
//!   bridged (the next client turn read its prefix back and no ping found it
//!   gone), what rewriting the prefix that turn read would have cost over
//!   reading it: `loss_cost` of the tokens read.
//! - **missed cost**: for a pause nobody pinged that ended in a TTL loss, what
//!   that loss cost. This is the ceiling on what more pinging could recover.
//!
//! Pings sent after a conversation's last client turn in the window bridge
//! nothing anyone came back for; they are counted as `trailing`. Pings in a
//! pause shorter than the lifetime kept a cache warm that was warm anyway and
//! are counted as `unneeded`.
//!
//! The walk is the cache report's own (`step`), so a turn is a hit, a loss
//! or a branch here exactly when it is one there.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use super::{Cause, GAP_BUCKETS, LONG_TTL_MS, SHORT_TTL_MS, TurnKind, Walk, loss_cost, step};
use crate::live::bucket_for;
use crate::{StatsError, StatsReader};

/// Conversations listed when the caller names no count.
pub const DEFAULT_CONVERSATIONS: usize = 50;

/// One idle-gap bucket: the pauses that fell in it, and what pinging them
/// cost and saved.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KeepWarmGap {
    pub label: String,
    /// Upper bound of the bucket, exclusive; `None` for the open last one.
    pub upto_ms: Option<i64>,
    /// Client turns that followed a pause in this bucket.
    pub pauses: u64,
    /// Of those, pauses with at least one ping in them.
    pub pinged: u64,
    pub bridged: u64,
    pub not_bridged: u64,
    /// Pinged pauses no longer than the cache lifetime.
    pub unneeded: u64,
    pub ping_cost: f64,
    pub avoided_cost: f64,
    /// Unpinged pauses that ended in a TTL loss, and what those losses cost.
    pub unpinged_expired: u64,
    pub missed_cost: f64,
}

/// One conversation's pings, priced.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KeepWarmConversation {
    pub conversation: String,
    pub model: Option<String>,
    pub turns: u64,
    pub pings: u64,
    pub ping_cost: f64,
    pub bridged: u64,
    pub not_bridged: u64,
    pub trailing_pings: u64,
    pub avoided_cost: f64,
    pub missed_cost: f64,
    /// `avoided_cost - ping_cost`.
    pub net: f64,
}

/// One timeline bucket.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KeepWarmPoint {
    pub bucket_ms: i64,
    pub pings: u64,
    pub ping_cost: f64,
    pub avoided_cost: f64,
}

/// The keep-warm return report for one window.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KeepWarmReport {
    pub tenant: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub bucket_ms: i64,
    pub rows: u64,
    pub truncated: bool,
    pub pings: u64,
    /// Pings that read a cached prefix back.
    pub warm: u64,
    /// Pings that answered but read nothing: the prefix was already gone.
    pub cold: u64,
    pub failed: u64,
    pub ping_cost: f64,
    pub avoided_cost: f64,
    /// `avoided_cost - ping_cost`; positive means the pings paid for
    /// themselves.
    pub net: f64,
    /// `avoided_cost / ping_cost`.
    pub return_ratio: Option<f64>,
    pub bridged: u64,
    pub not_bridged: u64,
    pub unneeded: u64,
    pub trailing_pings: u64,
    pub trailing_cost: f64,
    pub unpinged_expired: u64,
    pub missed_cost: f64,
    pub gaps: Vec<KeepWarmGap>,
    pub timeline: Vec<KeepWarmPoint>,
    /// Conversations that sent pings or lost a prefix to an unpinged pause,
    /// most ping cost first.
    pub conversations: Vec<KeepWarmConversation>,
    pub conversations_total: u64,
}

/// A conversation's walk plus what this view adds to it.
#[derive(Default)]
struct Track {
    walk: Walk,
    summary: KeepWarmConversation,
    cost_since_client: f64,
}

impl StatsReader {
    /// Keep-warm costs and returns for `tenant` over `[from_ms, to_ms)`,
    /// listing at most `conversations` conversations.
    pub fn keep_warm(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
        conversations: usize,
    ) -> Result<KeepWarmReport, StatsError> {
        let (rows, truncated) = self.cache_rows(tenant, from_ms, to_ms)?;
        let bucket_ms = bucket_for(to_ms.saturating_sub(from_ms));
        let mut report = KeepWarmReport {
            tenant: tenant.to_string(),
            from_ms,
            to_ms,
            bucket_ms,
            rows: rows.len() as u64,
            truncated,
            gaps: GAP_BUCKETS
                .iter()
                .map(|(label, upto)| KeepWarmGap {
                    label: label.to_string(),
                    upto_ms: (*upto != i64::MAX).then_some(*upto),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let mut timeline: BTreeMap<i64, KeepWarmPoint> = BTreeMap::new();
        let mut tracks: HashMap<String, Track> = HashMap::new();

        for row in &rows {
            let usage = row.usage();
            let cost = usage.map_or(0.0, |u| u.cost(row.rates()));
            let point = timeline
                .entry(from_ms + ((row.time_ms - from_ms) / bucket_ms) * bucket_ms)
                .or_default();
            if row.keep_warm {
                report.pings += 1;
                report.ping_cost += cost;
                point.pings += 1;
                point.ping_cost += cost;
                let ok = row.status.is_some_and(|s| (200..300).contains(&s)) && row.error.is_none();
                match usage {
                    Some(u) if ok && u.read > 0 => report.warm += 1,
                    Some(_) if ok => report.cold += 1,
                    _ => report.failed += 1,
                }
            }
            let Some(conversation) = row.conversation.as_deref() else {
                continue;
            };
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
                    summary: KeepWarmConversation {
                        conversation: conversation.to_string(),
                        ..Default::default()
                    },
                    ..Default::default()
                });
            let turn = step(&mut track.walk, row, usage);
            if track.summary.model.is_none() {
                track.summary.model = row.model.clone();
            }
            if row.keep_warm {
                track.summary.pings += 1;
                track.summary.ping_cost += cost;
            } else {
                track.summary.turns += 1;
            }

            let walk = &mut track.walk;
            if !row.keep_warm
                && matches!(
                    turn.kind,
                    TurnKind::Hit | TurnKind::Partial | TurnKind::Miss
                )
                && let Some(end) = walk.last_client_end
            {
                let gap = (turn.time_ms - end).max(0);
                let ttl = if walk.summary.long_ttl {
                    LONG_TTL_MS
                } else {
                    SHORT_TTL_MS
                };
                let bucket = report
                    .gaps
                    .iter_mut()
                    .find(|b| b.upto_ms.is_none_or(|upto| gap < upto))
                    .expect("the last bucket is open");
                bucket.pauses += 1;
                if walk.pings_since_client > 0 {
                    bucket.pinged += 1;
                    bucket.ping_cost += track.cost_since_client;
                    if gap <= ttl {
                        bucket.unneeded += 1;
                        report.unneeded += 1;
                    } else if turn.lost_tokens > 0 || walk.lost_since_client {
                        bucket.not_bridged += 1;
                        report.not_bridged += 1;
                        track.summary.not_bridged += 1;
                    } else {
                        let avoided = loss_cost(row, turn.read.unwrap_or(0));
                        bucket.bridged += 1;
                        bucket.avoided_cost += avoided;
                        report.bridged += 1;
                        report.avoided_cost += avoided;
                        point.avoided_cost += avoided;
                        track.summary.bridged += 1;
                        track.summary.avoided_cost += avoided;
                    }
                } else if gap > ttl && turn.cause == Some(Cause::TtlExpiry) {
                    let missed = loss_cost(row, turn.lost_tokens);
                    bucket.unpinged_expired += 1;
                    bucket.missed_cost += missed;
                    report.unpinged_expired += 1;
                    report.missed_cost += missed;
                    track.summary.missed_cost += missed;
                }
            }
            // The same bookkeeping the cache report's walk does between steps.
            if row.keep_warm {
                walk.pings_since_client += 1;
                walk.pinged = true;
                walk.lost_since_client |= turn.lost_tokens > 0;
                track.cost_since_client += cost;
            } else if turn.kind != TurnKind::NoUsage {
                walk.last_client_end = Some(row.end_ms());
                walk.pings_since_client = 0;
                walk.lost_since_client = false;
                track.cost_since_client = 0.0;
            }
        }

        let mut listed = Vec::new();
        for (_, mut track) in tracks {
            if track.walk.pings_since_client > 0 {
                report.trailing_pings += track.walk.pings_since_client;
                report.trailing_cost += track.cost_since_client;
                track.summary.trailing_pings = track.walk.pings_since_client;
            }
            if track.summary.pings > 0 || track.summary.missed_cost > 0.0 {
                track.summary.net = track.summary.avoided_cost - track.summary.ping_cost;
                listed.push(track.summary);
            }
        }
        listed.sort_by(|a, b| {
            b.ping_cost
                .total_cmp(&a.ping_cost)
                .then(b.missed_cost.total_cmp(&a.missed_cost))
                .then(a.conversation.cmp(&b.conversation))
        });
        report.conversations_total = listed.len() as u64;
        listed.truncate(conversations);
        report.conversations = listed;
        report.net = report.avoided_cost - report.ping_cost;
        report.return_ratio =
            (report.ping_cost > 0.0).then(|| report.avoided_cost / report.ping_cost);
        report.timeline = timeline
            .into_iter()
            .map(|(bucket_ms, point)| KeepWarmPoint { bucket_ms, ..point })
            .collect();
        Ok(report)
    }
}
