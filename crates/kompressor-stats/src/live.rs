//! One snapshot of what the service is doing right now.
//!
//! The other report queries answer one question each over an arbitrary window.
//! A live view needs most of them at once, over a short window that ends now,
//! plus what only the proxy rows know — status codes, cache reads, latency —
//! and the last few requests by name. [`StatsReader::live`] gathers all of it
//! in one call so a dashboard polling every couple of seconds costs one
//! blocking task rather than eight.
//!
//! "Live" is bounded by the writer, not by this query: events are committed on
//! [`crate::DEFAULT_FLUSH_INTERVAL`] (100 ms), so a request is visible here a
//! fraction of a second after it finishes.

use serde::{Deserialize, Serialize};

use crate::StatsError;
use crate::reader::{ProviderCoverage, RetrievalSummary, StatsReader, TypeCoverage, ratio};

/// The trailing windows [`LiveSnapshot::rates`] reports, widest last.
pub const RATE_WINDOWS: &[(&str, i64)] = &[("1m", 60_000), ("5m", 300_000), ("15m", 900_000)];

/// About how many points the timeline is cut into, whatever the window.
const TIMELINE_POINTS: i64 = 60;

/// Credits and debits over the whole window.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LiveTotals {
    /// Compressions recorded, on every surface.
    pub compressions: u64,
    /// Requests the proxy relayed, compressed or not.
    pub proxy_requests: u64,
    pub messages_total: i64,
    pub messages_compressed: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub saved_gross: i64,
    /// Tokens retrieval put back into prompts.
    pub retrieved_tokens: i64,
    /// `saved_gross - retrieved_tokens`. May be negative.
    pub saved_net: i64,
    /// `saved_gross / tokens_in`.
    pub ratio: f64,
    /// Handles minted, i.e. elisions the model could fetch back.
    pub refs_emitted: i64,
    pub engine_ms_total: f64,
    pub engine_ms_avg: f64,
}

/// Throughput over one trailing window ending now.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RateWindow {
    pub window: String,
    pub compressions: u64,
    pub proxy_requests: u64,
    pub tokens_in: i64,
    pub saved_gross: i64,
    pub requests_per_min: f64,
    pub saved_per_min: f64,
}

/// One point of the timeline.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TimelinePoint {
    pub bucket_ms: i64,
    pub compressions: u64,
    pub proxy_requests: u64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub saved_gross: i64,
}

/// p50/p95/p99/max of `samples` by nearest rank (`ceil(n * q) - 1`), the same
/// rank the SQL version took its `OFFSET` at.
pub(crate) fn percentiles(mut samples: Vec<f64>) -> Latency {
    if samples.is_empty() {
        return Latency::default();
    }
    samples.sort_by(f64::total_cmp);
    let n = samples.len();
    let at = |quantile: f64| {
        let rank = ((n as f64 * quantile).ceil() as usize).clamp(1, n) - 1;
        Some(samples[rank])
    };
    Latency {
        samples: n as u64,
        p50: at(0.50),
        p95: at(0.95),
        p99: at(0.99),
        max: at(1.0),
    }
}

/// Percentiles of one latency column, in milliseconds.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Latency {
    /// Rows the percentiles were taken over.
    pub samples: u64,
    pub p50: Option<f64>,
    pub p95: Option<f64>,
    pub p99: Option<f64>,
    pub max: Option<f64>,
}

/// What the upstream answered, from the proxy's own rows.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UpstreamStats {
    pub requests: u64,
    pub status_2xx: u64,
    pub status_3xx: u64,
    pub status_4xx: u64,
    pub status_5xx: u64,
    /// No status recorded: the upstream never answered.
    pub no_status: u64,
    /// Rows carrying an error message.
    pub errors: u64,
    pub streamed: u64,
    /// Requests the provider reported usage for. A relayed WebSocket turn and a
    /// failed request report none, so the token sums below cover only these.
    pub with_usage: u64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// Cache reads as a share of every input token the provider billed.
    pub cache_read_share: f64,
    pub retrieve_rounds: i64,
    pub retrieve_tokens_injected: i64,
    pub upstream_ms: Latency,
    pub total_ms: Latency,
}

/// Savings by the model a request named.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelCoverage {
    pub model: String,
    pub compressions: u64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub saved: i64,
    pub ratio: f64,
}

/// How many messages the engine left alone for one reason.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SkipReason {
    pub reason: String,
    pub messages: i64,
}

/// One recent request: a proxy row with whatever was compressed for it, or a
/// compression made on another surface.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RecentRequest {
    pub time_ms: i64,
    pub request_id: String,
    pub surface: String,
    pub provider: Option<String>,
    pub api_family: Option<String>,
    pub model: Option<String>,
    pub upstream: Option<String>,
    pub status: Option<i64>,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub saved: i64,
    pub messages_compressed: i64,
    pub refs_emitted: i64,
    pub prompt_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub retrieve_rounds: i64,
    pub total_ms: Option<f64>,
    pub error: Option<String>,
}

/// Everything the live view shows, for one tenant and one window.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveSnapshot {
    pub tenant: String,
    pub from_ms: i64,
    pub to_ms: i64,
    /// Width of one [`TimelinePoint`].
    pub bucket_ms: i64,
    pub totals: LiveTotals,
    pub rates: Vec<RateWindow>,
    pub timeline: Vec<TimelinePoint>,
    pub upstream: UpstreamStats,
    pub by_provider: Vec<ProviderCoverage>,
    pub by_model: Vec<ModelCoverage>,
    pub by_type: Vec<TypeCoverage>,
    pub skipped_by_reason: Vec<SkipReason>,
    pub retrievals: RetrievalSummary,
    pub recent: Vec<RecentRequest>,
}

/// The timeline bucket for a window: about `TIMELINE_POINTS` of them, never
/// narrower than a second, rounded to a whole minute once it is wider than one
/// so the labels read naturally.
pub fn bucket_for(window_ms: i64) -> i64 {
    let raw = (window_ms / TIMELINE_POINTS).max(1_000);
    if raw >= 60_000 {
        (raw / 60_000) * 60_000
    } else {
        raw
    }
}

impl StatsReader {
    /// Snapshot `[from_ms, to_ms)` for `tenant`, with the `recent` latest
    /// requests and trailing rates measured back from `to_ms`.
    pub fn live(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
        recent: usize,
    ) -> Result<LiveSnapshot, StatsError> {
        let bucket_ms = bucket_for(to_ms.saturating_sub(from_ms));
        // One committed snapshot for every panel. Without it, a page polled
        // while the writer commits could show totals from one batch and the
        // timeline or recent list from the next, which do not add up.
        let tx = self.conn.unchecked_transaction()?;

        // The totals and the trailing rates come from one pass over the wider
        // of the window and the longest rate window, with each figure a
        // conditional sum, rather than one scan per figure.
        let widest = RATE_WINDOWS.iter().map(|(_, w)| *w).max().unwrap_or(0);
        let scan_from = from_ms.min(to_ms - widest);
        let rate_from: Vec<i64> = RATE_WINDOWS.iter().map(|(_, w)| to_ms - w).collect();
        let (totals_row, rate_rows) =
            self.live_compressions(tenant, from_ms, to_ms, scan_from, &rate_from)?;
        let (proxy_requests, rate_proxy) =
            self.live_proxy_counts(tenant, from_ms, to_ms, scan_from, &rate_from)?;
        let (_, _, retrieved_tokens) = self.retrieval_totals(
            "tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3",
            (tenant, from_ms, to_ms),
        )?;
        let [
            requests,
            tokens_in,
            tokens_out,
            messages_total,
            messages_compressed,
            refs_emitted,
        ] = totals_row.0;
        let engine_ms_total = totals_row.1;
        let requests = requests.max(0) as u64;
        let saved_gross = tokens_in - tokens_out;
        let totals = LiveTotals {
            compressions: requests,
            proxy_requests,
            messages_total,
            messages_compressed,
            tokens_in,
            tokens_out,
            saved_gross,
            retrieved_tokens,
            saved_net: saved_gross - retrieved_tokens,
            ratio: ratio(saved_gross, tokens_in),
            refs_emitted,
            engine_ms_total,
            engine_ms_avg: if requests > 0 {
                engine_ms_total / requests as f64
            } else {
                0.0
            },
        };

        let rates = RATE_WINDOWS
            .iter()
            .zip(rate_rows)
            .zip(rate_proxy)
            .map(
                |(((label, width), (compressions, tokens_in, saved_gross)), proxy_requests)| {
                    let minutes = *width as f64 / 60_000.0;
                    let compressions = compressions.max(0) as u64;
                    // A proxy-only deployment counts its traffic in proxy rows; an
                    // API-only one has none. Whichever is larger is the request rate.
                    let requests = proxy_requests.max(compressions);
                    RateWindow {
                        window: label.to_string(),
                        compressions,
                        proxy_requests,
                        tokens_in,
                        saved_gross,
                        requests_per_min: requests as f64 / minutes,
                        saved_per_min: saved_gross as f64 / minutes,
                    }
                },
            )
            .collect();

        let snapshot = LiveSnapshot {
            tenant: tenant.to_string(),
            from_ms,
            to_ms,
            bucket_ms,
            totals,
            rates,
            timeline: self.timeline(tenant, from_ms, to_ms, bucket_ms)?,
            upstream: self.upstream(tenant, from_ms, to_ms)?,
            by_provider: self.by_provider(tenant, from_ms, to_ms)?,
            by_model: self.by_model(tenant, from_ms, to_ms)?,
            by_type: self.by_type(tenant, from_ms, to_ms)?,
            skipped_by_reason: self.skipped(tenant, from_ms, to_ms)?,
            retrievals: self.retrievals(tenant, from_ms, to_ms)?,
            recent: self.recent(tenant, from_ms, to_ms, recent)?,
        };
        tx.commit()?;
        Ok(snapshot)
    }

    /// Window totals of `optimization_events` (count, tokens in/out, messages
    /// total/compressed, refs, engine ms) and, per trailing rate window
    /// starting at `rate_from[i]`, (count, tokens in, saved) — one pass over
    /// `[scan_from, to_ms)`.
    #[allow(clippy::type_complexity)]
    fn live_compressions(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
        scan_from: i64,
        rate_from: &[i64],
    ) -> Result<(([i64; 6], f64), Vec<(i64, i64, i64)>), StatsError> {
        let mut columns = String::from(
            "COALESCE(SUM(time_ms >= ?2), 0),
             COALESCE(SUM(CASE WHEN time_ms >= ?2 THEN tokens_in END), 0),
             COALESCE(SUM(CASE WHEN time_ms >= ?2 THEN (CASE WHEN coalesce(outcome,'') = 'discarded' THEN tokens_in ELSE tokens_out END) END), 0),
             COALESCE(SUM(CASE WHEN time_ms >= ?2 THEN messages_total END), 0),
             COALESCE(SUM(CASE WHEN time_ms >= ?2 THEN messages_compressed END), 0),
             COALESCE(SUM(CASE WHEN time_ms >= ?2 THEN refs_emitted END), 0),
             COALESCE(SUM(CASE WHEN time_ms >= ?2 THEN engine_ms END), 0.0)",
        );
        let mut values: Vec<rusqlite::types::Value> = vec![
            tenant.to_string().into(),
            from_ms.into(),
            to_ms.into(),
            scan_from.into(),
        ];
        for start in rate_from {
            values.push((*start).into());
            let n = values.len();
            columns.push_str(&format!(
                ", COALESCE(SUM(time_ms >= ?{n}), 0),
                   COALESCE(SUM(CASE WHEN time_ms >= ?{n} THEN tokens_in END), 0),
                   COALESCE(SUM(CASE WHEN time_ms >= ?{n} THEN tokens_in - (CASE WHEN coalesce(outcome,'') = 'discarded' THEN tokens_in ELSE tokens_out END) END), 0)"
            ));
        }
        let sql = format!(
            "SELECT {columns} FROM optimization_events
             WHERE tenant = ?1 AND time_ms >= ?4 AND time_ms < ?3"
        );
        Ok(self.conn.prepare_cached(&sql)?.query_row(
            rusqlite::params_from_iter(values),
            |row| {
                let mut totals = [0i64; 6];
                for (i, slot) in totals.iter_mut().enumerate() {
                    *slot = row.get(i)?;
                }
                let engine: f64 = row.get(6)?;
                let mut rates = Vec::with_capacity(rate_from.len());
                for i in 0..rate_from.len() {
                    let at = 7 + 3 * i;
                    rates.push((row.get(at)?, row.get(at + 1)?, row.get(at + 2)?));
                }
                Ok(((totals, engine), rates))
            },
        )?)
    }

    /// Proxy rows in the window and in each trailing rate window, from one
    /// pass that the `(tenant, time_ms)` index answers on its own.
    fn live_proxy_counts(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
        scan_from: i64,
        rate_from: &[i64],
    ) -> Result<(u64, Vec<u64>), StatsError> {
        let mut columns = String::from("COALESCE(SUM(time_ms >= ?2), 0)");
        let mut values: Vec<rusqlite::types::Value> = vec![
            tenant.to_string().into(),
            from_ms.into(),
            to_ms.into(),
            scan_from.into(),
        ];
        for start in rate_from {
            values.push((*start).into());
            columns.push_str(&format!(", COALESCE(SUM(time_ms >= ?{}), 0)", values.len()));
        }
        let sql = format!(
            "SELECT {columns} FROM proxy_requests
             WHERE tenant = ?1 AND time_ms >= ?4 AND time_ms < ?3"
        );
        Ok(self.conn.prepare_cached(&sql)?.query_row(
            rusqlite::params_from_iter(values),
            |row| {
                let count = |i: usize| row.get::<_, i64>(i).map(|v| v.max(0) as u64);
                let total = count(0)?;
                let mut rates = Vec::with_capacity(rate_from.len());
                for i in 0..rate_from.len() {
                    rates.push(count(1 + i)?);
                }
                Ok((total, rates))
            },
        )?)
    }

    fn timeline(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
        bucket_ms: i64,
    ) -> Result<Vec<TimelinePoint>, StatsError> {
        // Buckets are aligned to the window's start, so the first point is
        // the start of the window and the last one holds "now".
        let mut points: std::collections::BTreeMap<i64, TimelinePoint> =
            std::collections::BTreeMap::new();
        let mut stmt = self.conn.prepare_cached(
            "SELECT ?2 + ((time_ms - ?2) / ?4) * ?4 AS bucket,
                    COUNT(*), SUM(tokens_in), SUM((CASE WHEN coalesce(outcome,'') = 'discarded' THEN tokens_in ELSE tokens_out END))
             FROM optimization_events
             WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3
             GROUP BY bucket",
        )?;
        let rows = stmt.query_map((tenant, from_ms, to_ms, bucket_ms), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        for row in rows {
            let (bucket, count, tokens_in, tokens_out) = row?;
            let point = points.entry(bucket).or_default();
            point.compressions = count.max(0) as u64;
            point.tokens_in = tokens_in;
            point.tokens_out = tokens_out;
            point.saved_gross = tokens_in - tokens_out;
        }
        let mut stmt = self.conn.prepare_cached(
            "SELECT ?2 + ((time_ms - ?2) / ?4) * ?4 AS bucket, COUNT(*)
             FROM proxy_requests
             WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3
             GROUP BY bucket",
        )?;
        let rows = stmt.query_map((tenant, from_ms, to_ms, bucket_ms), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (bucket, count) = row?;
            points.entry(bucket).or_default().proxy_requests = count.max(0) as u64;
        }
        Ok(points
            .into_iter()
            .map(|(bucket_ms, mut point)| {
                point.bucket_ms = bucket_ms;
                point
            })
            .collect())
    }

    fn upstream(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<UpstreamStats, StatsError> {
        let (mut stats, billed_input) = self.conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(status BETWEEN 200 AND 299), 0),
                    COALESCE(SUM(status BETWEEN 300 AND 399), 0),
                    COALESCE(SUM(status BETWEEN 400 AND 499), 0),
                    COALESCE(SUM(status >= 500), 0),
                    COALESCE(SUM(status IS NULL), 0),
                    COALESCE(SUM(error IS NOT NULL AND error <> ''), 0),
                    COALESCE(SUM(stream), 0),
                    COALESCE(SUM(prompt_tokens IS NOT NULL), 0),
                    COALESCE(SUM(prompt_tokens), 0),
                    COALESCE(SUM(completion_tokens), 0),
                    COALESCE(SUM(cache_read_tokens), 0),
                    COALESCE(SUM(cache_write_tokens), 0),
                    COALESCE(SUM(retrieve_rounds), 0),
                    COALESCE(SUM(retrieve_tokens_injected), 0),
                    COALESCE(SUM(CASE WHEN api_family = 'anthropic_messages'
                                      THEN max(coalesce(prompt_tokens, 0), 0)
                                           + max(coalesce(cache_read_tokens, 0), 0)
                                           + max(coalesce(cache_write_tokens, 0), 0)
                                      ELSE max(coalesce(prompt_tokens, 0),
                                               coalesce(cache_read_tokens, 0), 0) END), 0)
             FROM proxy_requests WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3",
            (tenant, from_ms, to_ms),
            |row| {
                let count = |i: usize| row.get::<_, i64>(i).map(|v| v.max(0) as u64);
                let billed_input: i64 = row.get(15)?;
                Ok((
                    UpstreamStats {
                        requests: count(0)?,
                        status_2xx: count(1)?,
                        status_3xx: count(2)?,
                        status_4xx: count(3)?,
                        status_5xx: count(4)?,
                        no_status: count(5)?,
                        errors: count(6)?,
                        streamed: count(7)?,
                        with_usage: count(8)?,
                        prompt_tokens: row.get(9)?,
                        completion_tokens: row.get(10)?,
                        cache_read_tokens: row.get(11)?,
                        cache_write_tokens: row.get(12)?,
                        retrieve_rounds: row.get(13)?,
                        retrieve_tokens_injected: row.get(14)?,
                        ..Default::default()
                    },
                    billed_input,
                ))
            },
        )?;
        // Billed input, per row and per family. Anthropic reports fresh input,
        // cache reads and cache writes as three disjoint counts, and their sum
        // is the input; OpenAI's prompt count already contains its cached
        // part. Adding the three for every family put OpenAI's reads in the
        // denominator twice and understated the share — 44% for a prompt that
        // was 80% cached.
        stats.cache_read_share = ratio(stats.cache_read_tokens, billed_input);
        (stats.upstream_ms, stats.total_ms) = self.latencies(tenant, from_ms, to_ms)?;
        Ok(stats)
    }

    /// Percentiles of `upstream_ms` and `total_ms` by rank.
    ///
    /// Neither column is indexed, so each "ORDER BY col LIMIT 1 OFFSET k" the
    /// earlier version ran was a sort of the whole window — four sorts and a
    /// count per column, ten statements per snapshot. Reading both columns in
    /// one pass and ranking them here costs one scan and two in-memory sorts.
    fn latencies(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<(Latency, Latency), StatsError> {
        let mut upstream = Vec::new();
        let mut total = Vec::new();
        let mut stmt = self.conn.prepare_cached(
            "SELECT upstream_ms, total_ms FROM proxy_requests
             WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3
               AND (upstream_ms IS NOT NULL OR total_ms IS NOT NULL)",
        )?;
        let mut rows = stmt.query((tenant, from_ms, to_ms))?;
        while let Some(row) = rows.next()? {
            if let Some(v) = row.get::<_, Option<f64>>(0)? {
                upstream.push(v);
            }
            if let Some(v) = row.get::<_, Option<f64>>(1)? {
                total.push(v);
            }
        }
        Ok((percentiles(upstream), percentiles(total)))
    }

    fn by_model(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<ModelCoverage>, StatsError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT COALESCE(model, ''), COUNT(*), SUM(tokens_in), SUM((CASE WHEN coalesce(outcome,'') = 'discarded' THEN tokens_in ELSE tokens_out END))
             FROM optimization_events
             WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3
             GROUP BY 1 ORDER BY 3 DESC, 1",
        )?;
        let rows = stmt.query_map((tenant, from_ms, to_ms), |row| {
            let tokens_in: i64 = row.get(2)?;
            let tokens_out: i64 = row.get(3)?;
            Ok(ModelCoverage {
                model: row.get(0)?,
                compressions: row.get::<_, i64>(1)?.max(0) as u64,
                tokens_in,
                tokens_out,
                saved: tokens_in - tokens_out,
                ratio: ratio(tokens_in - tokens_out, tokens_in),
            })
        })?;
        rows.map(|row| row.map_err(StatsError::from)).collect()
    }

    /// Why messages were left alone. The column arrived in a later schema
    /// version; rows written before it have none and are skipped.
    fn skipped(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<SkipReason>, StatsError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT j.key, SUM(j.value)
             FROM optimization_events o, json_each(o.skipped_by_reason) AS j
             WHERE o.tenant = ?1 AND o.time_ms >= ?2 AND o.time_ms < ?3
               AND o.skipped_by_reason IS NOT NULL AND json_valid(o.skipped_by_reason)
             GROUP BY j.key ORDER BY 2 DESC, 1",
        )?;
        let rows = stmt.query_map((tenant, from_ms, to_ms), |row| {
            Ok(SkipReason {
                reason: row.get(0)?,
                messages: row.get(1)?,
            })
        })?;
        rows.map(|row| row.map_err(StatsError::from)).collect()
    }

    /// The latest `limit` requests: proxy rows, each with the compressions
    /// recorded under its request id, and compressions made on any other
    /// surface, merged newest first.
    fn recent(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
        limit: usize,
    ) -> Result<Vec<RecentRequest>, StatsError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let limit_sql = limit as i64;
        let mut out = Vec::new();

        // Joined on the request id rather than `compression_event_id`, which
        // is not set on every path that records a compression.
        let mut stmt = self.conn.prepare_cached(
            "SELECT p.time_ms, p.request_id, p.api_family, p.model, p.upstream, p.status,
                    p.prompt_tokens, p.cache_read_tokens, p.retrieve_rounds, p.total_ms, p.error,
                    o.provider, COALESCE(o.tokens_in, 0), COALESCE(o.tokens_out, 0),
                    COALESCE(o.messages_compressed, 0), COALESCE(o.refs_emitted, 0)
             FROM proxy_requests p
             LEFT JOIN (
                 SELECT request_id, MAX(provider) AS provider, SUM(tokens_in) AS tokens_in,
                        SUM((CASE WHEN coalesce(outcome,'') = 'discarded' THEN tokens_in ELSE tokens_out END)) AS tokens_out,
                        SUM(messages_compressed) AS messages_compressed,
                        SUM(refs_emitted) AS refs_emitted
                 FROM optimization_events
                 WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3 AND surface = 'proxy'
                 GROUP BY request_id
             ) o ON o.request_id = p.request_id
             WHERE p.tenant = ?1 AND p.time_ms >= ?2 AND p.time_ms < ?3
             ORDER BY p.time_ms DESC, p.id DESC LIMIT ?4",
        )?;
        let rows = stmt.query_map((tenant, from_ms, to_ms, limit_sql), |row| {
            let tokens_in: i64 = row.get(12)?;
            let tokens_out: i64 = row.get(13)?;
            Ok(RecentRequest {
                time_ms: row.get(0)?,
                request_id: row.get(1)?,
                surface: "proxy".to_string(),
                api_family: row.get(2)?,
                model: row.get(3)?,
                upstream: row.get(4)?,
                status: row.get(5)?,
                prompt_tokens: row.get(6)?,
                cache_read_tokens: row.get(7)?,
                retrieve_rounds: row.get(8)?,
                total_ms: row.get(9)?,
                error: row.get(10)?,
                provider: row.get(11)?,
                tokens_in,
                tokens_out,
                saved: tokens_in - tokens_out,
                messages_compressed: row.get(14)?,
                refs_emitted: row.get(15)?,
            })
        })?;
        for row in rows {
            out.push(row?);
        }

        let mut stmt = self.conn.prepare_cached(
            "SELECT time_ms, request_id, surface, provider, api_family, model,
                    tokens_in, tokens_out, messages_compressed, refs_emitted
             FROM optimization_events
             WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3 AND surface <> 'proxy'
             ORDER BY time_ms DESC, id DESC LIMIT ?4",
        )?;
        let rows = stmt.query_map((tenant, from_ms, to_ms, limit_sql), |row| {
            let tokens_in: i64 = row.get(6)?;
            let tokens_out: i64 = row.get(7)?;
            Ok(RecentRequest {
                time_ms: row.get(0)?,
                request_id: row.get(1)?,
                surface: row.get(2)?,
                provider: row.get(3)?,
                api_family: row.get(4)?,
                model: row.get(5)?,
                tokens_in,
                tokens_out,
                saved: tokens_in - tokens_out,
                messages_compressed: row.get(8)?,
                refs_emitted: row.get(9)?,
                ..Default::default()
            })
        })?;
        for row in rows {
            out.push(row?);
        }

        out.sort_by_key(|row| std::cmp::Reverse(row.time_ms));
        out.truncate(limit);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Event, OptimizationEvent, ProxyRequestEvent, RetrievalEvent, StatsConfig, StatsHandle,
        StatsSink, Surface, spawn,
    };
    use kompressor_engine::types::{Kind, Mode};
    use tempfile::TempDir;

    const T0: i64 = 1_700_000_000_000;
    const MIN: i64 = 60_000;

    struct Db {
        dir: TempDir,
        sink: StatsSink,
        handle: Option<StatsHandle>,
    }

    impl Db {
        fn new() -> Db {
            let dir = tempfile::tempdir().expect("a temp dir");
            let (sink, handle) =
                spawn(&StatsConfig::at(dir.path().join("stats.db"))).expect("the writer starts");
            Db {
                dir,
                sink,
                handle: Some(handle),
            }
        }

        fn write(&self, events: Vec<Event>) {
            for event in events {
                self.sink.send(event);
            }
            self.sink.flush().expect("the batch commits");
        }

        fn reader(&self) -> StatsReader {
            StatsReader::open(self.dir.path().join("stats.db")).expect("opens")
        }
    }

    impl Drop for Db {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                let _ = handle.shutdown();
            }
        }
    }

    fn compression(time_ms: i64, tenant: &str, id: &str, tokens_in: u32, tokens_out: u32) -> Event {
        let mut event =
            OptimizationEvent::new(time_ms, tenant, id, Surface::Proxy, Mode::Recoverable);
        event.tokens_in = tokens_in;
        event.tokens_out = tokens_out;
        event.messages_total = 4;
        event.messages_compressed = 1;
        event.messages_skipped = 3;
        event.refs_emitted = 2;
        event.engine_ms = 3.0;
        event.provider = Some("opencode-zen".into());
        event.model = Some("glm-5.3".into());
        event.record_type(Kind::Log, 1);
        event.skipped_by_reason.insert("protected:frozen".into(), 2);
        event.skipped_by_reason.insert("opaque".into(), 1);
        Event::Optimization(event)
    }

    fn proxied(time_ms: i64, tenant: &str, id: &str, status: Option<u16>, total_ms: f64) -> Event {
        Event::ProxyRequest(ProxyRequestEvent {
            prefix: None,
            context: None,
            timing: None,
            time_ms,
            tenant: tenant.into(),
            request_id: id.into(),
            // Anthropic's shape: the cache read is reported beside the fresh
            // input, not inside it.
            api_family: "anthropic_messages".into(),
            model: Some("glm-5.3".into()),
            upstream: "https://opencode.ai".into(),
            status,
            stream: true,
            strip_usage: false,
            prompt_tokens: status.map(|_| 100),
            completion_tokens: status.map(|_| 10),
            cache_read_tokens: status.map(|_| 300),
            cache_write_tokens: None,
            cache_write_1h_tokens: None,
            shadow_prompt_tokens: None,
            conversation: None,
            retrieve_rounds: 1,
            retrieve_tokens_injected: 50,
            upstream_ms: Some(total_ms - 5.0),
            total_ms: Some(total_ms),
            error: status.is_none().then(|| "connect refused".to_string()),
            keep_warm: false,
        })
    }

    /// OpenAI's prompt count already contains its cached part, so a prompt of
    /// 1,000 with 800 read from cache is 80% cached — not 800 / 1,800, which
    /// is what adding the two (right for Anthropic) reported.
    #[test]
    fn the_cache_read_share_is_taken_per_api_family() {
        let db = Db::new();
        let openai = |id: &str, prompt: u32, read: u32| match proxied(T0, "", id, Some(200), 10.0) {
            Event::ProxyRequest(mut event) => {
                event.api_family = "openai_chat".into();
                event.prompt_tokens = Some(prompt);
                event.cache_read_tokens = Some(read);
                Event::ProxyRequest(event)
            }
            other => other,
        };
        db.write(vec![openai("o1", 1_000, 800)]);
        let share = |db: &Db| {
            db.reader()
                .live("", T0, T0 + 60 * MIN, 10)
                .unwrap()
                .upstream
                .cache_read_share
        };
        assert!((share(&db) - 0.8).abs() < 1e-9, "{}", share(&db));

        // Mixed with an Anthropic row (100 fresh + 300 read): 1,100 of 1,400.
        db.write(vec![proxied(T0 + 1, "", "a1", Some(200), 10.0)]);
        assert!(
            (share(&db) - 1_100.0 / 1_400.0).abs() < 1e-9,
            "{}",
            share(&db)
        );
    }

    /// A rewrite the proxy threw away never reached the provider. It is still
    /// a run — it counts, and so does what it was shown — but it saved
    /// nothing, on the live page as in the views.
    #[test]
    fn a_discarded_rewrite_saves_nothing_on_the_live_page() {
        let db = Db::new();
        db.write(vec![
            compression(T0, "", "kept", 1_000, 400),
            compression(T0 + 1, "", "thrown", 1_000, 100),
            Event::Discarded {
                tenant: "".into(),
                request_id: "thrown".into(),
            },
        ]);
        let live = db.reader().live("", T0, T0 + 60 * MIN, 10).unwrap();
        assert_eq!(live.totals.compressions, 2);
        assert_eq!(
            (live.totals.tokens_in, live.totals.saved_gross),
            (2_000, 600)
        );
        assert_eq!(
            live.timeline.iter().map(|p| p.saved_gross).sum::<i64>(),
            600
        );
        assert_eq!(live.by_model.iter().map(|m| m.saved).sum::<i64>(), 600);
    }

    fn fetched(time_ms: i64, tenant: &str, id: &str, tokens: u32) -> Event {
        Event::Retrieval(RetrievalEvent {
            context: None,
            time_ms,
            tenant: tenant.into(),
            request_id: Some(id.into()),
            surface: Surface::Proxy,
            reference: "k_abc".into(),
            found: true,
            expired: false,
            ranged: false,
            fragment_bytes: u64::from(tokens) * 4,
            fragment_chars: u64::from(tokens) * 4,
            fragment_tokens: tokens,
            returned_chars: u64::from(tokens) * 4,
            returned_tokens: tokens,
            withheld: false,
            range_status: None,
        })
    }

    #[test]
    fn the_timeline_has_about_sixty_points_and_whole_minutes_once_wide() {
        assert_eq!(bucket_for(60 * MIN), MIN, "an hour is one point a minute");
        assert_eq!(bucket_for(24 * 60 * MIN), 24 * MIN);
        assert_eq!(
            bucket_for(5 * MIN),
            5_000,
            "under a minute stays in seconds"
        );
        assert_eq!(bucket_for(10_000), 1_000, "never narrower than a second");
    }

    #[test]
    fn a_snapshot_joins_compressions_proxy_rows_and_retrievals() {
        let db = Db::new();
        db.write(vec![
            compression(T0, "", "r1", 1_000, 200),
            proxied(T0, "", "r1", Some(200), 100.0),
            compression(T0 + MIN, "", "r2", 500, 500),
            proxied(T0 + MIN, "", "r2", Some(502), 300.0),
            proxied(T0 + 2 * MIN, "", "r3", None, 50.0),
            fetched(T0 + MIN, "", "r2", 40),
        ]);
        let live = db.reader().live("", T0, T0 + 60 * MIN, 10).unwrap();

        let t = &live.totals;
        assert_eq!(t.compressions, 2);
        assert_eq!(
            t.proxy_requests, 3,
            "a passthrough is a request with no compression"
        );
        assert_eq!(
            (t.tokens_in, t.tokens_out, t.saved_gross),
            (1_500, 700, 800)
        );
        assert_eq!(t.retrieved_tokens, 40);
        assert_eq!(t.saved_net, 760);
        assert_eq!(t.refs_emitted, 4);
        assert!((t.ratio - 800.0 / 1_500.0).abs() < 1e-9);
        assert!((t.engine_ms_avg - 3.0).abs() < 1e-9);

        let u = &live.upstream;
        assert_eq!(
            (u.requests, u.status_2xx, u.status_5xx, u.no_status),
            (3, 1, 1, 1)
        );
        assert_eq!(u.errors, 1);
        assert_eq!(
            u.with_usage, 2,
            "a request that never got an answer reported no usage"
        );
        assert_eq!((u.prompt_tokens, u.cache_read_tokens), (200, 600));
        assert!((u.cache_read_share - 600.0 / 800.0).abs() < 1e-9);
        assert_eq!(u.total_ms.samples, 3);
        assert_eq!(u.total_ms.p50, Some(100.0));
        assert_eq!(u.total_ms.max, Some(300.0));

        assert_eq!(live.by_model[0].model, "glm-5.3");
        assert_eq!(live.by_type[0].content_type, "log");
        assert_eq!(live.skipped_by_reason[0].reason, "protected:frozen");
        assert_eq!(live.skipped_by_reason[0].messages, 4);
        assert_eq!(live.retrievals.hits, 1);
    }

    #[test]
    fn recent_lists_newest_first_with_what_was_compressed_for_each() {
        let db = Db::new();
        let mut api =
            OptimizationEvent::new(T0 + 3 * MIN, "", "api-1", Surface::Api, Mode::Recoverable);
        api.tokens_in = 50;
        api.tokens_out = 10;
        db.write(vec![
            compression(T0, "", "r1", 1_000, 200),
            proxied(T0, "", "r1", Some(200), 100.0),
            proxied(T0 + MIN, "", "r2", Some(200), 90.0),
            Event::Optimization(api),
        ]);
        let live = db.reader().live("", T0, T0 + 60 * MIN, 10).unwrap();
        let ids: Vec<&str> = live.recent.iter().map(|r| r.request_id.as_str()).collect();
        assert_eq!(ids, ["api-1", "r2", "r1"]);
        assert_eq!(live.recent[0].surface, "api");
        assert_eq!(
            live.recent[2].saved, 800,
            "the proxy row carries its compression"
        );
        assert_eq!(live.recent[1].saved, 0, "and a passthrough shows none");
        assert_eq!(live.recent[2].status, Some(200));

        let two = db.reader().live("", T0, T0 + 60 * MIN, 2).unwrap();
        assert_eq!(two.recent.len(), 2, "the limit holds across both sources");
    }

    #[test]
    fn rates_count_only_the_trailing_window() {
        let db = Db::new();
        let now = T0 + 30 * MIN;
        db.write(vec![
            proxied(now - 30_000, "", "a", Some(200), 10.0),
            compression(now - 30_000, "", "a", 600, 0),
            proxied(now - 4 * MIN, "", "b", Some(200), 10.0),
            proxied(now - 20 * MIN, "", "c", Some(200), 10.0),
        ]);
        let live = db.reader().live("", T0, now, 0).unwrap();
        let by: std::collections::BTreeMap<&str, &RateWindow> =
            live.rates.iter().map(|r| (r.window.as_str(), r)).collect();
        assert_eq!(by["1m"].proxy_requests, 1);
        assert_eq!(by["5m"].proxy_requests, 2);
        assert_eq!(by["15m"].proxy_requests, 2, "twenty minutes ago is outside");
        assert!((by["1m"].requests_per_min - 1.0).abs() < 1e-9);
        assert!((by["1m"].saved_per_min - 600.0).abs() < 1e-9);
        assert!(live.recent.is_empty(), "a zero limit asks for no rows");
    }

    #[test]
    fn the_timeline_is_aligned_to_the_window_and_counts_both_kinds() {
        let db = Db::new();
        db.write(vec![
            compression(T0 + 10_000, "", "a", 100, 50),
            proxied(T0 + 10_000, "", "a", Some(200), 1.0),
            proxied(T0 + 2 * MIN + 1, "", "b", Some(200), 1.0),
        ]);
        let live = db.reader().live("", T0, T0 + 60 * MIN, 0).unwrap();
        assert_eq!(live.bucket_ms, MIN);
        let points: Vec<(i64, u64, u64)> = live
            .timeline
            .iter()
            .map(|p| (p.bucket_ms - T0, p.compressions, p.proxy_requests))
            .collect();
        assert_eq!(points, [(0, 1, 1), (2 * MIN, 0, 1)]);
        assert_eq!(live.timeline[0].saved_gross, 50);
    }

    #[test]
    fn another_tenants_traffic_is_not_in_the_snapshot() {
        let db = Db::new();
        db.write(vec![
            compression(T0, "team-a", "a", 100, 10),
            proxied(T0, "team-a", "a", Some(200), 5.0),
            compression(T0, "team-b", "b", 900, 10),
            proxied(T0, "team-b", "b", Some(500), 5.0),
        ]);
        let live = db.reader().live("team-a", T0, T0 + MIN, 10).unwrap();
        assert_eq!(live.totals.tokens_in, 100);
        assert_eq!(live.upstream.status_5xx, 0);
        assert_eq!(live.recent.len(), 1);
    }

    #[test]
    fn percentiles_take_the_nearest_rank_the_sql_offsets_did() {
        let p = percentiles((1..=100).rev().map(f64::from).collect());
        assert_eq!(p.samples, 100);
        assert_eq!(
            (p.p50, p.p95, p.p99, p.max),
            (Some(50.0), Some(95.0), Some(99.0), Some(100.0))
        );
        let one = percentiles(vec![7.0]);
        assert_eq!((one.p50, one.max), (Some(7.0), Some(7.0)));
        assert_eq!(percentiles(Vec::new()).samples, 0);
    }

    #[test]
    fn an_empty_window_is_zeros_not_an_error() {
        let db = Db::new();
        let live = db.reader().live("", T0, T0 + MIN, 10).unwrap();
        assert_eq!(live.totals.compressions, 0);
        assert_eq!(live.upstream.total_ms.samples, 0);
        assert_eq!(live.upstream.total_ms.p95, None);
        assert!(live.timeline.is_empty() && live.recent.is_empty());
    }
}
