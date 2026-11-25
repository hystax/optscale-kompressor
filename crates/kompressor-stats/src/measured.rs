//! Three measurements the headroom plan's later tiers are gated on, each a
//! number and the query that produced it.
//!
//! - **Restarts against open conversations** ([`StatsReader::restarts`]): for
//!   each process start, the conversations with a client turn in the hour
//!   before it and in the hour after, and what the first turn after it read
//!   from the provider's cache against what the last turn before it had
//!   cached. A restart does not empty the provider's cache; it empties the
//!   proxy's memory of each conversation (frozen prefix, keep-warm timers,
//!   recompaction floors), and the next turn can then send different bytes.
//!   This is the cost a persisted prefix tracker would buy back.
//! - **Retrieve-round cost, measured** ([`StatsReader::retrieve_round_cost`]):
//!   what each round the retrieve loop added was billed, from the per-round
//!   usage the proxy files beside the request's sum. The figure it replaces
//!   assumed every extra round read the same prefix as the first; that
//!   assumption is reported beside the measurement (`assumed_extra_cache_read`)
//!   so the two can be compared on the same window.
//! - **Fetch probability** ([`StatsReader::fetch_probability`]): the share of
//!   elided handles the model asked for back, by the kind of content, the
//!   fragment's size and the size of the prompt that carried it.
//!
//! The SQL is public ([`RESTARTS_SQL`], [`RETRIEVE_ROUNDS_SQL`],
//! [`FETCH_PROBABILITY_SQL`]), returned in each report's `query` field and
//! kept in `scripts/db/queries.sql`, where a test holds it to these constants.
//! Each takes the named parameters `:tenant`, `:from_ms`, `:to_ms` (the
//! window, `[from, to)`) and `:experiment_id` (NULL for every experiment;
//! otherwise rows whose `request_context` carries that `stats.experiment_id`).
//!
//! The tables are schema v18's. A database a v18 writer has not opened yet
//! has none of them; the reports then say `available: false` and carry
//! zeros, rather than failing.

use rusqlite::named_params;
use serde::{Deserialize, Serialize};

use crate::StatsReader;
use crate::error::StatsError;

/// The hour either side of a start that a conversation must have a turn in
/// to count as open across it.
pub const RESTART_HORIZON_MS: i64 = 3_600_000;

/// The five-minute cache lifetime. A pair of turns further apart than this
/// may have lost its prefix to expiry whatever the restart did, so the
/// restart report gives the pairs inside it separately.
pub const SHORT_TTL_MS: i64 = 300_000;

/// Restarts against the conversations open across them.
///
/// A conversation is the prefix tracker's fingerprint
/// (`proxy_requests.conversation`), which is stable across restarts. A turn
/// is a client request (`keep_warm = 0`). For each start in the window: the
/// last turn of each conversation in the hour before it, the first in the
/// hour after, and for the conversations with both, the first turn's
/// `cache_read_tokens` against the last one's `cache_read + cache_write`
/// (what the provider then held of the prefix).
pub const RESTARTS_SQL: &str = r#"
WITH starts AS (
  SELECT id, time_ms, version, surfaces FROM process_starts
   WHERE time_ms >= :from_ms AND time_ms < :to_ms),
turns AS (
  SELECT p.id, p.conversation, p.time_ms,
         COALESCE(p.cache_read_tokens, 0) AS cache_read,
         COALESCE(p.cache_write_tokens, 0) AS cache_write
    FROM proxy_requests p
   WHERE p.tenant = :tenant AND p.keep_warm = 0 AND p.conversation IS NOT NULL
     AND p.time_ms >= :from_ms - 3600000 AND p.time_ms < :to_ms + 3600000
     AND (:experiment_id IS NULL OR EXISTS (
          SELECT 1 FROM request_context c
           WHERE c.tenant = p.tenant AND c.request_id = p.request_id
             AND json_extract(c.context, '$.experiment_id') = :experiment_id))),
before AS (
  SELECT s.id AS start_id, t.conversation, t.time_ms, t.cache_read + t.cache_write AS cached,
         ROW_NUMBER() OVER (PARTITION BY s.id, t.conversation
                            ORDER BY t.time_ms DESC, t.id DESC) AS nth
    FROM starts s JOIN turns t
      ON t.time_ms >= s.time_ms - 3600000 AND t.time_ms < s.time_ms),
after AS (
  SELECT s.id AS start_id, t.conversation, t.time_ms, t.cache_read AS read,
         ROW_NUMBER() OVER (PARTITION BY s.id, t.conversation
                            ORDER BY t.time_ms, t.id) AS nth
    FROM starts s JOIN turns t
      ON t.time_ms >= s.time_ms AND t.time_ms < s.time_ms + 3600000),
across AS (
  SELECT b.start_id, b.cached, a.read, a.time_ms - b.time_ms AS gap_ms
    FROM before b JOIN after a
      ON a.start_id = b.start_id AND a.conversation = b.conversation
   WHERE b.nth = 1 AND a.nth = 1)
SELECT s.id, s.time_ms, s.version, s.surfaces,
  (SELECT COUNT(*) FROM before b WHERE b.start_id = s.id AND b.nth = 1) AS conversations_before,
  (SELECT COUNT(*) FROM after a WHERE a.start_id = s.id AND a.nth = 1) AS conversations_after,
  (SELECT COUNT(*) FROM across x WHERE x.start_id = s.id) AS conversations_across,
  (SELECT COALESCE(SUM(x.cached), 0) FROM across x WHERE x.start_id = s.id) AS cached_before,
  (SELECT COALESCE(SUM(x.read), 0) FROM across x WHERE x.start_id = s.id) AS read_after,
  (SELECT COALESCE(SUM(MAX(x.cached - x.read, 0)), 0) FROM across x
    WHERE x.start_id = s.id) AS lost_tokens,
  (SELECT COUNT(*) FROM across x WHERE x.start_id = s.id AND 2 * x.read < x.cached) AS cold_after,
  (SELECT COUNT(*) FROM across x
    WHERE x.start_id = s.id AND x.gap_ms <= 300000) AS across_within_ttl,
  (SELECT COALESCE(SUM(x.cached), 0) FROM across x
    WHERE x.start_id = s.id AND x.gap_ms <= 300000) AS cached_before_within_ttl,
  (SELECT COALESCE(SUM(x.read), 0) FROM across x
    WHERE x.start_id = s.id AND x.gap_ms <= 300000) AS read_after_within_ttl
FROM starts s
ORDER BY s.time_ms, s.id
"#;

/// Retrieve-round cost per `(api_family, model)`.
///
/// `requests` is every client turn in the window; `rounds` is the per-round
/// usage (`retrieve_round_usage`) summed per request, split into round 0 (the
/// client's own call) and the rounds the loop added. `assumed_extra_cache_read`
/// is the figure this replaces: each request's summed `cache_read_tokens`
/// shared evenly across its rounds, the extra rounds' part of it.
pub const RETRIEVE_ROUNDS_SQL: &str = r#"
WITH requests AS (
  SELECT p.tenant, p.request_id, p.api_family, p.model, p.retrieve_rounds,
         COALESCE(p.cache_read_tokens, 0) AS cache_read
    FROM proxy_requests p
   WHERE p.tenant = :tenant AND p.keep_warm = 0
     AND p.time_ms >= :from_ms AND p.time_ms < :to_ms
     AND (:experiment_id IS NULL OR EXISTS (
          SELECT 1 FROM request_context c
           WHERE c.tenant = p.tenant AND c.request_id = p.request_id
             AND json_extract(c.context, '$.experiment_id') = :experiment_id))),
rounds AS (
  SELECT r.tenant, r.request_id,
         SUM(r.round >= 1) AS extra,
         SUM(CASE WHEN r.round >= 1 THEN COALESCE(r.input_tokens, 0) ELSE 0 END) AS input,
         SUM(CASE WHEN r.round >= 1 THEN COALESCE(r.cache_read_tokens, 0) ELSE 0 END) AS cache_read,
         SUM(CASE WHEN r.round >= 1 THEN COALESCE(r.cache_write_tokens, 0) ELSE 0 END) AS cache_write,
         SUM(CASE WHEN r.round >= 1 THEN COALESCE(r.output_tokens, 0) ELSE 0 END) AS output,
         SUM(CASE WHEN r.round = 0 THEN COALESCE(r.cache_read_tokens, 0) ELSE 0 END) AS first_read
    FROM retrieve_round_usage r
   WHERE r.tenant = :tenant AND r.time_ms >= :from_ms AND r.time_ms < :to_ms
   GROUP BY r.tenant, r.request_id)
SELECT q.api_family, COALESCE(q.model, '') AS model,
       COUNT(*) AS client_turns,
       SUM(q.retrieve_rounds > 0) AS requests_with_rounds,
       SUM(q.retrieve_rounds) AS extra_rounds_recorded,
       SUM(CASE WHEN q.retrieve_rounds > 0
                THEN q.cache_read * q.retrieve_rounds / (q.retrieve_rounds + 1.0)
                ELSE 0 END) AS assumed_extra_cache_read,
       SUM(r.extra > 0) AS requests_measured,
       COALESCE(SUM(r.extra), 0) AS extra_rounds_measured,
       COALESCE(SUM(r.input), 0) AS extra_input,
       COALESCE(SUM(r.cache_read), 0) AS extra_cache_read,
       COALESCE(SUM(r.cache_write), 0) AS extra_cache_write,
       COALESCE(SUM(r.output), 0) AS extra_output,
       COALESCE(SUM(CASE WHEN r.extra > 0 THEN r.first_read END), 0) AS first_round_cache_read
  FROM requests q
  LEFT JOIN rounds r ON r.tenant = q.tenant AND r.request_id = q.request_id
 GROUP BY q.api_family, COALESCE(q.model, '')
 ORDER BY q.api_family, model
"#;

/// Fetch probability by kind, fragment size and prompt size.
///
/// An elision is a handle the first time it was shipped (`elisions`), in the
/// window. It was fetched if any retrieval of that handle by the same tenant
/// was served or missed (`withheld = 0`); a handle may be asked for with or
/// without its `k_` prefix. Its prompt size is the input the provider billed
/// the request that first shipped it — fresh, read and written on Anthropic,
/// `prompt_tokens` elsewhere — taken from round 0 where that request ran the
/// retrieve loop, since the row's own figure sums every round. A handle
/// shipped outside the proxy (the API, MCP or CLI surfaces) has no such
/// request and is counted under `unknown`.
pub const FETCH_PROBABILITY_SQL: &str = r#"
WITH shipped AS (
  SELECT e.tenant, e.ref, e.request_id, e.kind, e.bytes
    FROM elisions e
   WHERE e.tenant = :tenant AND e.time_ms >= :from_ms AND e.time_ms < :to_ms
     AND (:experiment_id IS NULL OR EXISTS (
          SELECT 1 FROM request_context c
           WHERE c.tenant = e.tenant AND c.request_id = e.request_id
             AND json_extract(c.context, '$.experiment_id') = :experiment_id))),
carried AS (
  SELECT s.kind, s.bytes,
    (SELECT CASE WHEN p.api_family = 'anthropic_messages'
                 THEN COALESCE(u.input_tokens, p.prompt_tokens)
                      + COALESCE(u.cache_read_tokens, p.cache_read_tokens, 0)
                      + COALESCE(u.cache_write_tokens, p.cache_write_tokens, 0)
                 ELSE COALESCE(u.input_tokens, p.prompt_tokens) END
       FROM proxy_requests p
       LEFT JOIN retrieve_round_usage u
         ON u.tenant = p.tenant AND u.request_id = p.request_id AND u.round = 0
      WHERE p.tenant = s.tenant AND p.request_id = s.request_id AND p.keep_warm = 0
      ORDER BY p.id LIMIT 1) AS prefix_tokens,
    EXISTS (SELECT 1 FROM retrieval_events r
             WHERE r.tenant = s.tenant AND r.ref IN (s.ref, substr(s.ref, 3))
               AND r.withheld = 0) AS fetched
    FROM shipped s)
SELECT kind,
  CASE WHEN bytes < 1024 THEN '<1KiB'
       WHEN bytes < 4096 THEN '1-4KiB'
       WHEN bytes < 16384 THEN '4-16KiB'
       WHEN bytes < 65536 THEN '16-64KiB'
       ELSE '>=64KiB' END AS size_bucket,
  CASE WHEN prefix_tokens IS NULL THEN 'unknown'
       WHEN prefix_tokens < 10000 THEN '<10k'
       WHEN prefix_tokens < 50000 THEN '10-50k'
       WHEN prefix_tokens < 100000 THEN '50-100k'
       WHEN prefix_tokens < 200000 THEN '100-200k'
       ELSE '>=200k' END AS prefix_bucket,
  COUNT(*) AS elisions,
  SUM(fetched) AS fetched
FROM carried
GROUP BY 1, 2, 3
ORDER BY 1, 2, 3
"#;

/// The fragment-size buckets, smallest first.
pub const SIZE_BUCKETS: [&str; 5] = ["<1KiB", "1-4KiB", "4-16KiB", "16-64KiB", ">=64KiB"];
/// The prompt-size buckets, smallest first, then `unknown`.
pub const PREFIX_BUCKETS: [&str; 6] =
    ["<10k", "10-50k", "50-100k", "100-200k", ">=200k", "unknown"];

fn share(numerator: f64, denominator: f64) -> Option<f64> {
    (denominator > 0.0).then(|| numerator / denominator)
}

/// What every report says about what it covers.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Scope {
    pub tenant: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub experiment_id: Option<String>,
    /// Whether the database holds the v18 tables these are read from.
    pub available: bool,
    /// The SQL that produced the rows, with its named parameters.
    pub query: String,
}

// ---------------------------------------------------------------- restarts

/// One process start, set against the conversations open across it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RestartRow {
    pub start_id: i64,
    pub time_ms: i64,
    pub version: String,
    pub surfaces: String,
    /// Conversations with a client turn in the hour before the start.
    pub conversations_before: u64,
    /// Conversations with a client turn in the hour after it.
    pub conversations_after: u64,
    /// Conversations with both: open across the start.
    pub conversations_across: u64,
    /// For those: what the last turn before the start read and wrote, summed.
    pub cached_before: i64,
    /// What the first turn after it read, summed.
    pub read_after: i64,
    /// `read_after / cached_before`.
    pub read_share: Option<f64>,
    /// Per conversation, `cached_before - read_after` where positive.
    pub lost_tokens: i64,
    /// Conversations whose first turn after read less than half of it.
    pub cold_after: u64,
    /// The same pairs, only those whose two turns were at most five minutes
    /// apart: where an expired cache cannot be the explanation.
    pub across_within_ttl: u64,
    pub cached_before_within_ttl: i64,
    pub read_after_within_ttl: i64,
    pub read_share_within_ttl: Option<f64>,
}

/// Every start in the window, and their sum.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RestartReport {
    #[serde(flatten)]
    pub scope: Scope,
    pub starts: Vec<RestartRow>,
    /// The sums over `starts`; `start_id`, `time_ms`, `version` and
    /// `surfaces` are empty.
    pub totals: RestartRow,
}

// ---------------------------------------------------------- retrieve rounds

/// What the rounds the loop added were billed, for one family and model.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RoundCost {
    pub api_family: String,
    pub model: String,
    pub client_turns: u64,
    /// Client turns whose `proxy_requests.retrieve_rounds` is above zero.
    pub requests_with_rounds: u64,
    /// The rounds `proxy_requests` counted, measured or not.
    pub extra_rounds_recorded: u64,
    /// The old estimate: each request's summed cache read, shared evenly
    /// across its rounds, the extra rounds' part.
    pub assumed_extra_cache_read: f64,
    /// Requests with per-round usage on record, and their extra rounds.
    pub requests_measured: u64,
    pub extra_rounds_measured: u64,
    /// `requests_measured / requests_with_rounds`.
    pub coverage: Option<f64>,
    /// The provider's prompt figure on the extra rounds (on OpenAI the cached
    /// part included), what of it was billed fresh, read and written, and
    /// the output.
    pub extra_input: i64,
    pub extra_uncached: i64,
    pub extra_cache_read: i64,
    pub extra_cache_write: i64,
    pub extra_output: i64,
    /// What round 0 of the measured requests read, for comparison.
    pub first_round_cache_read: i64,
    pub cache_read_per_extra_round: Option<f64>,
    /// The extra rounds' input in input-equivalent tokens, output excluded:
    /// fresh at 1, reads and five-minute writes at [`crate::cache::rates`].
    pub extra_round_cost: f64,
    pub cost_per_extra_round: Option<f64>,
    /// Per 1,000 client turns of the window.
    pub extra_rounds_per_1000_turns: Option<f64>,
    pub cache_read_per_1000_turns: Option<f64>,
    pub cost_per_1000_turns: Option<f64>,
}

impl RoundCost {
    fn add(&mut self, other: &RoundCost) {
        self.client_turns += other.client_turns;
        self.requests_with_rounds += other.requests_with_rounds;
        self.extra_rounds_recorded += other.extra_rounds_recorded;
        self.assumed_extra_cache_read += other.assumed_extra_cache_read;
        self.requests_measured += other.requests_measured;
        self.extra_rounds_measured += other.extra_rounds_measured;
        self.extra_input += other.extra_input;
        self.extra_uncached += other.extra_uncached;
        self.extra_cache_read += other.extra_cache_read;
        self.extra_cache_write += other.extra_cache_write;
        self.extra_output += other.extra_output;
        self.first_round_cache_read += other.first_round_cache_read;
        self.extra_round_cost += other.extra_round_cost;
    }

    fn finish(&mut self) {
        let rounds = self.extra_rounds_measured as f64;
        let turns = self.client_turns as f64;
        self.coverage = share(
            self.requests_measured as f64,
            self.requests_with_rounds as f64,
        );
        self.cache_read_per_extra_round = share(self.extra_cache_read as f64, rounds);
        self.cost_per_extra_round = share(self.extra_round_cost, rounds);
        self.extra_rounds_per_1000_turns = share(1000.0 * rounds, turns);
        self.cache_read_per_1000_turns = share(1000.0 * self.extra_cache_read as f64, turns);
        self.cost_per_1000_turns = share(1000.0 * self.extra_round_cost, turns);
    }
}

/// Retrieve-round cost over the window.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RoundCostReport {
    #[serde(flatten)]
    pub scope: Scope,
    /// Every family and model; `api_family` and `model` are empty.
    pub totals: RoundCost,
    pub groups: Vec<RoundCost>,
}

// --------------------------------------------------------- fetch probability

/// Elisions and fetches for one cell or one margin.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FetchCell {
    pub kind: String,
    pub size_bucket: String,
    pub prefix_bucket: String,
    pub elisions: u64,
    pub fetched: u64,
    pub probability: Option<f64>,
}

impl FetchCell {
    fn finish(mut self) -> FetchCell {
        self.probability = share(self.fetched as f64, self.elisions as f64);
        self
    }
}

/// Fetch probability over the window: every cell, and each dimension alone.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FetchReport {
    #[serde(flatten)]
    pub scope: Scope,
    pub elisions: u64,
    pub fetched: u64,
    pub probability: Option<f64>,
    /// One per `(kind, size_bucket, prefix_bucket)` with any elision.
    pub cells: Vec<FetchCell>,
    /// Each with the other two dimensions blank.
    pub by_kind: Vec<FetchCell>,
    pub by_size: Vec<FetchCell>,
    pub by_prefix: Vec<FetchCell>,
}

impl StatsReader {
    fn scope(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
        experiment_id: Option<&str>,
        tables: &[&str],
        query: &str,
    ) -> Result<Scope, StatsError> {
        let mut available = true;
        for table in tables {
            available &= crate::schema::has_table(&self.conn, table)?;
        }
        Ok(Scope {
            tenant: tenant.to_string(),
            from_ms,
            to_ms,
            experiment_id: experiment_id.map(str::to_string),
            available,
            query: query.trim().to_string(),
        })
    }

    /// Process starts in `[from_ms, to_ms)` against `tenant`'s conversations
    /// open across each ([`RESTARTS_SQL`]).
    pub fn restarts(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
        experiment_id: Option<&str>,
    ) -> Result<RestartReport, StatsError> {
        let scope = self.scope(
            tenant,
            from_ms,
            to_ms,
            experiment_id,
            &["process_starts"],
            RESTARTS_SQL,
        )?;
        let mut report = RestartReport {
            scope,
            ..RestartReport::default()
        };
        if !report.scope.available {
            return Ok(report);
        }
        let mut statement = self.conn.prepare(RESTARTS_SQL)?;
        let rows = statement.query_map(
            named_params! {
                ":tenant": tenant,
                ":from_ms": from_ms,
                ":to_ms": to_ms,
                ":experiment_id": experiment_id,
            },
            |row| {
                Ok(RestartRow {
                    start_id: row.get(0)?,
                    time_ms: row.get(1)?,
                    version: row.get(2)?,
                    surfaces: row.get(3)?,
                    conversations_before: row.get::<_, i64>(4)? as u64,
                    conversations_after: row.get::<_, i64>(5)? as u64,
                    conversations_across: row.get::<_, i64>(6)? as u64,
                    cached_before: row.get(7)?,
                    read_after: row.get(8)?,
                    read_share: None,
                    lost_tokens: row.get(9)?,
                    cold_after: row.get::<_, i64>(10)? as u64,
                    across_within_ttl: row.get::<_, i64>(11)? as u64,
                    cached_before_within_ttl: row.get(12)?,
                    read_after_within_ttl: row.get(13)?,
                    read_share_within_ttl: None,
                })
            },
        )?;
        let totals = &mut report.totals;
        for row in rows {
            let mut row = row?;
            row.read_share = share(row.read_after as f64, row.cached_before as f64);
            row.read_share_within_ttl = share(
                row.read_after_within_ttl as f64,
                row.cached_before_within_ttl as f64,
            );
            totals.conversations_before += row.conversations_before;
            totals.conversations_after += row.conversations_after;
            totals.conversations_across += row.conversations_across;
            totals.cached_before += row.cached_before;
            totals.read_after += row.read_after;
            totals.lost_tokens += row.lost_tokens;
            totals.cold_after += row.cold_after;
            totals.across_within_ttl += row.across_within_ttl;
            totals.cached_before_within_ttl += row.cached_before_within_ttl;
            totals.read_after_within_ttl += row.read_after_within_ttl;
            report.starts.push(row);
        }
        let totals = &mut report.totals;
        totals.read_share = share(totals.read_after as f64, totals.cached_before as f64);
        totals.read_share_within_ttl = share(
            totals.read_after_within_ttl as f64,
            totals.cached_before_within_ttl as f64,
        );
        Ok(report)
    }

    /// What the retrieve loop's extra rounds were billed for `tenant`'s
    /// client turns in `[from_ms, to_ms)` ([`RETRIEVE_ROUNDS_SQL`]).
    pub fn retrieve_round_cost(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
        experiment_id: Option<&str>,
    ) -> Result<RoundCostReport, StatsError> {
        let scope = self.scope(
            tenant,
            from_ms,
            to_ms,
            experiment_id,
            &["retrieve_round_usage"],
            RETRIEVE_ROUNDS_SQL,
        )?;
        let mut report = RoundCostReport {
            scope,
            ..RoundCostReport::default()
        };
        if !report.scope.available {
            return Ok(report);
        }
        let mut statement = self.conn.prepare(RETRIEVE_ROUNDS_SQL)?;
        let rows = statement.query_map(
            named_params! {
                ":tenant": tenant,
                ":from_ms": from_ms,
                ":to_ms": to_ms,
                ":experiment_id": experiment_id,
            },
            |row| {
                let count = |i: usize| -> rusqlite::Result<u64> {
                    Ok(row.get::<_, Option<i64>>(i)?.unwrap_or(0).max(0) as u64)
                };
                Ok(RoundCost {
                    api_family: row.get(0)?,
                    model: row.get(1)?,
                    client_turns: count(2)?,
                    requests_with_rounds: count(3)?,
                    extra_rounds_recorded: count(4)?,
                    assumed_extra_cache_read: row.get::<_, Option<f64>>(5)?.unwrap_or(0.0),
                    requests_measured: count(6)?,
                    extra_rounds_measured: count(7)?,
                    extra_input: row.get(8)?,
                    extra_cache_read: row.get(9)?,
                    extra_cache_write: row.get(10)?,
                    extra_output: row.get(11)?,
                    first_round_cache_read: row.get(12)?,
                    ..RoundCost::default()
                })
            },
        )?;
        for row in rows {
            let mut group = row?;
            let model = (!group.model.is_empty()).then_some(group.model.as_str());
            let rates = crate::cache::rates(&group.api_family, model);
            // Anthropic reports the fresh remainder as its input; OpenAI's
            // prompt figure includes the cached part.
            group.extra_uncached = if group.api_family == "anthropic_messages" {
                group.extra_input
            } else {
                (group.extra_input - group.extra_cache_read).max(0)
            };
            group.extra_round_cost = group.extra_uncached as f64
                + group.extra_cache_read as f64 * rates.read
                + group.extra_cache_write as f64 * rates.write_5m;
            group.finish();
            report.totals.add(&group);
            report.groups.push(group);
        }
        report.totals.finish();
        Ok(report)
    }

    /// The share of `tenant`'s handles first shipped in `[from_ms, to_ms)`
    /// that were asked for back ([`FETCH_PROBABILITY_SQL`]).
    pub fn fetch_probability(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
        experiment_id: Option<&str>,
    ) -> Result<FetchReport, StatsError> {
        let scope = self.scope(
            tenant,
            from_ms,
            to_ms,
            experiment_id,
            &["elisions", "retrieve_round_usage"],
            FETCH_PROBABILITY_SQL,
        )?;
        let mut report = FetchReport {
            scope,
            ..FetchReport::default()
        };
        if !report.scope.available {
            return Ok(report);
        }
        let mut statement = self.conn.prepare(FETCH_PROBABILITY_SQL)?;
        let rows = statement.query_map(
            named_params! {
                ":tenant": tenant,
                ":from_ms": from_ms,
                ":to_ms": to_ms,
                ":experiment_id": experiment_id,
            },
            |row| {
                Ok(FetchCell {
                    kind: row.get(0)?,
                    size_bucket: row.get(1)?,
                    prefix_bucket: row.get(2)?,
                    elisions: row.get::<_, i64>(3)?.max(0) as u64,
                    fetched: row.get::<_, Option<i64>>(4)?.unwrap_or(0).max(0) as u64,
                    probability: None,
                })
            },
        )?;
        let mut by_kind: Vec<FetchCell> = Vec::new();
        let mut by_size: Vec<FetchCell> = SIZE_BUCKETS
            .iter()
            .map(|bucket| FetchCell {
                size_bucket: (*bucket).to_string(),
                ..FetchCell::default()
            })
            .collect();
        let mut by_prefix: Vec<FetchCell> = PREFIX_BUCKETS
            .iter()
            .map(|bucket| FetchCell {
                prefix_bucket: (*bucket).to_string(),
                ..FetchCell::default()
            })
            .collect();
        for row in rows {
            let cell = row?;
            report.elisions += cell.elisions;
            report.fetched += cell.fetched;
            let kind = match by_kind.iter_mut().find(|k| k.kind == cell.kind) {
                Some(kind) => kind,
                None => {
                    by_kind.push(FetchCell {
                        kind: cell.kind.clone(),
                        ..FetchCell::default()
                    });
                    by_kind.last_mut().expect("just pushed")
                }
            };
            for margin in [
                Some(kind),
                by_size
                    .iter_mut()
                    .find(|s| s.size_bucket == cell.size_bucket),
                by_prefix
                    .iter_mut()
                    .find(|p| p.prefix_bucket == cell.prefix_bucket),
            ]
            .into_iter()
            .flatten()
            {
                margin.elisions += cell.elisions;
                margin.fetched += cell.fetched;
            }
            report.cells.push(cell.finish());
        }
        report.probability = share(report.fetched as f64, report.elisions as f64);
        report.by_kind = by_kind.into_iter().map(FetchCell::finish).collect();
        report.by_size = by_size.into_iter().map(FetchCell::finish).collect();
        report.by_prefix = by_prefix.into_iter().map(FetchCell::finish).collect();
        Ok(report)
    }
}
