//! Read-only queries over `stats.db`, and the token economy they report.
//!
//! # Gross, net, and why the difference is the whole point
//!
//! Compression credits `tokens_in - tokens_out` on every request. Summed over a
//! window that is `saved_gross`, and it is the number everyone wants to quote.
//!
//! In [`Mode::Recoverable`](kompressor_engine::types::Mode::Recoverable) it is
//! not the number. Every elision is stashed behind a handle, and every handle
//! the model asks for re-injects its fragment into a later prompt, which the
//! provider bills again. So the honest figure is
//!
//! ```text
//! saved_net = saved_gross - Σ returned_tokens over retrievals that found something
//! ```
//!
//! Only found retrievals are a debit: a miss returns a short "no such fragment"
//! and costs nothing worth counting, and charging for it would make an
//! over-aggressive TTL look like an expense instead of a bug.
//!
//! **Net can go negative, and the reader reports it negative.** A retrieved
//! fragment re-enters the prompt, is compressed again on the next turn, and can
//! be retrieved again — around the proxy's agent loop, one request retrieving
//! ~28 KB over ten rounds re-injected roughly 67K tokens against about 7K
//! saved. A UI that shows only gross cannot show that happening, so callers
//! must display net. Gross without net is not a summary of this system, it is
//! an advertisement for it.
//!
//! # The limitation net does not fix
//!
//! A windowed sum of gross **over-counts in agent loops**. Every turn re-sends
//! the whole growing history, the engine compresses all of it again, and the
//! same bytes are credited on every request in the window. This is per-call
//! gross, not unique bytes, and nothing in this schema can deduplicate it: that
//! needs conversation identity, which the surfaces do not carry. The size of
//! the error is known, not theoretical — a measured A/B on this workload put
//! the honest per-turn saving at roughly a tenth of what the equivalent
//! windowed histogram reported.
//!
//! Two smaller biases in the same direction, recorded so nobody rediscovers
//! them as bugs: a compression event exists even when the provider bills that
//! prefix as a cache read at a fraction of the price, so tokens saved are not
//! dollars saved; and the framing tokens a retrieval tool call costs are not
//! measured at all. Where the proxy saw provider usage,
//! [`ProxyRow::prompt_tokens`] is what was actually billed and is the number to
//! check an estimate against.

use crate::error::StatsError;
use crate::events::StoreSample;
use crate::schema;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Handles reported by [`StatsReader::retrievals`].
pub const TOP_REFS_LIMIT: usize = 20;

const MS_PER_HOUR: i64 = 3_600_000;
const MS_PER_DAY: i64 = 86_400_000;

/// Time granularity for [`StatsReader::histogram`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Bucket {
    /// One row per UTC hour.
    Hour,
    /// One row per UTC day.
    Day,
}

impl Bucket {
    /// Bucket width in milliseconds.
    pub fn width_ms(self) -> i64 {
        match self {
            Bucket::Hour => MS_PER_HOUR,
            Bucket::Day => MS_PER_DAY,
        }
    }

    /// SQL flooring `time_ms` to the start of its bucket.
    ///
    /// Written with the modulo rather than `(t / w) * w` because SQLite's
    /// integer division truncates toward zero, which for a timestamp before
    /// 1970 rounds *up* and files the event in the following bucket. Clocks do
    /// go backwards — a container starting before NTP has run stamps events in
    /// 1970 or earlier — and a row landing in the wrong bucket is exactly the
    /// class of error this crate exists to have stopped making.
    fn floor_expr(self, column: &str) -> String {
        let w = self.width_ms();
        format!("({column} - (({column} % {w}) + {w}) % {w})")
    }

    /// SQL rendering a floored `bucket` column as a human label.
    fn label_expr(self) -> &'static str {
        match self {
            Bucket::Hour => "strftime('%Y-%m-%dT%H:00:00Z', bucket/1000, 'unixepoch')",
            Bucket::Day => "date(bucket/1000, 'unixepoch')",
        }
    }
}

/// Totals for one compression mode.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModeTotals {
    /// `aggressive`, `lossless` or `recoverable`.
    pub mode: String,
    /// Compressions recorded in the window.
    pub requests: u64,
    /// Estimated tokens before compression.
    pub tokens_in: i64,
    /// Estimated tokens after compression.
    pub tokens_out: i64,
    /// `tokens_in - tokens_out`.
    pub saved_gross: i64,
    /// The debit charged to this mode: tokens re-injected by retrievals that
    /// happened during a request this mode compressed.
    ///
    /// Only `recoverable` mints handles, so in practice every other mode reads
    /// zero here and its [`saved_net`](ModeTotals::saved_net) equals its gross.
    /// A retrieval on a request that was never compressed — `kompressor
    /// retrieve` from a terminal, say — joins to no mode and is in none of
    /// these rows, so the column can sum to less than
    /// [`Summary::retrieved_tokens`].
    pub retrieved_tokens: i64,
}

impl ModeTotals {
    /// `saved_gross - retrieved_tokens`; may be negative.
    pub fn saved_net(&self) -> i64 {
        self.saved_gross - self.retrieved_tokens
    }
}

/// Totals for one surface.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SurfaceTotals {
    /// `api`, `mcp`, `proxy` or `cli`.
    pub surface: String,
    /// Compressions recorded in the window.
    pub requests: u64,
    /// Estimated tokens before compression.
    pub tokens_in: i64,
    /// Estimated tokens after compression.
    pub tokens_out: i64,
    /// `tokens_in - tokens_out`.
    pub saved_gross: i64,
}

/// What a window of traffic did, credits and debits together.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Summary {
    /// Tenant the window was scoped to.
    pub tenant: String,
    /// Start of the window, inclusive.
    pub from_ms: i64,
    /// End of the window, exclusive.
    pub to_ms: i64,
    /// Compressions recorded.
    pub requests: u64,
    /// Messages seen across them.
    pub messages_total: i64,
    /// Messages the engine rewrote.
    pub messages_compressed: i64,
    /// Estimated tokens before compression.
    pub tokens_in: i64,
    /// Estimated tokens after compression.
    pub tokens_out: i64,
    /// The credit: `Σ(tokens_in - tokens_out)`. Over-counts in agent loops —
    /// see the module doc — because re-sent history is credited every turn.
    pub saved_gross: i64,
    /// The debit: tokens re-injected by retrievals that found their fragment.
    pub retrieved_tokens: i64,
    /// `saved_gross - retrieved_tokens`. **May be negative**, and a caller that
    /// shows only `saved_gross` is reporting a number this crate knows to be
    /// wrong.
    pub saved_net: i64,
    /// Retrieval attempts in the window.
    pub retrievals: u64,
    /// Attempts that found their fragment.
    pub retrieval_hits: u64,
    /// Hits over attempts; zero when there were no attempts.
    pub hit_rate: f64,
    /// Split by compression mode.
    pub by_mode: Vec<ModeTotals>,
    /// Split by surface.
    pub by_surface: Vec<SurfaceTotals>,
}

/// One time bucket of the histogram.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BucketRow {
    /// Start of the bucket, milliseconds since the Unix epoch.
    pub bucket_ms: i64,
    /// The same instant as a UTC label.
    pub label: String,
    /// Compressions in the bucket.
    pub requests: u64,
    /// Estimated tokens before compression.
    pub tokens_in: i64,
    /// Estimated tokens after compression.
    pub tokens_out: i64,
    /// Credit in this bucket.
    pub saved_gross: i64,
    /// Debit in this bucket, bucketed by when the retrieval happened — which
    /// is not necessarily when the fragment was created.
    pub retrieved_tokens: i64,
    /// `saved_gross - retrieved_tokens` for this bucket; may be negative.
    pub saved_net: i64,
}

/// Coverage of one detected content type.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TypeCoverage {
    /// The engine's own type name (`json`, `log`, `source_code`, …), with mixed
    /// bodies already unfolded into their constituents.
    pub content_type: String,
    /// Messages compressed as that type.
    pub messages: i64,
    /// Share of all typed messages in the window, 0.0 to 1.0.
    pub share: f64,
}

/// What one upstream mount carried.
///
/// "Provider" is the mount's own name — the string the operator wrote in their
/// configuration — rather than a host, because one mount can send to more than
/// one host: the same `openai` mount goes to `api.openai.com` on an API key and
/// to `chatgpt.com` under a subscription. `hosts` says which it actually used,
/// so a report can show the split without the grouping key moving under it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderCoverage {
    /// The upstream mount's name, or `""` for traffic that never named one.
    pub provider: String,
    /// The origins this mount actually sent to, most-used first.
    pub hosts: Vec<String>,
    pub requests: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    /// Tokens removed, gross.
    pub saved: i64,
    /// Share of tokens removed, 0.0 to 1.0.
    pub ratio: f64,
}

/// Everything recorded about the asks for one handle.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RefHistory {
    /// Times it was asked for and the store was read.
    pub retrievals: u64,
    /// Times it was there.
    pub hits: u64,
    /// Times the budget refused it without reading the store.
    pub withheld: u64,
    /// Tokens it re-injected in total.
    pub returned_tokens: i64,
    pub first_ms: Option<i64>,
    pub last_ms: Option<i64>,
    /// The latest asks, newest first, withheld ones included.
    pub recent: Vec<RefAsk>,
}

/// One ask for a handle.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RefAsk {
    pub time_ms: i64,
    pub request_id: Option<String>,
    pub surface: String,
    pub found: bool,
    pub expired: bool,
    pub ranged: bool,
    pub withheld: bool,
    pub returned_tokens: i64,
}

/// One frequently-asked handle.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RefCount {
    /// The handle, `k_` prefix included.
    pub reference: String,
    /// Times it was asked for.
    pub retrievals: u64,
    /// Times it was there.
    pub hits: u64,
    /// Tokens it re-injected in total.
    pub returned_tokens: i64,
    /// Times the budget refused it without reading the store. Not counted in
    /// `retrievals`; a handle that appears here and nowhere else is one the
    /// model wanted and never got.
    pub withheld: u64,
}

/// The retrieval side on its own.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetrievalSummary {
    /// Attempts in the window.
    pub retrievals: u64,
    /// Attempts that found their fragment.
    pub hits: u64,
    /// Attempts that did not.
    pub misses: u64,
    /// Misses that were expiries — a fragment held and let go — as opposed to a
    /// handle never stored. A rising expiry count is a TTL problem; a rising
    /// miss count with no expiries is a client inventing handles.
    pub expiries: u64,
    /// Attempts asking for a byte range instead of the whole fragment.
    pub ranged: u64,
    /// Ranged over attempts; zero when there were none.
    pub ranged_share: f64,
    /// Hits over attempts; zero when there were none.
    pub hit_rate: f64,
    /// Tokens re-injected by hits: the debit in [`Summary::saved_net`].
    pub retrieved_tokens: i64,
    /// Asks the per-request token budget refused before the store was read.
    ///
    /// Outside every field above, deliberately. These never reached the store,
    /// so counting them as attempts would divide hits by a denominator that
    /// includes asks the store was never given, and counting them as misses
    /// would blame it for a budget the operator set. A rising figure here with
    /// a healthy `hit_rate` means `proxy.retrieve_max_tokens_per_request` is
    /// the binding constraint, which is not visible anywhere else.
    pub withheld: u64,
    /// Busiest handles, at most [`TOP_REFS_LIMIT`].
    pub top_refs: Vec<RefCount>,
}

/// The proxy's own view of a request, with the provider's billed numbers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProxyRow {
    /// Wire dialect the client spoke.
    pub api_family: String,
    /// Model requested.
    pub model: Option<String>,
    /// Upstream the request went to.
    pub upstream: String,
    /// HTTP status returned to the client.
    pub status: Option<i64>,
    /// Whether the response was streamed.
    pub stream: bool,
    /// Prompt tokens the provider billed. This is measured, unlike
    /// [`RequestEconomy::saved_gross`], which is estimated.
    pub prompt_tokens: Option<i64>,
    /// Completion tokens the provider billed.
    pub completion_tokens: Option<i64>,
    /// Prompt tokens served from the provider's cache, at a fraction of the
    /// price — the share of a saving that was never going to cost full rate.
    pub cache_read_tokens: Option<i64>,
    /// Prompt tokens written into the provider's cache.
    pub cache_write_tokens: Option<i64>,
    /// What the prompt would have billed uncompressed, when measured.
    pub shadow_prompt_tokens: Option<i64>,
    /// Retrieval rounds inside this request.
    pub retrieve_rounds: i64,
    /// Tokens re-injected across those rounds.
    pub retrieve_tokens_injected: i64,
    /// Time waiting on the upstream, in milliseconds.
    pub upstream_ms: Option<f64>,
    /// Total time in the proxy, in milliseconds.
    pub total_ms: Option<f64>,
    /// Error text, if it failed.
    pub error: Option<String>,
}

/// One request's economy: compression, retrievals and proxy row joined on
/// `request_id`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestEconomy {
    /// Tenant.
    pub tenant: String,
    /// The request.
    pub request_id: String,
    /// Earliest event time for the request.
    pub time_ms: i64,
    /// Mode of its first compression, absent if it was never compressed.
    pub mode: Option<String>,
    /// Model, as the compression recorded it.
    pub model: Option<String>,
    /// Surface that compressed it.
    pub surface: Option<String>,
    /// Compression events under this id. More than one is normal on the proxy:
    /// an agent loop re-compresses the conversation on every round.
    pub compressions: u64,
    /// Estimated tokens before compression, summed.
    pub tokens_in: i64,
    /// Estimated tokens after compression, summed.
    pub tokens_out: i64,
    /// Credit for this request.
    pub saved_gross: i64,
    /// Debit for this request.
    pub retrieved_tokens: i64,
    /// `saved_gross - retrieved_tokens`; may be negative.
    pub saved_net: i64,
    /// Retrieval attempts stamped with this id.
    pub retrievals: u64,
    /// Attempts that found their fragment.
    pub retrieval_hits: u64,
    /// The proxy's row, when the request went through the proxy.
    pub proxy: Option<ProxyRow>,
}

/// One row of `daily_rollups`: an aggregate that outlives pruning.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DailyRollup {
    /// `YYYY-MM-DD`, UTC.
    pub day: String,
    /// Tenant.
    pub tenant: String,
    /// Compression mode, or `unknown` for retrievals whose request could not be
    /// located.
    pub mode: String,
    /// Surface.
    pub surface: String,
    /// Compressions that day.
    pub requests: i64,
    /// Estimated tokens before compression.
    pub tokens_in: i64,
    /// Estimated tokens after compression.
    pub tokens_out: i64,
    /// Retrieval attempts.
    pub retrievals: i64,
    /// Attempts that found their fragment.
    pub retrieval_hits: i64,
    /// Tokens re-injected by those hits.
    pub retrieved_tokens: i64,
}

impl DailyRollup {
    /// `tokens_in - tokens_out` for the day.
    pub fn saved_gross(&self) -> i64 {
        self.tokens_in - self.tokens_out
    }

    /// Gross minus what retrieval gave back; may be negative.
    pub fn saved_net(&self) -> i64 {
        self.saved_gross() - self.retrieved_tokens
    }
}

/// Read-only access to `stats.db`.
pub struct StatsReader {
    pub(crate) conn: Connection,
    /// Whether `retrieval_events` has the `withheld` column, which schema v10
    /// adds.
    ///
    /// Probed because the reader opens read-only and therefore never migrates:
    /// the writer upgrades the database when the server starts, so a `stats`
    /// command run against a database no current writer has opened yet sees
    /// the old shape. Selecting the column unconditionally turned that into a
    /// hard `no such column` failure for every retrieval query — measured on a
    /// v9 database, which is what every installation has before it restarts.
    ///
    /// A database without the column recorded no refusals, so reporting zero
    /// is not a degraded answer, it is the right one.
    has_withheld: bool,
}

/// What one wrapped session did, for the line `kompressor wrap` prints as it
/// exits.
///
/// The three numbers answer three different questions and the first is the one
/// that matters: a session can be perfectly healthy and compress nothing (a
/// client that pins its whole prompt), but a session where *no request
/// arrived at all* means the agent never routed through the proxy — which is
/// the failure this exists to surface, because every other symptom of it looks
/// like success.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionOutcome {
    /// Requests the proxy relayed, across every tenant.
    pub proxy_requests: u64,
    /// Messages the engine rewrote.
    pub messages_compressed: i64,
    /// Estimated prompt tokens removed.
    pub saved_gross: i64,
    /// Tokens re-injected by retrievals, which `saved_gross` does not subtract.
    pub retrieved_tokens: i64,
}

impl SessionOutcome {
    /// `saved_gross` less what retrieval put back. May be negative.
    pub fn saved_net(&self) -> i64 {
        self.saved_gross - self.retrieved_tokens
    }
}

impl StatsReader {
    /// Open `path` read-only. The file must already exist — the reader does not
    /// create or migrate anything, so a typo in a path reads as an error rather
    /// than as an empty report.
    pub fn open(path: impl AsRef<Path>) -> Result<StatsReader, StatsError> {
        let conn = schema::open_read_only(path.as_ref())?;
        let has_withheld = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('retrieval_events') WHERE name = 'withheld'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        Ok(StatsReader { conn, has_withheld })
    }

    /// `withheld = 0` where the column exists, and the constant true where it
    /// does not — so a query reads "the asks that reached the store" on either
    /// shape. See [`StatsReader::has_withheld`].
    fn served(&self) -> &'static str {
        if self.has_withheld {
            "withheld = 0"
        } else {
            "1"
        }
    }

    /// `SUM(withheld)`, or a literal zero on a database that predates it.
    fn withheld_sum(&self) -> &'static str {
        if self.has_withheld {
            "COALESCE(SUM(withheld),0)"
        } else {
            "0"
        }
    }

    /// Everything one wrapped session needs to say what it did, from
    /// `from_ms` to now.
    ///
    /// Deliberately not scoped to a tenant. A wrap session does not know what
    /// tenant its agent will resolve to — that depends on headers the agent
    /// sends — and reporting a false zero because the rows landed under a
    /// different tenant would be worse than counting a co-tenant's traffic. If
    /// two wrap sessions share one proxy this over-counts; over-counting is the
    /// safe direction, because the alarm is on zero.
    pub fn session_since(&self, from_ms: i64) -> Result<SessionOutcome, StatsError> {
        let proxy_requests: i64 = self.conn.query_row(
            "SELECT count(*) FROM proxy_requests WHERE time_ms >= ?1",
            [from_ms],
            |row| row.get(0),
        )?;
        let (messages_compressed, saved_gross): (i64, i64) = self.conn.query_row(
            "SELECT coalesce(sum(messages_compressed), 0),
                    coalesce(sum(tokens_in - tokens_out), 0)
             FROM optimization_events WHERE time_ms >= ?1",
            [from_ms],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let retrieved_tokens: i64 = self.conn.query_row(
            "SELECT coalesce(sum(returned_tokens), 0)
             FROM retrieval_events WHERE time_ms >= ?1 AND found = 1",
            [from_ms],
            |row| row.get(0),
        )?;
        Ok(SessionOutcome {
            proxy_requests: proxy_requests.max(0) as u64,
            messages_compressed,
            saved_gross,
            retrieved_tokens,
        })
    }

    /// Credits, debits and the split by mode and surface for
    /// `[from_ms, to_ms)`.
    pub fn summary(&self, tenant: &str, from_ms: i64, to_ms: i64) -> Result<Summary, StatsError> {
        let (requests, tokens_in, tokens_out, messages_total, messages_compressed) =
            self.conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(tokens_in),0), COALESCE(SUM(tokens_out),0),
                        COALESCE(SUM(messages_total),0), COALESCE(SUM(messages_compressed),0)
                 FROM optimization_events
                 WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3",
                (tenant, from_ms, to_ms),
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )?;

        let (retrievals, hits, retrieved_tokens) = self.retrieval_totals(
            "tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3",
            (tenant, from_ms, to_ms),
        )?;

        // The debit, split by the mode of the request the retrieval happened
        // in. A scalar subquery rather than a join: a join would multiply a
        // retrieval by however many optimization rows shared its request id,
        // and nothing in the schema forbids two. This way each retrieval is
        // counted once no matter what, and an unattributable one groups under
        // NULL, which is dropped here and shows up as the difference between
        // this column and the window total.
        let mut debit_by_mode: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        {
            let mut stmt = self.conn.prepare_cached(
                "SELECT (SELECT o.mode FROM optimization_events o
                          WHERE o.tenant = r.tenant AND o.request_id = r.request_id
                          LIMIT 1) AS mode,
                        COALESCE(SUM(r.returned_tokens), 0)
                 FROM retrieval_events r
                 WHERE r.tenant = ?1 AND r.time_ms >= ?2 AND r.time_ms < ?3
                   AND r.found = 1
                 GROUP BY mode",
            )?;
            let rows = stmt.query_map((tenant, from_ms, to_ms), |row| {
                Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (mode, tokens) = row?;
                if let Some(mode) = mode {
                    *debit_by_mode.entry(mode).or_insert(0) += tokens;
                }
            }
        }

        let mut by_mode = Vec::new();
        {
            let mut stmt = self.conn.prepare_cached(
                "SELECT mode, COUNT(*), COALESCE(SUM(tokens_in),0), COALESCE(SUM(tokens_out),0)
                 FROM optimization_events
                 WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3
                 GROUP BY mode ORDER BY mode",
            )?;
            let rows = stmt.query_map((tenant, from_ms, to_ms), |row| {
                let tokens_in: i64 = row.get(2)?;
                let tokens_out: i64 = row.get(3)?;
                let mode: String = row.get(0)?;
                Ok(ModeTotals {
                    requests: row.get::<_, i64>(1)? as u64,
                    tokens_in,
                    tokens_out,
                    saved_gross: tokens_in - tokens_out,
                    retrieved_tokens: 0,
                    mode,
                })
            })?;
            for row in rows {
                let mut row = row?;
                row.retrieved_tokens = debit_by_mode.get(&row.mode).copied().unwrap_or(0);
                by_mode.push(row);
            }
        }

        let mut by_surface = Vec::new();
        {
            let mut stmt = self.conn.prepare_cached(
                "SELECT surface, COUNT(*), COALESCE(SUM(tokens_in),0), COALESCE(SUM(tokens_out),0)
                 FROM optimization_events
                 WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3
                 GROUP BY surface ORDER BY surface",
            )?;
            let rows = stmt.query_map((tenant, from_ms, to_ms), |row| {
                let tokens_in: i64 = row.get(2)?;
                let tokens_out: i64 = row.get(3)?;
                Ok(SurfaceTotals {
                    surface: row.get(0)?,
                    requests: row.get::<_, i64>(1)? as u64,
                    tokens_in,
                    tokens_out,
                    saved_gross: tokens_in - tokens_out,
                })
            })?;
            for row in rows {
                by_surface.push(row?);
            }
        }

        let saved_gross = tokens_in - tokens_out;
        Ok(Summary {
            tenant: tenant.to_string(),
            from_ms,
            to_ms,
            requests: requests as u64,
            messages_total,
            messages_compressed,
            tokens_in,
            tokens_out,
            saved_gross,
            retrieved_tokens,
            saved_net: saved_gross - retrieved_tokens,
            retrievals: retrievals as u64,
            retrieval_hits: hits as u64,
            hit_rate: ratio(hits, retrievals),
            by_mode,
            by_surface,
        })
    }

    /// The same credits and debits, bucketed by hour or by day.
    ///
    /// Compressions and retrievals are bucketed by their own `time_ms` and
    /// summed in one pass over the union, so the columns of this report add up
    /// to [`StatsReader::summary`] over the same window exactly. Attributing a
    /// retrieval back to the bucket of the compression that created the
    /// fragment would read better and would no longer sum to anything.
    pub fn histogram(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
        bucket: Bucket,
    ) -> Result<Vec<BucketRow>, StatsError> {
        let floor = bucket.floor_expr("time_ms");
        let sql = format!(
            "SELECT bucket, {label},
                    SUM(requests), SUM(tokens_in), SUM(tokens_out), SUM(retrieved)
             FROM (
               SELECT {floor} AS bucket, 1 AS requests, tokens_in AS tokens_in,
                      tokens_out AS tokens_out, 0 AS retrieved
                 FROM optimization_events
                WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3
               UNION ALL
               SELECT {floor} AS bucket, 0, 0, 0, returned_tokens
                 FROM retrieval_events
                WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3 AND found = 1
             )
             GROUP BY bucket ORDER BY bucket",
            label = bucket.label_expr(),
        );

        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map((tenant, from_ms, to_ms), |row| {
            let tokens_in: i64 = row.get(3)?;
            let tokens_out: i64 = row.get(4)?;
            let retrieved_tokens: i64 = row.get(5)?;
            let saved_gross = tokens_in - tokens_out;
            Ok(BucketRow {
                bucket_ms: row.get(0)?,
                label: row.get(1)?,
                requests: row.get::<_, i64>(2)? as u64,
                tokens_in,
                tokens_out,
                saved_gross,
                retrieved_tokens,
                saved_net: saved_gross - retrieved_tokens,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Messages per detected content type, busiest first.
    ///
    /// `content_types` is a JSON object per event, so this unfolds it with
    /// `json_each` rather than storing one row per type: the map belongs to the
    /// request, and splitting it at write time would have made every other
    /// query count requests more than once.
    /// What each upstream mount carried, busiest first.
    ///
    /// Joined to `proxy_requests` for the hosts rather than read from
    /// `optimization_events` alone: the event knows the mount it was compressed
    /// for, the proxy row knows where the bytes went, and under a subscription
    /// those differ.
    pub fn by_provider(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<ProviderCoverage>, StatsError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT COALESCE(o.provider, '') AS provider,
                    COUNT(*) AS requests,
                    SUM(o.tokens_in) AS tokens_in,
                    SUM(o.tokens_out) AS tokens_out,
                    COALESCE(GROUP_CONCAT(DISTINCT p.upstream), '') AS hosts
             FROM optimization_events o
             LEFT JOIN proxy_requests p ON p.request_id = o.request_id
             WHERE o.tenant = ?1 AND o.time_ms >= ?2 AND o.time_ms < ?3
             GROUP BY provider
             ORDER BY tokens_in DESC, provider",
        )?;
        let rows = stmt.query_map((tenant, from_ms, to_ms), |row| {
            let tokens_in: i64 = row.get(2)?;
            let tokens_out: i64 = row.get(3)?;
            let hosts: String = row.get(4)?;
            Ok(ProviderCoverage {
                provider: row.get(0)?,
                requests: row.get(1)?,
                tokens_in,
                tokens_out,
                saved: tokens_in - tokens_out,
                ratio: if tokens_in > 0 {
                    (tokens_in - tokens_out) as f64 / tokens_in as f64
                } else {
                    0.0
                },
                hosts: hosts
                    .split(',')
                    .filter(|h| !h.is_empty())
                    .map(str::to_string)
                    .collect(),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn by_type(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<TypeCoverage>, StatsError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT j.key AS content_type, SUM(j.value) AS messages
             FROM optimization_events o, json_each(o.content_types) AS j
             WHERE o.tenant = ?1 AND o.time_ms >= ?2 AND o.time_ms < ?3
             GROUP BY j.key
             ORDER BY messages DESC, content_type",
        )?;
        let rows = stmt.query_map((tenant, from_ms, to_ms), |row| {
            Ok(TypeCoverage {
                content_type: row.get(0)?,
                messages: row.get(1)?,
                share: 0.0,
            })
        })?;
        let mut out: Vec<TypeCoverage> = Vec::new();
        for row in rows {
            out.push(row?);
        }
        let total: i64 = out.iter().map(|t| t.messages).sum();
        for entry in &mut out {
            entry.share = ratio(entry.messages, total);
        }
        Ok(out)
    }

    /// Hit rate, expiries, ranged share and the busiest handles.
    pub fn retrievals(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<RetrievalSummary, StatsError> {
        // `withheld = 0` on every aggregate but the last. A refusal is an ask
        // the store never saw, so it belongs in neither the numerator nor the
        // denominator of a hit rate — see `RetrievalSummary::withheld`.
        let (served, withheld_sum) = (self.served(), self.withheld_sum());
        let (retrievals, hits, expiries, ranged, retrieved_tokens, withheld) =
            self.conn.query_row(
                &format!(
                    "SELECT COALESCE(SUM(CASE WHEN {served} THEN 1 ELSE 0 END),0),
                        COALESCE(SUM(CASE WHEN {served} THEN found ELSE 0 END),0),
                        COALESCE(SUM(CASE WHEN {served} THEN expired ELSE 0 END),0),
                        COALESCE(SUM(CASE WHEN {served} THEN ranged ELSE 0 END),0),
                        COALESCE(SUM(CASE WHEN {served} AND found = 1
                                          THEN returned_tokens ELSE 0 END),0),
                        {withheld_sum}
                 FROM retrieval_events
                 WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3"
                ),
                (tenant, from_ms, to_ms),
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )?;

        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT \"ref\", COALESCE(SUM(CASE WHEN {served} THEN 1 ELSE 0 END),0) AS n,
                    COALESCE(SUM(CASE WHEN {served} THEN found ELSE 0 END),0),
                    COALESCE(SUM(CASE WHEN {served} AND found = 1
                                      THEN returned_tokens ELSE 0 END),0),
                    {withheld_sum}
             FROM retrieval_events
             WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3
             GROUP BY \"ref\"
             ORDER BY COUNT(*) DESC, \"ref\"
             LIMIT ?4"
        ))?;
        let rows = stmt.query_map((tenant, from_ms, to_ms, TOP_REFS_LIMIT as i64), |row| {
            Ok(RefCount {
                reference: row.get(0)?,
                retrievals: row.get::<_, i64>(1)? as u64,
                hits: row.get::<_, i64>(2)? as u64,
                returned_tokens: row.get(3)?,
                withheld: row.get::<_, i64>(4)? as u64,
            })
        })?;
        let mut top_refs = Vec::new();
        for row in rows {
            top_refs.push(row?);
        }

        Ok(RetrievalSummary {
            retrievals: retrievals as u64,
            hits: hits as u64,
            misses: (retrievals - hits) as u64,
            withheld: withheld as u64,
            expiries: expiries as u64,
            ranged: ranged as u64,
            ranged_share: ratio(ranged, retrievals),
            hit_rate: ratio(hits, retrievals),
            retrieved_tokens,
            top_refs,
        })
    }

    /// Every recorded ask for one handle, `recent` of them listed.
    ///
    /// `handle` is the bare handle body. The `ref` column holds whatever the
    /// caller asked with, so the three spellings a model uses — bare, `k_`
    /// and the marker's `ref:k_` — are all matched.
    pub fn ref_history(
        &self,
        tenant: &str,
        handle: &str,
        recent: usize,
    ) -> Result<RefHistory, StatsError> {
        let (served, withheld_sum) = (self.served(), self.withheld_sum());
        let prefixed = format!("k_{handle}");
        let marker = format!("ref:k_{handle}");
        let spellings = (tenant, handle, prefixed.as_str(), marker.as_str());
        let mut history = self.conn.query_row(
            &format!(
                "SELECT COALESCE(SUM(CASE WHEN {served} THEN 1 ELSE 0 END),0),
                        COALESCE(SUM(CASE WHEN {served} THEN found ELSE 0 END),0),
                        {withheld_sum},
                        COALESCE(SUM(CASE WHEN {served} AND found = 1
                                          THEN returned_tokens ELSE 0 END),0),
                        MIN(time_ms), MAX(time_ms)
                 FROM retrieval_events
                 WHERE tenant = ?1 AND \"ref\" IN (?2, ?3, ?4)"
            ),
            spellings,
            |row| {
                Ok(RefHistory {
                    retrievals: row.get::<_, i64>(0)?.max(0) as u64,
                    hits: row.get::<_, i64>(1)?.max(0) as u64,
                    withheld: row.get::<_, i64>(2)?.max(0) as u64,
                    returned_tokens: row.get(3)?,
                    first_ms: row.get(4)?,
                    last_ms: row.get(5)?,
                    recent: Vec::new(),
                })
            },
        )?;

        let withheld = if self.has_withheld { "withheld" } else { "0" };
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT time_ms, request_id, surface, found, expired, ranged, {withheld},
                    returned_tokens
             FROM retrieval_events
             WHERE tenant = ?1 AND \"ref\" IN (?2, ?3, ?4)
             ORDER BY time_ms DESC, id DESC
             LIMIT ?5"
        ))?;
        let rows = stmt.query_map(
            (
                tenant,
                handle,
                prefixed.as_str(),
                marker.as_str(),
                recent as i64,
            ),
            |row| {
                Ok(RefAsk {
                    time_ms: row.get(0)?,
                    request_id: row.get(1)?,
                    surface: row.get(2)?,
                    found: row.get::<_, i64>(3)? != 0,
                    expired: row.get::<_, i64>(4)? != 0,
                    ranged: row.get::<_, i64>(5)? != 0,
                    withheld: row.get::<_, i64>(6)? != 0,
                    returned_tokens: row.get(7)?,
                })
            },
        )?;
        for row in rows {
            history.recent.push(row?);
        }
        Ok(history)
    }

    /// One request, joined across compression, retrieval and proxy rows.
    ///
    /// `None` only when the id is unknown to all three tables. A request that
    /// was proxied but never compressed still has an economy — a zero one — and
    /// hiding it would make "no savings" indistinguishable from "no such
    /// request".
    pub fn request(
        &self,
        tenant: &str,
        request_id: &str,
    ) -> Result<Option<RequestEconomy>, StatsError> {
        let (compressions, tokens_in, tokens_out, first_opt_time) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(tokens_in),0), COALESCE(SUM(tokens_out),0), MIN(time_ms)
             FROM optimization_events WHERE tenant = ?1 AND request_id = ?2",
            (tenant, request_id),
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        )?;

        let head = self
            .conn
            .query_row(
                "SELECT mode, model, surface FROM optimization_events
                 WHERE tenant = ?1 AND request_id = ?2
                 ORDER BY time_ms, id LIMIT 1",
                (tenant, request_id),
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;

        let (retrievals, hits, retrieved_tokens) = self.retrieval_totals(
            "tenant = ?1 AND request_id = ?2",
            rusqlite::params![tenant, request_id],
        )?;

        let proxy = self
            .conn
            .query_row(
                "SELECT time_ms, api_family, model, upstream, status, stream,
                        prompt_tokens, completion_tokens, cache_read_tokens, cache_write_tokens,
                        shadow_prompt_tokens, retrieve_rounds, retrieve_tokens_injected,
                        upstream_ms, total_ms, error
                 FROM proxy_requests WHERE tenant = ?1 AND request_id = ?2
                 ORDER BY time_ms, id LIMIT 1",
                (tenant, request_id),
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        ProxyRow {
                            api_family: row.get(1)?,
                            model: row.get(2)?,
                            upstream: row.get(3)?,
                            status: row.get(4)?,
                            stream: row.get::<_, i64>(5)? != 0,
                            prompt_tokens: row.get(6)?,
                            completion_tokens: row.get(7)?,
                            cache_read_tokens: row.get(8)?,
                            cache_write_tokens: row.get(9)?,
                            shadow_prompt_tokens: row.get(10)?,
                            retrieve_rounds: row.get(11)?,
                            retrieve_tokens_injected: row.get(12)?,
                            upstream_ms: row.get(13)?,
                            total_ms: row.get(14)?,
                            error: row.get(15)?,
                        },
                    ))
                },
            )
            .optional()?;

        if compressions == 0 && retrievals == 0 && proxy.is_none() {
            return Ok(None);
        }

        let (mode, model, surface) = match head {
            Some((mode, model, surface)) => (Some(mode), model, Some(surface)),
            None => (None, None, None),
        };
        let proxy_time = proxy.as_ref().map(|(time_ms, _)| *time_ms);
        let time_ms = match (first_opt_time, proxy_time) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => 0,
        };
        let saved_gross = tokens_in - tokens_out;

        Ok(Some(RequestEconomy {
            tenant: tenant.to_string(),
            request_id: request_id.to_string(),
            time_ms,
            mode,
            model: model.or_else(|| proxy.as_ref().and_then(|(_, p)| p.model.clone())),
            surface,
            compressions: compressions as u64,
            tokens_in,
            tokens_out,
            saved_gross,
            retrieved_tokens,
            saved_net: saved_gross - retrieved_tokens,
            retrievals: retrievals as u64,
            retrieval_hits: hits as u64,
            proxy: proxy.map(|(_, p)| p),
        }))
    }

    /// The most recent fragment-store sample.
    ///
    /// `tenant` is accepted for symmetry with the other queries and ignored:
    /// the store sweep walks the whole file, so the sample is per process. It
    /// stays in the signature because making it per-tenant later should not be
    /// a breaking change at every call site.
    pub fn store(&self, _tenant: &str) -> Result<Option<StoreSample>, StatsError> {
        Ok(self
            .conn
            .query_row(
                "SELECT time_ms, live_refs, live_bytes FROM store_samples
                 ORDER BY time_ms DESC LIMIT 1",
                [],
                |row| {
                    Ok(StoreSample {
                        time_ms: row.get(0)?,
                        live_refs: row.get(1)?,
                        live_bytes: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Daily aggregates, inclusive of both `YYYY-MM-DD` bounds.
    ///
    /// These survive [`crate::StatsWriter::prune`], so they are the only thing
    /// that answers a question older than the retention window.
    pub fn rollups(
        &self,
        tenant: &str,
        from_day: &str,
        to_day: &str,
    ) -> Result<Vec<DailyRollup>, StatsError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT day, tenant, mode, surface,
                    COALESCE(requests,0), COALESCE(tokens_in,0), COALESCE(tokens_out,0),
                    COALESCE(retrievals,0), COALESCE(retrieval_hits,0), COALESCE(retrieved_tokens,0)
             FROM daily_rollups
             WHERE tenant = ?1 AND day >= ?2 AND day <= ?3
             ORDER BY day, mode, surface",
        )?;
        let rows = stmt.query_map((tenant, from_day, to_day), |row| {
            Ok(DailyRollup {
                day: row.get(0)?,
                tenant: row.get(1)?,
                mode: row.get(2)?,
                surface: row.get(3)?,
                requests: row.get(4)?,
                tokens_in: row.get(5)?,
                tokens_out: row.get(6)?,
                retrievals: row.get(7)?,
                retrieval_hits: row.get(8)?,
                retrieved_tokens: row.get(9)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// `(attempts, hits, tokens re-injected by hits)` for a `WHERE` fragment.
    ///
    /// The debit counts **found** rows only: a miss returns an apology, not a
    /// fragment, so charging the economy for it would invent an expense.
    /// Attempts, hits and tokens returned — over the asks that reached the
    /// store.
    ///
    /// Withheld rows are excluded here for the same reason they are in
    /// [`StatsReader::retrievals`], and this one matters more: it feeds
    /// `kompressor stats summary`, the headline. Counting them reported `25
    /// retrievals (14 hits, 56.0% hit rate)` for a database in which every one
    /// of the 14 asks the store saw was answered — an invented 44% failure
    /// rate, on the one line most readers will look at. Measured on a real
    /// database after the column shipped and before this followed.
    fn retrieval_totals(
        &self,
        predicate: &str,
        params: impl rusqlite::Params,
    ) -> Result<(i64, i64, i64), StatsError> {
        let served = self.served();
        let sql = format!(
            "SELECT COALESCE(SUM(CASE WHEN {served} THEN 1 ELSE 0 END),0),
                    COALESCE(SUM(CASE WHEN {served} THEN found ELSE 0 END),0),
                    COALESCE(SUM(CASE WHEN {served} AND found = 1
                                      THEN returned_tokens ELSE 0 END),0)
             FROM retrieval_events WHERE {predicate}"
        );
        Ok(self.conn.query_row(&sql, params, |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?)
    }
}

/// `numerator / denominator`, or zero when there is nothing to divide.
pub(crate) fn ratio(numerator: i64, denominator: i64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}
