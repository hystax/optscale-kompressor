//! What the provider's prompt cache did, conversation by conversation.
//!
//! The proxy files one `proxy_requests` row per request it relays and one per
//! keep-warm ping it sends, each with the provider's own usage split into fresh
//! input, cache reads and cache writes (and, for Anthropic, the part written as
//! one-hour entries), and each tagged with the conversation the prefix tracker
//! filed it under. [`StatsReader::cache`] walks those rows in time order, one
//! conversation at a time, and asks of every row the question the bill turns
//! on: did the provider read back what the previous request left in its cache?
//!
//! # How a row is classified
//!
//! After each request the provider holds a cached prefix. For Anthropic that is
//! what the request read plus what it wrote; for OpenAI, which caches without
//! being asked, it is the whole prompt. The next row in the same conversation
//! is expected to read that much back, or as much of it as its own prompt
//! still carries: a client that compacted its context sent a shorter prompt
//! and dropped the rest itself. What it reads short of that is *lost*:
//!
//! - `first`: the first row of the conversation inside the window. There is
//!   nothing to compare it with.
//! - `hit`: lost less than [`min_loss_tokens`]: 1,024 on Anthropic, the
//!   smallest prefix it caches, and 2,048 on OpenAI, whose gpt-5.5 caches at
//!   2,048-token intervals.
//! - `partial`: some of the prefix read back, at least that much not.
//! - `miss`: nothing read back.
//! - `branch`: a client request carrying fewer messages than the one before it
//!   in the same conversation — a Claude Code subagent summary or side query
//!   that re-sends an earlier history. It is not compared with the main line,
//!   and the main line is not compared with it.
//! - `no_usage`: the provider reported no usage (a failed request, a relayed
//!   WebSocket turn). It neither counts nor moves the comparison.
//!
//! A loss is put down to `ttl_expiry` when the conversation had been idle —
//! since its last request *or* ping — for longer than the cache lifetime: five
//! minutes, or an hour once the conversation has written one-hour entries.
//! Otherwise it is a `prefix_change`: something in the bytes changed, which on
//! a proxied conversation means the proxy or the client rewrote history.
//!
//! # What it costs
//!
//! Every token figure is also priced in *input-equivalent tokens*: fresh input
//! at 1, a cache read at the model's read rate (a tenth, or less on the models
//! [`rates`] names), an Anthropic write at 1.25 for five-minute entries and 2
//! for one-hour ones. The baseline is the same prompt with no cache at all,
//! every token at 1. Neither depends on a price list, so the share saved is
//! right whatever the model costs.
//!
//! The first row of each conversation in the window is compared with nothing,
//! so a window that starts mid-conversation under-counts losses by at most one
//! row per conversation.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::StatsError;
use crate::live::bucket_for;
use crate::reader::{StatsReader, ratio};

pub mod compression;
pub mod keepwarm;

/// Anthropic's default cache lifetime.
pub const SHORT_TTL_MS: i64 = 300_000;
/// The one-hour lifetime a request can ask for.
pub const LONG_TTL_MS: i64 = 3_600_000;
/// The smallest prefix a provider caches; a shortfall below it is not a loss.
pub const MIN_LOSS_TOKENS: i64 = 1_024;
/// The same for OpenAI, whose gpt-5.5 caches at 2,048-token intervals: a
/// prompt that grew can read back up to 2,047 tokens short of the last one
/// with nothing lost.
pub const OPENAI_MIN_LOSS_TOKENS: i64 = 2_048;

/// The smallest shortfall that counts as a loss on `api_family`.
pub fn min_loss_tokens(api_family: &str) -> i64 {
    if is_anthropic(api_family) {
        MIN_LOSS_TOKENS
    } else {
        OPENAI_MIN_LOSS_TOKENS
    }
}
/// The most rows one report reads. The newest are kept.
pub const MAX_ROWS: usize = 200_000;
/// The most turns listed for one conversation. The newest are kept.
pub const MAX_TURNS: usize = 2_000;

/// Idle-gap buckets, by upper bound. A gap is the time from the end of a
/// client request to the start of the next one in the same conversation.
pub const GAP_BUCKETS: &[(&str, i64)] = &[
    ("under 1m", 60_000),
    ("1-5m", SHORT_TTL_MS),
    ("5-10m", 600_000),
    ("10-30m", 1_800_000),
    ("30-60m", LONG_TTL_MS),
    ("over 1h", i64::MAX),
];

/// Prices relative to the model's fresh-input price.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rates {
    pub read: f64,
    pub write_5m: f64,
    pub write_1h: f64,
}

/// The cache prices for one row, mirroring the proxy's `CachePrices`.
///
/// Anthropic writes at 1.25x input for five-minute entries and 2x for one-hour
/// ones, and reads at a tenth except where named: Claude Opus 5.5 and Sonnet
/// 5.5 at a twentieth, Fable 5.1 and Mythos 5.1 at a fortieth. OpenAI reads at
/// a tenth and charges nothing to write. A model routed as `anthropic/<id>` is
/// priced as `<id>`.
pub fn rates(api_family: &str, model: Option<&str>) -> Rates {
    if !is_anthropic(api_family) {
        return Rates {
            read: 0.1,
            write_5m: 1.0,
            write_1h: 1.0,
        };
    }
    let model = model.unwrap_or_default();
    let model = model.strip_prefix("anthropic/").unwrap_or(model);
    let read = if model.starts_with("claude-opus-5-5") || model.starts_with("claude-sonnet-5-5") {
        0.05
    } else if model.starts_with("claude-fable-5-1") || model.starts_with("claude-mythos-5-1") {
        0.025
    } else {
        0.1
    };
    Rates {
        read,
        write_5m: 1.25,
        write_1h: 2.0,
    }
}

fn is_anthropic(api_family: &str) -> bool {
    api_family == "anthropic_messages"
}

/// Input tokens by how the provider billed them, and what that came to.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CacheTokens {
    /// Rows counted, pings included.
    pub requests: u64,
    /// Rows the provider reported usage for; the sums cover only these.
    pub with_usage: u64,
    /// Input billed at the full price.
    pub uncached: i64,
    pub read: i64,
    pub write_5m: i64,
    pub write_1h: i64,
    pub output: i64,
    /// Every input token, however billed.
    pub input: i64,
    /// `read / input`.
    pub read_share: f64,
    /// Input priced in input-equivalent tokens.
    pub cost: f64,
    /// The same input with no cache: `input`, every token at full price.
    pub baseline: f64,
    /// `1 - cost / baseline`. Negative when writes cost more than reads saved.
    pub saved_share: f64,
}

impl CacheTokens {
    fn add(&mut self, row: &Row) {
        self.requests += 1;
        let Some(usage) = row.usage() else {
            return;
        };
        self.with_usage += 1;
        self.uncached += usage.uncached;
        self.read += usage.read;
        self.write_5m += usage.write_5m;
        self.write_1h += usage.write_1h;
        self.output += row.completion.unwrap_or(0);
        self.input += usage.input();
        self.cost += usage.cost(row.rates());
    }

    fn finish(&mut self) {
        self.read_share = ratio(self.read, self.input);
        self.baseline = self.input as f64;
        self.saved_share = if self.baseline > 0.0 {
            1.0 - self.cost / self.baseline
        } else {
            0.0
        };
    }
}

/// Prefix tokens the provider failed to read back, by cause.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Losses {
    /// Rows that lost at least [`min_loss_tokens`].
    pub events: u64,
    pub tokens: i64,
    pub ttl_expiry_events: u64,
    pub ttl_expiry_tokens: i64,
    pub prefix_change_events: u64,
    pub prefix_change_tokens: i64,
    /// What the losses cost over reading the same tokens back, in
    /// input-equivalent tokens: each lost token was billed as a write (or as
    /// fresh input) instead of as a read.
    pub cost: f64,
}

impl Losses {
    fn add(&mut self, cause: Cause, tokens: i64, extra_cost: f64) {
        self.events += 1;
        self.tokens += tokens;
        self.cost += extra_cost;
        match cause {
            Cause::TtlExpiry => {
                self.ttl_expiry_events += 1;
                self.ttl_expiry_tokens += tokens;
            }
            Cause::PrefixChange => {
                self.prefix_change_events += 1;
                self.prefix_change_tokens += tokens;
            }
        }
    }
}

/// What the keep-warm pings did.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KeepWarm {
    pub pings: u64,
    /// Pings that read the prefix back: the cache was alive and is now
    /// refreshed.
    pub warm: u64,
    /// Pings that read nothing: the prefix had already lapsed, and the ping
    /// paid to write it again.
    pub cold: u64,
    /// Pings refused or unanswered.
    pub failed: u64,
    /// Conversations at least one ping went to.
    pub conversations: u64,
    /// Prefix tokens the warm pings refreshed.
    pub read: i64,
    pub output: i64,
    /// What the pings cost, in input-equivalent tokens, output excluded.
    pub cost: f64,
    /// Client turns that came back after an idle gap longer than the five-minute
    /// lifetime, with pings in between, and read their prefix back without any
    /// of those pings finding it gone.
    pub bridged: u64,
    /// The same, but the prefix was lost anyway: by the turn, or by a ping in
    /// the gap that found it lapsed and wrote it again.
    pub not_bridged: u64,
}

/// Client turns by how long the conversation had been idle before them.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GapBucket {
    pub label: String,
    /// Upper bound in milliseconds; `None` for the last, open bucket.
    pub upto_ms: Option<i64>,
    pub turns: u64,
    pub hits: u64,
    pub losses: u64,
    pub lost_tokens: i64,
    /// Turns with at least one keep-warm ping inside the gap.
    pub with_pings: u64,
}

/// One timeline point.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CachePoint {
    pub bucket_ms: i64,
    pub uncached: i64,
    pub read: i64,
    pub write: i64,
    pub pings: u64,
    pub lost_tokens: i64,
}

/// Cache use by the model a request named.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelCache {
    pub model: String,
    pub api_family: String,
    #[serde(flatten)]
    pub tokens: CacheTokens,
    pub rates: Option<Rates>,
}

/// One conversation's account.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConversationCache {
    pub conversation: String,
    pub model: Option<String>,
    pub api_family: String,
    pub first_ms: i64,
    pub last_ms: i64,
    /// Client requests, pings excluded.
    pub turns: u64,
    pub pings: u64,
    pub branches: u64,
    pub tokens: CacheTokens,
    pub losses: Losses,
    pub longest_gap_ms: i64,
    /// Whether the conversation has written one-hour entries.
    pub long_ttl: bool,
}

/// Why a loss happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    TtlExpiry,
    PrefixChange,
}

/// What one row did to the cache. See the module documentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnKind {
    First,
    Hit,
    Partial,
    Miss,
    Branch,
    NoUsage,
}

/// One row of one conversation, for the detail view.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Turn {
    pub time_ms: i64,
    pub request_id: String,
    pub keep_warm: bool,
    pub model: Option<String>,
    pub status: Option<i64>,
    pub messages: Option<i64>,
    /// Idle time since the conversation's previous request or ping ended.
    pub gap_ms: Option<i64>,
    pub uncached: Option<i64>,
    pub read: Option<i64>,
    pub write_5m: Option<i64>,
    pub write_1h: Option<i64>,
    pub output: Option<i64>,
    /// What the provider was expected to read back.
    pub expected: Option<i64>,
    pub kind: TurnKind,
    pub cause: Option<Cause>,
    pub lost_tokens: i64,
    pub error: Option<String>,
}

/// Everything the cache view shows, for one tenant and one window.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CacheReport {
    pub tenant: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub bucket_ms: i64,
    pub rows: u64,
    /// More than [`MAX_ROWS`] rows matched; only the newest were read.
    pub truncated: bool,
    /// Every row, pings included.
    pub totals: CacheTokens,
    /// Client requests only.
    pub clients: CacheTokens,
    /// Keep-warm pings only, as tokens.
    pub pings: CacheTokens,
    pub losses: Losses,
    pub keep_warm: KeepWarm,
    pub gaps: Vec<GapBucket>,
    pub timeline: Vec<CachePoint>,
    pub by_model: Vec<ModelCache>,
    /// Conversations in the window, the costliest losses first.
    pub conversations: Vec<ConversationCache>,
    pub conversations_total: u64,
    /// Rows filed under no conversation: nothing was frozen for them, so they
    /// cannot be compared with anything. In `totals`, not in `losses`.
    pub unattributed: CacheTokens,
    /// The rows of the conversation asked for, oldest first.
    pub turns: Option<Vec<Turn>>,
}

/// One `proxy_requests` row, as the walk needs it.
#[derive(Clone, Debug)]
struct Row {
    time_ms: i64,
    request_id: String,
    api_family: String,
    model: Option<String>,
    status: Option<i64>,
    prompt: Option<i64>,
    completion: Option<i64>,
    read: Option<i64>,
    write: Option<i64>,
    write_1h: Option<i64>,
    conversation: Option<String>,
    keep_warm: bool,
    duration_ms: f64,
    error: Option<String>,
    messages: Option<i64>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Usage {
    uncached: i64,
    read: i64,
    write_5m: i64,
    write_1h: i64,
}

impl Usage {
    pub(crate) fn input(self) -> i64 {
        self.uncached + self.read + self.write_5m + self.write_1h
    }

    pub(crate) fn cost(self, rates: Rates) -> f64 {
        self.uncached as f64
            + self.read as f64 * rates.read
            + self.write_5m as f64 * rates.write_5m
            + self.write_1h as f64 * rates.write_1h
    }
}

pub(crate) fn normalize_usage(
    api_family: &str,
    prompt: Option<i64>,
    read: Option<i64>,
    write: Option<i64>,
    write_1h: Option<i64>,
) -> Option<Usage> {
    if prompt.is_none() && read.is_none() && write.is_none() {
        return None;
    }
    let prompt = prompt.unwrap_or(0).max(0);
    let read = read.unwrap_or(0).max(0);
    let write = write.unwrap_or(0).max(0);
    let write_1h = write_1h.unwrap_or(0).clamp(0, write);
    Some(if is_anthropic(api_family) {
        Usage {
            uncached: prompt,
            read,
            write_5m: write - write_1h,
            write_1h,
        }
    } else {
        Usage {
            uncached: (prompt - read).max(0),
            read,
            write_5m: 0,
            write_1h: 0,
        }
    })
}

impl Row {
    fn rates(&self) -> Rates {
        rates(&self.api_family, self.model.as_deref())
    }

    /// The provider's usage, normalised: Anthropic reports fresh input, reads
    /// and writes as three disjoint counts; OpenAI's prompt already includes
    /// its cached part and it reports no writes.
    fn usage(&self) -> Option<Usage> {
        normalize_usage(
            &self.api_family,
            self.prompt,
            self.read,
            self.write,
            self.write_1h,
        )
    }

    /// What the provider holds after this row, for the next to read back.
    fn cached_after(&self, usage: Usage) -> i64 {
        if is_anthropic(&self.api_family) {
            usage.read + usage.write_5m + usage.write_1h
        } else {
            usage.input()
        }
    }

    fn end_ms(&self) -> i64 {
        self.time_ms + self.duration_ms.max(0.0) as i64
    }
}

/// The walk's state for one conversation.
#[derive(Default)]
struct Walk {
    summary: ConversationCache,
    expected: Option<i64>,
    /// End of the last request or ping that reported usage.
    last_end: Option<i64>,
    /// End of the last client request, and pings sent since.
    last_client_end: Option<i64>,
    pings_since_client: u64,
    /// Whether a ping since the last client request found the prefix gone:
    /// the pause was not bridged, whatever the next turn reads.
    lost_since_client: bool,
    messages: Option<i64>,
    pinged: bool,
    turns: Vec<Turn>,
}

impl StatsReader {
    /// The cache report for `tenant` over `[from_ms, to_ms)`, listing at most
    /// `conversations` conversations, with the turns of `detail` when named.
    pub fn cache(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
        conversations: usize,
        detail: Option<&str>,
    ) -> Result<CacheReport, StatsError> {
        let (rows, truncated) = self.cache_rows(tenant, from_ms, to_ms)?;
        Ok(build(
            tenant,
            from_ms,
            to_ms,
            &rows,
            truncated,
            conversations,
            detail,
        ))
    }

    fn cache_rows(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<(Vec<Row>, bool), StatsError> {
        // Newest first under the limit, so a truncated report keeps what just
        // happened; turned back to time order below.
        let mut stmt = self.conn.prepare_cached(
            "SELECT p.time_ms, p.request_id, p.api_family, p.model, p.status,
                    p.prompt_tokens, p.completion_tokens, p.cache_read_tokens,
                    p.cache_write_tokens, p.cache_write_1h_tokens, p.conversation,
                    p.keep_warm, COALESCE(p.total_ms, p.upstream_ms, 0.0), p.error,
                    (SELECT MAX(o.messages_total) FROM optimization_events o
                      WHERE o.tenant = p.tenant AND o.request_id = p.request_id)
             FROM proxy_requests p
             WHERE p.tenant = ?1 AND p.time_ms >= ?2 AND p.time_ms < ?3
             ORDER BY p.time_ms DESC, p.id DESC
             LIMIT ?4",
        )?;
        let limit = (MAX_ROWS + 1) as i64;
        let mut rows = stmt
            .query_map((tenant, from_ms, to_ms, limit), |row| {
                Ok(Row {
                    time_ms: row.get(0)?,
                    request_id: row.get(1)?,
                    api_family: row.get(2)?,
                    model: row.get(3)?,
                    status: row.get(4)?,
                    prompt: row.get(5)?,
                    completion: row.get(6)?,
                    read: row.get(7)?,
                    write: row.get(8)?,
                    write_1h: row.get(9)?,
                    conversation: row.get(10)?,
                    keep_warm: row.get::<_, i64>(11)? != 0,
                    duration_ms: row.get(12)?,
                    error: row.get(13)?,
                    messages: row.get(14)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let truncated = rows.len() > MAX_ROWS;
        rows.truncate(MAX_ROWS);
        rows.reverse();
        Ok((rows, truncated))
    }
}

/// One row that failed to read back the prefix the row before it left.
#[derive(Clone, Debug)]
pub(crate) struct LossRow {
    pub(crate) request_id: String,
    pub(crate) keep_warm: bool,
    pub(crate) cause: Cause,
    pub(crate) lost_tokens: i64,
    /// What the loss cost, in input-equivalent tokens.
    pub(crate) cost: f64,
}

impl StatsReader {
    /// Every loss in the window, by the row it happened on: the same walk the
    /// cache report makes, kept per request so another report can say what
    /// else was true of that request.
    pub(crate) fn cache_losses(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<(Vec<LossRow>, bool), StatsError> {
        let (rows, truncated) = self.cache_rows(tenant, from_ms, to_ms)?;
        let mut walks: HashMap<&str, Walk> = HashMap::new();
        let mut losses = Vec::new();
        for row in &rows {
            let Some(conversation) = row.conversation.as_deref() else {
                continue;
            };
            let walk = walks.entry(conversation).or_default();
            let turn = step(walk, row, row.usage());
            if turn.lost_tokens > 0 {
                losses.push(LossRow {
                    request_id: row.request_id.clone(),
                    keep_warm: row.keep_warm,
                    cause: turn.cause.unwrap_or(Cause::PrefixChange),
                    lost_tokens: turn.lost_tokens,
                    cost: loss_cost(row, turn.lost_tokens),
                });
            }
        }
        Ok((losses, truncated))
    }
}

fn build(
    tenant: &str,
    from_ms: i64,
    to_ms: i64,
    rows: &[Row],
    truncated: bool,
    limit: usize,
    detail: Option<&str>,
) -> CacheReport {
    let bucket_ms = bucket_for(to_ms.saturating_sub(from_ms));
    let mut totals = CacheTokens::default();
    let mut clients = CacheTokens::default();
    let mut pings = CacheTokens::default();
    let mut unattributed = CacheTokens::default();
    let mut losses = Losses::default();
    let mut keep_warm = KeepWarm::default();
    let mut gaps: Vec<GapBucket> = GAP_BUCKETS
        .iter()
        .map(|(label, upto)| GapBucket {
            label: label.to_string(),
            upto_ms: (*upto != i64::MAX).then_some(*upto),
            ..Default::default()
        })
        .collect();
    let mut timeline: BTreeMap<i64, CachePoint> = BTreeMap::new();
    let mut models: BTreeMap<(String, String), CacheTokens> = BTreeMap::new();
    let mut walks: HashMap<String, Walk> = HashMap::new();

    for row in rows {
        totals.add(row);
        if row.keep_warm {
            pings.add(row);
        } else {
            clients.add(row);
        }
        models
            .entry((
                row.model.clone().unwrap_or_default(),
                row.api_family.clone(),
            ))
            .or_default()
            .add(row);
        let usage = row.usage();
        let point = timeline
            .entry(from_ms + ((row.time_ms - from_ms) / bucket_ms) * bucket_ms)
            .or_default();
        if let Some(usage) = usage {
            point.uncached += usage.uncached;
            point.read += usage.read;
            point.write += usage.write_5m + usage.write_1h;
        }
        if row.keep_warm {
            point.pings += 1;
            keep_warm.pings += 1;
            keep_warm.output += row.completion.unwrap_or(0);
            let ok = row.status.is_some_and(|s| (200..300).contains(&s)) && row.error.is_none();
            match usage {
                Some(usage) if ok => {
                    keep_warm.cost += usage.cost(row.rates());
                    if usage.read > 0 {
                        keep_warm.warm += 1;
                        keep_warm.read += usage.read;
                    } else {
                        keep_warm.cold += 1;
                    }
                }
                Some(usage) => {
                    keep_warm.cost += usage.cost(row.rates());
                    keep_warm.failed += 1;
                }
                None => keep_warm.failed += 1,
            }
        }

        let Some(conversation) = row.conversation.as_deref() else {
            unattributed.add(row);
            continue;
        };
        let walk = walks
            .entry(conversation.to_string())
            .or_insert_with(|| Walk {
                summary: ConversationCache {
                    conversation: conversation.to_string(),
                    api_family: row.api_family.clone(),
                    first_ms: row.time_ms,
                    ..Default::default()
                },
                ..Default::default()
            });
        let turn = step(walk, row, usage);
        if turn.lost_tokens > 0 {
            point.lost_tokens += turn.lost_tokens;
            let cause = turn.cause.unwrap_or(Cause::PrefixChange);
            losses.add(cause, turn.lost_tokens, loss_cost(row, turn.lost_tokens));
        }
        if !row.keep_warm
            && matches!(
                turn.kind,
                TurnKind::Hit | TurnKind::Partial | TurnKind::Miss
            )
            && let Some(gap) = turn_client_gap(&turn, walk)
        {
            let bucket = gaps
                .iter_mut()
                .find(|b| b.upto_ms.is_none_or(|upto| gap < upto))
                .expect("the last bucket is open");
            bucket.turns += 1;
            if turn.lost_tokens > 0 {
                bucket.losses += 1;
                bucket.lost_tokens += turn.lost_tokens;
            } else {
                bucket.hits += 1;
            }
            let pinged = walk.pings_since_client > 0;
            if pinged {
                bucket.with_pings += 1;
            }
            if pinged && gap > SHORT_TTL_MS {
                if turn.lost_tokens > 0 || walk.lost_since_client {
                    keep_warm.not_bridged += 1;
                } else {
                    keep_warm.bridged += 1;
                }
            }
        }
        if row.keep_warm {
            walk.pings_since_client += 1;
            walk.pinged = true;
            walk.lost_since_client |= turn.lost_tokens > 0;
        } else if turn.kind != TurnKind::NoUsage {
            walk.last_client_end = Some(row.end_ms());
            walk.pings_since_client = 0;
            walk.lost_since_client = false;
        }
        if detail == Some(conversation) {
            walk.turns.push(turn);
        }
    }

    let mut conversations: Vec<ConversationCache> = Vec::with_capacity(walks.len());
    let mut turns = None;
    for (id, mut walk) in walks {
        walk.summary.tokens.finish();
        if walk.pinged {
            keep_warm.conversations += 1;
        }
        if detail == Some(id.as_str()) {
            let mut list = std::mem::take(&mut walk.turns);
            if list.len() > MAX_TURNS {
                list.drain(..list.len() - MAX_TURNS);
            }
            turns = Some(list);
        }
        conversations.push(walk.summary);
    }
    if detail.is_some() && turns.is_none() {
        turns = Some(Vec::new());
    }
    conversations.sort_by(|a, b| {
        b.losses
            .cost
            .total_cmp(&a.losses.cost)
            .then(b.last_ms.cmp(&a.last_ms))
            .then(a.conversation.cmp(&b.conversation))
    });
    let conversations_total = conversations.len() as u64;
    conversations.truncate(limit);

    for tokens in [&mut totals, &mut clients, &mut pings, &mut unattributed] {
        tokens.finish();
    }
    let mut by_model: Vec<ModelCache> = models
        .into_iter()
        .map(|((model, api_family), mut tokens)| {
            tokens.finish();
            let rates = (!model.is_empty()).then(|| rates(&api_family, Some(&model)));
            ModelCache {
                model,
                api_family,
                tokens,
                rates,
            }
        })
        .collect();
    by_model.sort_by_key(|m| std::cmp::Reverse(m.tokens.input));

    CacheReport {
        tenant: tenant.to_string(),
        from_ms,
        to_ms,
        bucket_ms,
        rows: rows.len() as u64,
        truncated,
        totals,
        clients,
        pings,
        losses,
        keep_warm,
        gaps,
        timeline: timeline
            .into_iter()
            .map(|(bucket_ms, point)| CachePoint { bucket_ms, ..point })
            .collect(),
        by_model,
        conversations,
        conversations_total,
        unattributed,
        turns,
    }
}

/// The idle time a client turn followed, measured from the previous client
/// request, so that pings inside the gap do not shorten it.
fn turn_client_gap(turn: &Turn, walk: &Walk) -> Option<i64> {
    walk.last_client_end.map(|end| (turn.time_ms - end).max(0))
}

/// Advance one conversation by one row and say what the row did.
fn step(walk: &mut Walk, row: &Row, usage: Option<Usage>) -> Turn {
    let summary = &mut walk.summary;
    summary.last_ms = summary.last_ms.max(row.time_ms);
    if summary.model.is_none() {
        summary.model = row.model.clone();
    }
    if row.keep_warm {
        summary.pings += 1;
    } else {
        summary.turns += 1;
    }
    summary.tokens.add(row);

    let gap_ms = walk.last_end.map(|end| (row.time_ms - end).max(0));
    let mut turn = Turn {
        time_ms: row.time_ms,
        request_id: row.request_id.clone(),
        keep_warm: row.keep_warm,
        model: row.model.clone(),
        status: row.status,
        messages: row.messages,
        gap_ms,
        uncached: usage.map(|u| u.uncached),
        read: usage.map(|u| u.read),
        write_5m: usage.map(|u| u.write_5m),
        write_1h: usage.map(|u| u.write_1h),
        output: row.completion,
        expected: walk.expected,
        kind: TurnKind::NoUsage,
        cause: None,
        lost_tokens: 0,
        error: row.error.clone(),
    };
    let Some(usage) = usage else {
        return turn;
    };

    let branch = !row.keep_warm
        && matches!((row.messages, walk.messages), (Some(now), Some(before)) if now < before);
    if branch {
        summary.branches += 1;
        turn.kind = TurnKind::Branch;
        turn.expected = None;
        return turn;
    }

    if let Some(gap) = gap_ms
        && !row.keep_warm
    {
        // Pings shorten the idle time the cache saw, not the agent's pause;
        // the longest gap is the agent's.
        let client_gap = walk
            .last_client_end
            .map(|end| (row.time_ms - end).max(0))
            .unwrap_or(gap);
        summary.longest_gap_ms = summary.longest_gap_ms.max(client_gap);
    }

    // What was cached can only be read back as far as this prompt still
    // carries it. A prompt shorter than the one before — a client that
    // compacted its context, as Codex does — dropped the rest itself; that is
    // not a cache that failed to serve it.
    let expected = walk.expected.map(|cached| cached.min(usage.input()));
    turn.expected = expected;
    turn.kind = match expected {
        None => TurnKind::First,
        Some(expected) => {
            let lost = (expected - usage.read).max(0);
            if lost < min_loss_tokens(&row.api_family) {
                TurnKind::Hit
            } else {
                let ttl = if summary.long_ttl {
                    LONG_TTL_MS
                } else {
                    SHORT_TTL_MS
                };
                let cause = if gap_ms.is_some_and(|gap| gap > ttl) {
                    Cause::TtlExpiry
                } else {
                    Cause::PrefixChange
                };
                turn.cause = Some(cause);
                turn.lost_tokens = lost;
                summary.losses.add(cause, lost, loss_cost(row, lost));
                if usage.read > 0 {
                    TurnKind::Partial
                } else {
                    TurnKind::Miss
                }
            }
        }
    };

    if usage.write_1h > 0 {
        summary.long_ttl = true;
    }
    walk.expected = Some(row.cached_after(usage));
    walk.last_end = Some(row.end_ms());
    if !row.keep_warm && row.messages.is_some() {
        walk.messages = row.messages;
    }
    turn
}

/// What `lost` tokens cost over reading them back: billed as a write on
/// Anthropic (at the row's TTL), as fresh input on OpenAI.
fn loss_cost(row: &Row, lost: i64) -> f64 {
    let rates = row.rates();
    let write = if is_anthropic(&row.api_family) {
        if row.write_1h.unwrap_or(0) > 0 {
            rates.write_1h
        } else {
            rates.write_5m
        }
    } else {
        1.0
    };
    lost as f64 * (write - rates.read)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_700_000_000_000;
    const MIN: i64 = 60_000;

    fn row(time_ms: i64, conversation: &str, read: i64, write: i64) -> Row {
        Row {
            time_ms,
            request_id: format!("r{time_ms}"),
            api_family: "anthropic_messages".into(),
            model: Some("claude-opus-5-5".into()),
            status: Some(200),
            prompt: Some(10),
            completion: Some(50),
            read: Some(read),
            write: Some(write),
            write_1h: Some(0),
            conversation: Some(conversation.into()),
            keep_warm: false,
            duration_ms: 1_000.0,
            error: None,
            messages: None,
        }
    }

    fn ping(time_ms: i64, conversation: &str, read: i64) -> Row {
        Row {
            keep_warm: true,
            completion: Some(1),
            prompt: Some(1),
            ..row(time_ms, conversation, read, 0)
        }
    }

    fn report(rows: &[Row], detail: Option<&str>) -> CacheReport {
        build("", T0, T0 + 600 * MIN, rows, false, 50, detail)
    }

    fn kinds(report: &CacheReport) -> Vec<TurnKind> {
        report
            .turns
            .as_ref()
            .expect("detail asked for")
            .iter()
            .map(|t| t.kind)
            .collect()
    }

    #[test]
    fn a_conversation_that_reads_its_prefix_back_loses_nothing() {
        let rows = [
            row(T0, "c", 0, 20_000),
            row(T0 + MIN, "c", 20_000, 2_000),
            row(T0 + 2 * MIN, "c", 22_000, 1_500),
        ];
        let report = report(&rows, Some("c"));
        assert_eq!(
            kinds(&report),
            [TurnKind::First, TurnKind::Hit, TurnKind::Hit]
        );
        assert_eq!(report.losses.events, 0);
        assert_eq!(report.totals.read, 42_000);
        assert_eq!(report.totals.write_5m, 23_500);
        assert_eq!(report.totals.uncached, 30);
        assert_eq!(report.totals.input, 65_530);
        // Opus 5.5 reads at a twentieth.
        let cost = 30.0 + 42_000.0 * 0.05 + 23_500.0 * 1.25;
        assert!((report.totals.cost - cost).abs() < 1e-6);
        assert!(report.totals.saved_share > 0.0);
    }

    #[test]
    fn a_miss_after_an_idle_gap_is_expiry_and_one_without_is_a_prefix_change() {
        let rows = [
            row(T0, "c", 0, 20_000),
            // Back after nine minutes: the five-minute entry has lapsed.
            row(T0 + 10 * MIN, "c", 0, 21_000),
            // Back after a minute, yet only half read back.
            row(T0 + 12 * MIN, "c", 10_000, 12_000),
        ];
        let report = report(&rows, Some("c"));
        assert_eq!(
            kinds(&report),
            [TurnKind::First, TurnKind::Miss, TurnKind::Partial]
        );
        let turns = report.turns.as_ref().unwrap();
        assert_eq!(turns[1].cause, Some(Cause::TtlExpiry));
        assert_eq!(turns[1].lost_tokens, 20_000);
        assert_eq!(turns[2].cause, Some(Cause::PrefixChange));
        assert_eq!(turns[2].lost_tokens, 11_000);
        assert_eq!(report.losses.ttl_expiry_tokens, 20_000);
        assert_eq!(report.losses.prefix_change_tokens, 11_000);
        assert_eq!(report.conversations[0].losses.events, 2);
        // Each lost token was written at 1.25 instead of read at 0.05.
        assert!((report.losses.cost - 31_000.0 * 1.2).abs() < 1e-6);
    }

    #[test]
    fn a_small_shortfall_is_not_a_loss() {
        let rows = [row(T0, "c", 0, 20_000), row(T0 + MIN, "c", 19_500, 900)];
        let report = report(&rows, Some("c"));
        assert_eq!(kinds(&report), [TurnKind::First, TurnKind::Hit]);
        assert_eq!(report.losses.events, 0);
    }

    #[test]
    fn pings_bridge_a_long_gap_and_are_counted_apart_from_the_agent() {
        let rows = [
            row(T0, "c", 0, 20_000),
            ping(T0 + 4 * MIN + 30_000, "c", 20_000),
            ping(T0 + 9 * MIN, "c", 20_000),
            row(T0 + 12 * MIN, "c", 20_000, 500),
        ];
        let report = report(&rows, Some("c"));
        assert_eq!(
            kinds(&report),
            [TurnKind::First, TurnKind::Hit, TurnKind::Hit, TurnKind::Hit]
        );
        assert_eq!(report.keep_warm.pings, 2);
        assert_eq!(report.keep_warm.warm, 2);
        assert_eq!(report.keep_warm.bridged, 1);
        assert_eq!(report.keep_warm.conversations, 1);
        assert_eq!(report.clients.requests, 2);
        assert_eq!(report.pings.requests, 2);
        // The agent paused about eleven minutes, pings or not.
        let bucket = report.gaps.iter().find(|b| b.label == "10-30m").unwrap();
        assert_eq!((bucket.turns, bucket.hits, bucket.with_pings), (1, 1, 1));
        assert_eq!(report.conversations[0].longest_gap_ms, 12 * MIN - MIN / 60);
        let ping_cost = 2.0 * (1.0 + 20_000.0 * 0.05);
        assert!((report.keep_warm.cost - ping_cost).abs() < 1e-6);
    }

    #[test]
    fn a_cold_ping_takes_the_loss_and_the_next_turn_reads_what_it_wrote() {
        let rows = [
            row(T0, "c", 0, 20_000),
            Row {
                write: Some(20_000),
                ..ping(T0 + 7 * MIN, "c", 0)
            },
            row(T0 + 8 * MIN, "c", 20_000, 300),
        ];
        let report = report(&rows, Some("c"));
        assert_eq!(
            kinds(&report),
            [TurnKind::First, TurnKind::Miss, TurnKind::Hit]
        );
        assert_eq!(report.keep_warm.cold, 1);
        assert_eq!(report.losses.ttl_expiry_events, 1);
        // The turn after read its prefix, but only because the cold ping had
        // paid to write it again: the pause was not bridged.
        assert_eq!(report.keep_warm.bridged, 0);
        assert_eq!(report.keep_warm.not_bridged, 1);
    }

    #[test]
    fn one_hour_entries_stretch_the_lifetime_a_gap_is_measured_against() {
        let mut first = row(T0, "c", 0, 20_000);
        first.write_1h = Some(20_000);
        let rows = [first, row(T0 + 20 * MIN, "c", 5_000, 15_000)];
        let report = report(&rows, Some("c"));
        let turns = report.turns.as_ref().unwrap();
        assert_eq!(turns[1].cause, Some(Cause::PrefixChange));
        assert!(report.conversations[0].long_ttl);
        assert_eq!(report.totals.write_1h, 20_000);
    }

    #[test]
    fn a_branch_is_compared_with_nothing_and_leaves_the_main_line_alone() {
        let mut main = row(T0, "c", 0, 30_000);
        main.messages = Some(40);
        let mut side = row(T0 + MIN, "c", 8_000, 1_000);
        side.messages = Some(12);
        let mut next = row(T0 + 2 * MIN, "c", 30_000, 2_000);
        next.messages = Some(42);
        let report = report(&[main, side, next], Some("c"));
        assert_eq!(
            kinds(&report),
            [TurnKind::First, TurnKind::Branch, TurnKind::Hit]
        );
        assert_eq!(report.losses.events, 0);
        assert_eq!(report.conversations[0].branches, 1);
    }

    #[test]
    fn openai_prompts_include_their_cached_part_and_cost_nothing_to_write() {
        let openai = |time_ms, prompt, read| Row {
            api_family: "openai_responses".into(),
            model: Some("gpt-5.5".into()),
            prompt: Some(prompt),
            read: Some(read),
            write: None,
            write_1h: None,
            ..row(time_ms, "o", 0, 0)
        };
        let rows = [openai(T0, 10_000, 0), openai(T0 + MIN, 12_000, 9_984)];
        let report = report(&rows, Some("o"));
        assert_eq!(kinds(&report), [TurnKind::First, TurnKind::Hit]);
        assert_eq!(report.totals.input, 22_000);
        assert_eq!(report.totals.uncached, 12_016);
        assert_eq!(report.totals.read, 9_984);
        assert!((report.totals.cost - (12_016.0 + 998.4)).abs() < 1e-6);
    }

    fn openai_row(time_ms: i64, prompt: i64, read: i64) -> Row {
        Row {
            api_family: "openai_responses".into(),
            model: Some("gpt-5.5".into()),
            prompt: Some(prompt),
            read: Some(read),
            write: None,
            write_1h: None,
            ..row(time_ms, "o", 0, 0)
        }
    }

    #[test]
    fn a_prompt_the_client_compacted_is_not_a_lost_prefix() {
        // Codex compacts: 60K of history becomes a 20K summary, and the
        // provider reads back all it can of the shorter prompt.
        let rows = [
            openai_row(T0, 60_000, 0),
            openai_row(T0 + MIN, 20_000, 19_968),
            openai_row(T0 + 2 * MIN, 24_000, 19_968),
        ];
        let report = report(&rows, Some("o"));
        assert_eq!(
            kinds(&report),
            [TurnKind::First, TurnKind::Hit, TurnKind::Hit]
        );
        assert_eq!(report.turns.as_ref().unwrap()[1].expected, Some(20_000));
        assert_eq!(report.losses.events, 0);

        // The same on Anthropic: the expectation is the prompt it now sends.
        let mut shorter = row(T0 + MIN, "c", 10_000, 0);
        shorter.prompt = Some(500);
        let rows = [row(T0, "c", 0, 30_000), shorter];
        let report = super::tests::report(&rows, Some("c"));
        assert_eq!(kinds(&report), [TurnKind::First, TurnKind::Hit]);
        assert_eq!(report.turns.as_ref().unwrap()[1].expected, Some(10_500));
    }

    #[test]
    fn a_compacted_prompt_still_loses_what_it_carries_and_does_not_read() {
        let rows = [
            openai_row(T0, 60_000, 0),
            openai_row(T0 + MIN, 20_000, 4_096),
        ];
        let report = report(&rows, Some("o"));
        assert_eq!(kinds(&report), [TurnKind::First, TurnKind::Partial]);
        assert_eq!(report.losses.prefix_change_tokens, 15_904);
    }

    #[test]
    fn openai_reads_up_to_one_cache_interval_short_with_nothing_lost() {
        let short = |read| {
            let rows = [
                openai_row(T0, 30_000, 0),
                openai_row(T0 + MIN, 32_000, read),
            ];
            kinds(&report(&rows, Some("o")))[1]
        };
        // gpt-5.5 caches at 2,048-token intervals.
        assert_eq!(short(28_672), TurnKind::Hit);
        assert_eq!(short(27_000), TurnKind::Partial);
        assert_eq!(min_loss_tokens("openai_chat"), 2_048);
        assert_eq!(min_loss_tokens("anthropic_messages"), 1_024);
    }

    #[test]
    fn rows_without_usage_or_a_conversation_count_only_where_they_can() {
        let mut failed = row(T0 + MIN, "c", 0, 0);
        failed.prompt = None;
        failed.read = None;
        failed.write = None;
        failed.status = Some(529);
        let mut loose = row(T0 + 2 * MIN, "", 0, 500);
        loose.conversation = None;
        let rows = [
            row(T0, "c", 0, 20_000),
            failed,
            loose,
            row(T0 + 3 * MIN, "c", 20_000, 0),
        ];
        let report = report(&rows, Some("c"));
        assert_eq!(
            kinds(&report),
            [TurnKind::First, TurnKind::NoUsage, TurnKind::Hit]
        );
        assert_eq!(report.totals.requests, 4);
        assert_eq!(report.totals.with_usage, 3);
        assert_eq!(report.unattributed.requests, 1);
        assert_eq!(report.conversations_total, 1);
    }

    #[test]
    fn conversations_list_the_costliest_losses_first_and_obey_the_limit() {
        let rows = [
            row(T0, "a", 0, 5_000),
            row(T0, "b", 0, 50_000),
            row(T0 + 10 * MIN, "a", 0, 5_000),
            row(T0 + 10 * MIN, "b", 0, 50_000),
            row(T0 + 10 * MIN, "c", 0, 1_000),
        ];
        let full = build("", T0, T0 + 60 * MIN, &rows, false, 50, None);
        let order: Vec<&str> = full
            .conversations
            .iter()
            .map(|c| c.conversation.as_str())
            .collect();
        assert_eq!(order, ["b", "a", "c"]);
        assert!(full.turns.is_none());
        let short = build("", T0, T0 + 60 * MIN, &rows, false, 1, None);
        assert_eq!(short.conversations.len(), 1);
        assert_eq!(short.conversations_total, 3);
    }

    #[test]
    fn rates_follow_the_proxy_price_table() {
        let opus = rates("anthropic_messages", Some("anthropic/claude-opus-5-5"));
        assert_eq!(opus.read, 0.05);
        assert_eq!(
            rates("anthropic_messages", Some("claude-fable-5-1")).read,
            0.025
        );
        assert_eq!(
            rates("anthropic_messages", Some("claude-haiku-5-5")).read,
            0.1
        );
        assert_eq!(rates("anthropic_messages", None).write_1h, 2.0);
        assert_eq!(rates("openai_chat", Some("gpt-5.5")).read, 0.1);
    }
}

#[cfg(test)]
mod db_tests {
    use crate::{Event, ProxyRequestEvent, StatsConfig, StatsReader, spawn};

    const T0: i64 = 1_700_000_000_000;
    const MIN: i64 = 60_000;

    fn event(time_ms: i64, tenant: &str, read: u32, write: u32, keep_warm: bool) -> Event {
        Event::ProxyRequest(ProxyRequestEvent {
            prefix: None,
            context: None,
            timing: None,
            time_ms,
            tenant: tenant.into(),
            request_id: format!("r{time_ms}{keep_warm}"),
            api_family: "anthropic_messages".into(),
            model: Some("claude-sonnet-5-5".into()),
            upstream: "https://api.anthropic.com".into(),
            status: Some(200),
            stream: !keep_warm,
            strip_usage: false,
            prompt_tokens: Some(5),
            completion_tokens: Some(if keep_warm { 1 } else { 40 }),
            cache_read_tokens: Some(read),
            cache_write_tokens: Some(write),
            cache_write_1h_tokens: Some(0),
            shadow_prompt_tokens: None,
            conversation: Some("00000000000000aa".into()),
            retrieve_rounds: 0,
            retrieve_tokens_injected: 0,
            upstream_ms: Some(900.0),
            total_ms: (!keep_warm).then_some(1_000.0),
            error: None,
            keep_warm,
            output_shaping: None,
        })
    }

    #[test]
    fn the_report_reads_the_real_schema_one_tenant_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        let (sink, handle) = spawn(&StatsConfig::at(&path)).unwrap();
        for e in [
            event(T0, "acme", 0, 30_000, false),
            event(T0 + 4 * MIN, "acme", 30_000, 0, true),
            event(T0 + 6 * MIN, "acme", 30_000, 1_000, false),
            event(T0 + 20 * MIN, "acme", 0, 31_000, false),
            event(T0 + MIN, "other", 0, 99_000, false),
        ] {
            sink.send(e);
        }
        sink.flush().unwrap();
        let reader = StatsReader::open(&path).unwrap();
        let report = reader
            .cache("acme", T0, T0 + 60 * MIN, 10, Some("00000000000000aa"))
            .unwrap();
        assert_eq!(report.rows, 4);
        assert!(!report.truncated);
        assert_eq!(report.keep_warm.pings, 1);
        assert_eq!(report.keep_warm.warm, 1);
        assert_eq!(report.losses.ttl_expiry_events, 1);
        assert_eq!(report.losses.ttl_expiry_tokens, 31_000);
        assert_eq!(report.turns.as_ref().unwrap().len(), 4);
        assert_eq!(report.conversations.len(), 1);
        assert_eq!(report.timeline.iter().map(|p| p.pings).sum::<u64>(), 1);
        drop(reader);
        handle.shutdown().unwrap();
    }
}
