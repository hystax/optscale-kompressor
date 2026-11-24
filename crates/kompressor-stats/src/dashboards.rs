//! The reports behind the second set of dashboards: long-horizon trends, the
//! engine, the upstreams, compression yield by content type, and the health of
//! the ref store.
//!
//! Every report reads tables the writer already fills; nothing here needs a
//! column or an index the schema does not have. The keep-warm return view
//! lives beside the cache report it shares a walk with
//! ([`crate::cache::keepwarm`]).
//!
//! The yield and store-health reports also describe the ref store, which is a
//! different database this crate cannot open. The caller hands them the
//! store's bookkeeping as [`StoredRef`]s — a census without fragment text — and
//! the joins happen here, where they can be tested.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::history::{HistoryFilter, HistoryMetrics, history_on};
use crate::live::{Latency, bucket_for, percentiles};
use crate::{StatsError, StatsReader};

const DAY_MS: i64 = 86_400_000;

/// The most rows a report reads, newest first. Past it the report says it
/// was truncated rather than silently describing a sample.
pub const MAX_ROWS: usize = 200_000;
/// The most points a trend draws.
pub const MAX_TREND_POINTS: usize = 400;
/// Failures closer together than this belong to one burst.
pub const BURST_GAP_MS: i64 = 60_000;
/// The fewest failures that make a burst.
pub const BURST_MIN_FAILURES: u64 = 3;
/// Bursts and failures listed, newest first.
pub const MAX_BURSTS: usize = 20;
pub const MAX_FAILURES: usize = 50;
/// Slowest engine runs listed.
pub const MAX_SLOWEST: usize = 20;
/// Characters of an upstream error message kept.
const ERROR_CHARS: usize = 300;

// ---------------------------------------------------------------- calendar

/// Days since 1970-01-01 for a proleptic Gregorian date (Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date `days` after 1970-01-01, as (year, month, day).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// `YYYY-MM-DD` of the UTC day holding `ms`.
pub fn utc_date(ms: i64) -> String {
    let (y, m, d) = civil_from_days(ms.div_euclid(DAY_MS));
    format!("{y:04}-{m:02}-{d:02}")
}

// ------------------------------------------------------------------ trends

/// How wide one trend point is. Weeks start on Monday, months on the 1st,
/// both in UTC.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrendBucket {
    Day,
    #[default]
    Week,
    Month,
}

impl TrendBucket {
    pub fn parse(name: &str) -> Option<TrendBucket> {
        match name {
            "day" => Some(TrendBucket::Day),
            "week" => Some(TrendBucket::Week),
            "month" => Some(TrendBucket::Month),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TrendBucket::Day => "day",
            TrendBucket::Week => "week",
            TrendBucket::Month => "month",
        }
    }

    /// Start of the bucket holding `ms`.
    fn start(self, ms: i64) -> i64 {
        let day = ms.div_euclid(DAY_MS);
        match self {
            TrendBucket::Day => day * DAY_MS,
            // 1970-01-01 was a Thursday: three days after a Monday.
            TrendBucket::Week => (day - (day + 3).rem_euclid(7)) * DAY_MS,
            TrendBucket::Month => {
                let (y, m, _) = civil_from_days(day);
                days_from_civil(y, m, 1) * DAY_MS
            }
        }
    }

    /// Start of the bucket after the one starting at `start`.
    fn next(self, start: i64) -> i64 {
        match self {
            TrendBucket::Day => start + DAY_MS,
            TrendBucket::Week => start + 7 * DAY_MS,
            TrendBucket::Month => {
                let (y, m, _) = civil_from_days(start.div_euclid(DAY_MS));
                let (y, m) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
                days_from_civil(y, m, 1) * DAY_MS
            }
        }
    }

    /// The buckets covering `[from, to)`, the first widened back to its start
    /// so every point is a whole period (the last may still be running).
    pub fn spans(self, from: i64, to: i64) -> Vec<(i64, i64)> {
        let mut spans = Vec::new();
        let mut start = self.start(from);
        while start < to {
            let next = self.next(start);
            spans.push((start, next.min(to)));
            start = next;
        }
        spans
    }
}

/// One period of the trend.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TrendPoint {
    pub from_ms: i64,
    pub to_ms: i64,
    /// UTC date the period starts on.
    pub label: String,
    pub metrics: HistoryMetrics,
    pub conversations: usize,
    /// Tokens the engine removed less tokens retrieved back.
    pub net_reduction: i64,
    /// Share of the uncached price the prompt cache saved:
    /// `1 - input_equivalent_cost / input_tokens`.
    pub cache_saving: Option<f64>,
    pub error_share: Option<f64>,
    pub mean_latency_ms: Option<f64>,
    /// Builds and models seen in the period.
    pub builds: Vec<String>,
    pub models: Vec<String>,
    /// Archived days that straddle the period's edge and so could not be
    /// split; they are left out of it.
    pub archived_partial_days_omitted: usize,
}

/// Where a build was first seen.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildMarker {
    pub build_version: String,
    pub first_ms: i64,
    pub label: String,
}

/// The long-horizon trend: one point per period, from archived days and raw
/// rows alike, so it reaches past the raw-row retention.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrendsReport {
    pub tenant: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub bucket: TrendBucket,
    /// Whether older periods were dropped to stay within
    /// [`MAX_TREND_POINTS`].
    pub truncated: bool,
    pub points: Vec<TrendPoint>,
    pub totals: TrendPoint,
    pub markers: Vec<BuildMarker>,
}

fn trend_point(
    conn: &rusqlite::Connection,
    tenant: &str,
    from: i64,
    to: i64,
) -> Result<TrendPoint, StatsError> {
    let summary = history_on(conn, tenant, from, to, &HistoryFilter::default())?;
    let mut builds = BTreeSet::new();
    let mut models = BTreeSet::new();
    for row in &summary.rows {
        if !row.dimension.build_version.is_empty() {
            builds.insert(row.dimension.build_version.clone());
        }
        if row.dimension.model != "unrecorded" && !row.dimension.model.is_empty() {
            models.insert(row.dimension.model.clone());
        }
    }
    let m = &summary.metrics;
    Ok(TrendPoint {
        from_ms: from,
        to_ms: to,
        label: utc_date(from),
        cache_saving: (m.input_tokens > 0)
            .then(|| 1.0 - m.input_equivalent_cost / m.input_tokens as f64),
        metrics: summary.metrics.clone(),
        conversations: summary.conversations,
        net_reduction: summary.net_reduction,
        error_share: summary.error_share,
        mean_latency_ms: summary.mean_latency_ms,
        builds: builds.into_iter().collect(),
        models: models.into_iter().collect(),
        archived_partial_days_omitted: summary.archived_partial_days_omitted,
    })
}

impl StatsReader {
    /// The trend for `tenant` over `[from_ms, to_ms)` in `bucket` periods.
    ///
    /// Past [`MAX_TREND_POINTS`] periods the newest are kept and the report
    /// says it was truncated.
    pub fn trends(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
        bucket: TrendBucket,
    ) -> Result<TrendsReport, StatsError> {
        let mut spans = bucket.spans(from_ms, to_ms);
        let truncated = spans.len() > MAX_TREND_POINTS;
        if truncated {
            spans.drain(..spans.len() - MAX_TREND_POINTS);
        }
        let _snapshot = self.conn.unchecked_transaction()?;
        let mut points = Vec::with_capacity(spans.len());
        for (from, to) in &spans {
            points.push(trend_point(&self.conn, tenant, *from, *to)?);
        }
        let start = spans.first().map_or(from_ms, |s| s.0);
        let totals = trend_point(&self.conn, tenant, start, to_ms)?;
        let mut seen = BTreeSet::new();
        let mut markers = Vec::new();
        for point in &points {
            for build in &point.builds {
                if seen.insert(build.clone()) {
                    markers.push(BuildMarker {
                        build_version: build.clone(),
                        first_ms: point.from_ms,
                        label: point.label.clone(),
                    });
                }
            }
        }
        Ok(TrendsReport {
            tenant: tenant.to_string(),
            from_ms: start,
            to_ms,
            bucket,
            truncated,
            points,
            totals,
            markers,
        })
    }
}

// ------------------------------------------------------------------ engine

/// Engine runs grouped one way.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EngineGroup {
    pub name: String,
    pub runs: u64,
    pub tokens_in: i64,
    pub saved: i64,
    pub total_ms: f64,
    pub latency: Latency,
    /// Engine milliseconds per thousand tokens in.
    pub ms_per_1k_tokens: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EnginePoint {
    pub bucket_ms: i64,
    pub runs: u64,
    pub total_ms: f64,
    pub p95_ms: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SlowRun {
    pub time_ms: i64,
    pub request_id: String,
    pub surface: String,
    pub engine_ms: f64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub messages: i64,
    /// The content type with the most messages in the run.
    pub content_type: String,
    pub outcome: Option<String>,
}

/// Where the compression engine spends its time.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EngineReport {
    pub tenant: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub bucket_ms: i64,
    pub runs: u64,
    pub truncated: bool,
    pub total_ms: f64,
    pub latency: Latency,
    pub by_type: Vec<EngineGroup>,
    pub by_size: Vec<EngineGroup>,
    pub by_outcome: Vec<EngineGroup>,
    pub by_surface: Vec<EngineGroup>,
    pub timeline: Vec<EnginePoint>,
    pub slowest: Vec<SlowRun>,
}

/// Prompt sizes the engine report groups by, as (label, upper bound).
pub const SIZE_BUCKETS: &[(&str, i64)] = &[
    ("<4K", 4_096),
    ("4K-16K", 16_384),
    ("16K-64K", 65_536),
    ("64K+", i64::MAX),
];

#[derive(Default)]
struct Samples {
    runs: u64,
    tokens_in: i64,
    saved: i64,
    total_ms: f64,
    ms: Vec<f64>,
}

impl Samples {
    fn add(&mut self, ms: f64, tokens_in: i64, saved: i64) {
        self.runs += 1;
        self.tokens_in += tokens_in;
        self.saved += saved;
        self.total_ms += ms;
        self.ms.push(ms);
    }

    fn group(self, name: String) -> EngineGroup {
        EngineGroup {
            name,
            runs: self.runs,
            tokens_in: self.tokens_in,
            saved: self.saved,
            total_ms: self.total_ms,
            ms_per_1k_tokens: (self.tokens_in > 0)
                .then(|| self.total_ms * 1_000.0 / self.tokens_in as f64),
            latency: percentiles(self.ms),
        }
    }
}

fn groups(map: BTreeMap<String, Samples>) -> Vec<EngineGroup> {
    let mut out: Vec<EngineGroup> = map
        .into_iter()
        .map(|(name, samples)| samples.group(name))
        .collect();
    out.sort_by(|a, b| b.total_ms.total_cmp(&a.total_ms).then(a.name.cmp(&b.name)));
    out
}

/// The type holding the most messages of a run; ties go to the first name.
fn dominant_type(content_types: &str) -> String {
    serde_json::from_str::<BTreeMap<String, i64>>(content_types)
        .ok()
        .and_then(|types| {
            types.into_iter().filter(|(_, n)| *n > 0).fold(
                None,
                |best: Option<(String, i64)>, (name, n)| match best {
                    Some((_, top)) if top >= n => best,
                    _ => Some((name, n)),
                },
            )
        })
        .map_or_else(|| "none".to_string(), |(name, _)| name)
}

impl StatsReader {
    /// Engine time for `tenant` over `[from_ms, to_ms)`.
    pub fn engine(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<EngineReport, StatsError> {
        let bucket_ms = bucket_for(to_ms.saturating_sub(from_ms));
        let mut stmt = self.conn.prepare_cached(
            "SELECT time_ms, request_id, surface, engine_ms, tokens_in, tokens_out,
                    messages_total, content_types, outcome
             FROM optimization_events
             WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3
             ORDER BY time_ms DESC, id DESC LIMIT ?4",
        )?;
        let mut rows = stmt.query(params![tenant, from_ms, to_ms, (MAX_ROWS + 1) as i64])?;
        let mut report = EngineReport {
            tenant: tenant.to_string(),
            from_ms,
            to_ms,
            bucket_ms,
            ..Default::default()
        };
        let mut all = Vec::new();
        let mut by_type: BTreeMap<String, Samples> = BTreeMap::new();
        let mut by_size: BTreeMap<String, Samples> = BTreeMap::new();
        let mut by_outcome: BTreeMap<String, Samples> = BTreeMap::new();
        let mut by_surface: BTreeMap<String, Samples> = BTreeMap::new();
        let mut timeline: BTreeMap<i64, Vec<f64>> = BTreeMap::new();
        let mut slowest: Vec<SlowRun> = Vec::new();
        while let Some(row) = rows.next()? {
            if all.len() == MAX_ROWS {
                report.truncated = true;
                break;
            }
            let time_ms: i64 = row.get(0)?;
            let ms: f64 = row.get::<_, f64>(3)?.max(0.0);
            let tokens_in: i64 = row.get(4)?;
            let tokens_out: i64 = row.get(5)?;
            let outcome: Option<String> = row.get(8)?;
            let saved = if outcome.as_deref() == Some("discarded") {
                0
            } else {
                tokens_in - tokens_out
            };
            let content_type = dominant_type(&row.get::<_, String>(7)?);
            let surface: String = row.get(2)?;
            all.push(ms);
            report.total_ms += ms;
            by_type
                .entry(content_type.clone())
                .or_default()
                .add(ms, tokens_in, saved);
            let size = SIZE_BUCKETS
                .iter()
                .find(|(_, upto)| tokens_in < *upto)
                .map_or("64K+", |(label, _)| label);
            by_size
                .entry(size.to_string())
                .or_default()
                .add(ms, tokens_in, saved);
            by_outcome
                .entry(outcome.clone().unwrap_or_else(|| "unrecorded".into()))
                .or_default()
                .add(ms, tokens_in, saved);
            by_surface
                .entry(surface.clone())
                .or_default()
                .add(ms, tokens_in, saved);
            timeline
                .entry(from_ms + ((time_ms - from_ms) / bucket_ms) * bucket_ms)
                .or_default()
                .push(ms);
            if slowest.len() < MAX_SLOWEST || slowest.last().is_some_and(|s| s.engine_ms < ms) {
                slowest.push(SlowRun {
                    time_ms,
                    request_id: row.get(1)?,
                    surface,
                    engine_ms: ms,
                    tokens_in,
                    tokens_out,
                    messages: row.get(6)?,
                    content_type,
                    outcome,
                });
                slowest.sort_by(|a, b| b.engine_ms.total_cmp(&a.engine_ms));
                slowest.truncate(MAX_SLOWEST);
            }
        }
        report.runs = all.len() as u64;
        report.latency = percentiles(all);
        report.by_type = groups(by_type);
        report.by_outcome = groups(by_outcome);
        report.by_surface = groups(by_surface);
        // Sizes keep their natural order rather than cost order.
        report.by_size = SIZE_BUCKETS
            .iter()
            .filter_map(|(label, _)| {
                by_size
                    .remove(*label)
                    .map(|samples| samples.group(label.to_string()))
            })
            .collect();
        report.timeline = timeline
            .into_iter()
            .map(|(bucket_ms, ms)| EnginePoint {
                bucket_ms,
                runs: ms.len() as u64,
                total_ms: ms.iter().sum(),
                p95_ms: percentiles(ms).p95,
            })
            .collect();
        report.slowest = slowest;
        Ok(report)
    }
}

// ---------------------------------------------------------------- upstream

/// What a set of upstream requests came back with.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StatusMix {
    pub requests: u64,
    pub ok: u64,
    pub redirect: u64,
    /// 4xx other than 429.
    pub client_error: u64,
    pub rate_limited: u64,
    pub server_error: u64,
    /// No status: the upstream never answered.
    pub no_status: u64,
    /// Requests that failed by any measure: no status, 4xx/5xx, or an error.
    pub failed: u64,
    /// Requests the proxy sent more than once.
    pub retried: u64,
    pub pings: u64,
}

impl StatusMix {
    fn add(&mut self, row: &UpstreamRow) {
        self.requests += 1;
        match row.status {
            None => self.no_status += 1,
            Some(429) => self.rate_limited += 1,
            Some(s) if (200..300).contains(&s) => self.ok += 1,
            Some(s) if (300..400).contains(&s) => self.redirect += 1,
            Some(s) if (400..500).contains(&s) => self.client_error += 1,
            Some(s) if s >= 500 => self.server_error += 1,
            Some(_) => {}
        }
        if row.failed() {
            self.failed += 1;
        }
        if row.attempts > 1 {
            self.retried += 1;
        }
        if row.keep_warm {
            self.pings += 1;
        }
    }

    fn error_share(&self) -> Option<f64> {
        (self.requests > 0).then(|| self.failed as f64 / self.requests as f64)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UpstreamGroup {
    pub upstream: String,
    pub model: String,
    pub mix: StatusMix,
    pub error_share: Option<f64>,
    pub upstream_ms: Latency,
    pub total_ms: Latency,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UpstreamPoint {
    pub bucket_ms: i64,
    pub requests: u64,
    pub failed: u64,
    pub rate_limited: u64,
    pub server_error: u64,
    pub p95_upstream_ms: Option<f64>,
}

/// Upstream incidents — no answer, 429, 5xx or a broken stream — close
/// enough together to be one episode. A 4xx the upstream rightly returned is a
/// failure but not an incident.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FailureBurst {
    pub from_ms: i64,
    pub to_ms: i64,
    pub failures: u64,
    /// Failures per status (`"429"`, `"503"`, `"none"`).
    pub statuses: BTreeMap<String, u64>,
    pub upstreams: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpstreamFailure {
    pub time_ms: i64,
    pub request_id: String,
    pub upstream: String,
    pub model: Option<String>,
    pub status: Option<i64>,
    pub error: Option<String>,
    pub attempts: i64,
    pub upstream_ms: Option<f64>,
    pub keep_warm: bool,
}

/// How the upstreams behaved.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UpstreamReport {
    pub tenant: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub bucket_ms: i64,
    pub truncated: bool,
    pub totals: StatusMix,
    pub error_share: Option<f64>,
    pub upstream_ms: Latency,
    pub by_upstream: Vec<UpstreamGroup>,
    pub timeline: Vec<UpstreamPoint>,
    pub bursts: Vec<FailureBurst>,
    pub failures: Vec<UpstreamFailure>,
}

struct UpstreamRow {
    time_ms: i64,
    request_id: String,
    upstream: String,
    model: Option<String>,
    status: Option<i64>,
    error: Option<String>,
    upstream_ms: Option<f64>,
    total_ms: Option<f64>,
    keep_warm: bool,
    attempts: i64,
}

impl UpstreamRow {
    fn failed(&self) -> bool {
        self.status.is_none_or(|s| s >= 400) || self.error.is_some()
    }

    /// Trouble on the upstream's side rather than a request it rightly
    /// refused: no answer, rate limiting, a 5xx, or a broken stream.
    fn incident(&self) -> bool {
        self.status.is_none_or(|s| s == 429 || s >= 500) || self.error.is_some()
    }

    fn status_name(&self) -> String {
        self.status.map_or_else(|| "none".into(), |s| s.to_string())
    }
}

fn finite(ms: Option<f64>) -> Option<f64> {
    ms.filter(|ms| ms.is_finite() && *ms >= 0.0)
}

fn bursts(failures: &[&UpstreamRow]) -> Vec<FailureBurst> {
    let mut out: Vec<FailureBurst> = Vec::new();
    let mut current: Option<(FailureBurst, BTreeSet<String>)> = None;
    for row in failures {
        let joins = current
            .as_ref()
            .is_some_and(|(burst, _)| row.time_ms - burst.to_ms <= BURST_GAP_MS);
        if !joins
            && let Some((mut burst, upstreams)) = current.take()
            && burst.failures >= BURST_MIN_FAILURES
        {
            burst.upstreams = upstreams.into_iter().collect();
            out.push(burst);
        }
        let (burst, upstreams) = current.get_or_insert_with(|| {
            (
                FailureBurst {
                    from_ms: row.time_ms,
                    to_ms: row.time_ms,
                    ..Default::default()
                },
                BTreeSet::new(),
            )
        });
        burst.to_ms = row.time_ms;
        burst.failures += 1;
        *burst.statuses.entry(row.status_name()).or_insert(0) += 1;
        upstreams.insert(row.upstream.clone());
    }
    if let Some((mut burst, upstreams)) = current
        && burst.failures >= BURST_MIN_FAILURES
    {
        burst.upstreams = upstreams.into_iter().collect();
        out.push(burst);
    }
    out.reverse();
    out.truncate(MAX_BURSTS);
    out
}

impl StatsReader {
    /// Upstream status, errors and latency for `tenant` over `[from_ms, to_ms)`.
    pub fn upstream_health(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<UpstreamReport, StatsError> {
        let bucket_ms = bucket_for(to_ms.saturating_sub(from_ms));
        let mut stmt = self.conn.prepare_cached(
            "SELECT time_ms, request_id, upstream, model, status, error, upstream_ms, total_ms,
                    keep_warm,
                    CASE WHEN json_valid(timing_json)
                         THEN json_extract(timing_json, '$.attempts_total') END
             FROM proxy_requests
             WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3
             ORDER BY time_ms DESC, id DESC LIMIT ?4",
        )?;
        let mut rows: Vec<UpstreamRow> = stmt
            .query_map(
                params![tenant, from_ms, to_ms, (MAX_ROWS + 1) as i64],
                |row| {
                    Ok(UpstreamRow {
                        time_ms: row.get(0)?,
                        request_id: row.get(1)?,
                        upstream: row.get(2)?,
                        model: row.get(3)?,
                        status: row.get(4)?,
                        error: row.get(5)?,
                        upstream_ms: row.get(6)?,
                        total_ms: row.get(7)?,
                        keep_warm: row.get::<_, i64>(8)? != 0,
                        attempts: row.get::<_, Option<i64>>(9)?.unwrap_or(1),
                    })
                },
            )?
            .collect::<Result<_, _>>()?;
        let truncated = rows.len() > MAX_ROWS;
        rows.truncate(MAX_ROWS);
        rows.reverse();

        let mut totals = StatusMix::default();
        let mut all_upstream_ms = Vec::new();
        // Per (upstream, model): the mix, then upstream and total milliseconds.
        type Group = (StatusMix, Vec<f64>, Vec<f64>);
        let mut groups: BTreeMap<(String, String), Group> = BTreeMap::new();
        let mut timeline: BTreeMap<i64, (UpstreamPoint, Vec<f64>)> = BTreeMap::new();
        for row in &rows {
            totals.add(row);
            let group = groups
                .entry((row.upstream.clone(), row.model.clone().unwrap_or_default()))
                .or_default();
            group.0.add(row);
            if let Some(ms) = finite(row.upstream_ms) {
                group.1.push(ms);
                all_upstream_ms.push(ms);
            }
            if let Some(ms) = finite(row.total_ms) {
                group.2.push(ms);
            }
            let (point, ms) = timeline
                .entry(from_ms + ((row.time_ms - from_ms) / bucket_ms) * bucket_ms)
                .or_default();
            point.requests += 1;
            if row.failed() {
                point.failed += 1;
            }
            match row.status {
                Some(429) => point.rate_limited += 1,
                Some(s) if s >= 500 => point.server_error += 1,
                _ => {}
            }
            if let Some(v) = finite(row.upstream_ms) {
                ms.push(v);
            }
        }
        let failures: Vec<&UpstreamRow> = rows.iter().filter(|r| r.failed()).collect();
        let incidents: Vec<&UpstreamRow> = rows.iter().filter(|r| r.incident()).collect();
        let mut by_upstream: Vec<UpstreamGroup> = groups
            .into_iter()
            .map(|((upstream, model), (mix, up, total))| UpstreamGroup {
                upstream,
                model,
                error_share: mix.error_share(),
                mix,
                upstream_ms: percentiles(up),
                total_ms: percentiles(total),
            })
            .collect();
        by_upstream.sort_by(|a, b| {
            b.mix
                .requests
                .cmp(&a.mix.requests)
                .then(a.upstream.cmp(&b.upstream))
                .then(a.model.cmp(&b.model))
        });
        Ok(UpstreamReport {
            tenant: tenant.to_string(),
            from_ms,
            to_ms,
            bucket_ms,
            truncated,
            error_share: totals.error_share(),
            totals,
            upstream_ms: percentiles(all_upstream_ms),
            by_upstream,
            timeline: timeline
                .into_iter()
                .map(|(bucket_ms, (point, ms))| UpstreamPoint {
                    bucket_ms,
                    p95_upstream_ms: percentiles(ms).p95,
                    ..point
                })
                .collect(),
            bursts: bursts(&incidents),
            failures: failures
                .iter()
                .rev()
                .take(MAX_FAILURES)
                .map(|row| UpstreamFailure {
                    time_ms: row.time_ms,
                    request_id: row.request_id.clone(),
                    upstream: row.upstream.clone(),
                    model: row.model.clone(),
                    status: row.status,
                    error: row
                        .error
                        .as_ref()
                        .map(|e| e.chars().take(ERROR_CHARS).collect()),
                    attempts: row.attempts,
                    upstream_ms: finite(row.upstream_ms),
                    keep_warm: row.keep_warm,
                })
                .collect(),
        })
    }
}

// --------------------------------------------------------------- ref store

/// One ref-store entry's bookkeeping, as the caller read it from the store.
/// Times are in seconds, as the store keeps them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredRef {
    /// The handle, with or without the `k_` marker prefix.
    pub id: String,
    pub kind: String,
    pub bytes: u64,
    pub created_at: u64,
    pub last_accessed: u64,
    pub expires_at: u64,
    pub expired: bool,
}

/// A handle without its `k_` marker, the form the ref store keys on.
fn handle_body(handle: &str) -> &str {
    handle.strip_prefix("k_").unwrap_or(handle)
}

/// What retrieval asked of one handle in the window.
#[derive(Clone, Debug, Default)]
struct Asked {
    asks: u64,
    hits: u64,
    expired: u64,
    returned: i64,
    first_ms: i64,
}

impl StatsReader {
    fn asked_refs(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<HashMap<String, Asked>, StatsError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT ref, COUNT(*), SUM(found), SUM(expired),
                    SUM(CASE WHEN found = 1 AND withheld = 0 THEN returned_tokens ELSE 0 END),
                    MIN(time_ms)
             FROM retrieval_events
             WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3
             GROUP BY ref",
        )?;
        let rows = stmt.query_map(params![tenant, from_ms, to_ms], |row| {
            Ok((
                handle_body(&row.get::<_, String>(0)?).to_string(),
                Asked {
                    asks: row.get::<_, i64>(1)?.max(0) as u64,
                    hits: row.get::<_, i64>(2)?.max(0) as u64,
                    expired: row.get::<_, i64>(3)?.max(0) as u64,
                    returned: row.get(4)?,
                    first_ms: row.get(5)?,
                },
            ))
        })?;
        let mut out: HashMap<String, Asked> = HashMap::new();
        for row in rows {
            let (id, asked) = row?;
            // `k_x` and `x` are one handle.
            let entry = out.entry(id).or_insert(Asked {
                first_ms: asked.first_ms,
                ..Default::default()
            });
            entry.asks += asked.asks;
            entry.hits += asked.hits;
            entry.expired += asked.expired;
            entry.returned += asked.returned;
            entry.first_ms = entry.first_ms.min(asked.first_ms);
        }
        Ok(out)
    }
}

// ------------------------------------------------------------------- yield

/// What the engine removed from one content type.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TypeYield {
    pub content_type: String,
    /// Compressing runs with at least one message of this type.
    pub requests: u64,
    pub messages: u64,
    /// Tokens removed, each run's saving split across its types by message
    /// count. An estimate: the engine does not record tokens per type.
    pub saved_estimate: f64,
    /// Runs whose messages were all this type, and what they removed: the
    /// exact figure the estimate can be checked against.
    pub single_type_requests: u64,
    pub single_type_saved: i64,
}

/// What retrieval took back from one kind of fragment.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KindAskBack {
    pub kind: String,
    /// Entries of this kind stored during the window and still in the store.
    pub refs_stored: u64,
    pub bytes_stored: u64,
    /// Of those, entries asked for at least once in the window.
    pub refs_asked: u64,
    pub asked_share: Option<f64>,
    /// Every ask in the window for a handle of this kind.
    pub asks: u64,
    pub hits: u64,
    pub returned_tokens: i64,
    /// Median seconds from storing an entry to its first ask.
    pub median_first_ask_s: Option<i64>,
}

/// Compression yield by content type: what was removed and what came back.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct YieldReport {
    pub tenant: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub compressing_runs: u64,
    pub saved_gross: i64,
    pub retrieved_tokens: i64,
    pub types: Vec<TypeYield>,
    pub kinds: Vec<KindAskBack>,
    /// Asks for handles the store no longer holds, whose kind is unknown.
    pub unknown_kind: KindAskBack,
    pub truncated: bool,
    /// Whether the store census was cut short.
    pub store_truncated: bool,
}

impl StatsReader {
    /// Yield by content type for `tenant` over `[from_ms, to_ms)`, joined to
    /// `store`, the tenant's ref-store census.
    pub fn compression_yield(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
        store: &[StoredRef],
        store_truncated: bool,
    ) -> Result<YieldReport, StatsError> {
        let _snapshot = self.conn.unchecked_transaction()?;
        let mut report = YieldReport {
            tenant: tenant.to_string(),
            from_ms,
            to_ms,
            store_truncated,
            ..Default::default()
        };
        let mut types: BTreeMap<String, TypeYield> = BTreeMap::new();
        let mut stmt = self.conn.prepare_cached(
            "SELECT content_types, tokens_in - tokens_out
             FROM optimization_events
             WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3
               AND messages_compressed > 0 AND coalesce(outcome, '') <> 'discarded'
             ORDER BY time_ms DESC, id DESC LIMIT ?4",
        )?;
        let mut rows = stmt.query(params![tenant, from_ms, to_ms, (MAX_ROWS + 1) as i64])?;
        while let Some(row) = rows.next()? {
            if report.compressing_runs as usize == MAX_ROWS {
                report.truncated = true;
                break;
            }
            report.compressing_runs += 1;
            let saved: i64 = row.get(1)?;
            report.saved_gross += saved;
            let counts: BTreeMap<String, i64> =
                serde_json::from_str(&row.get::<_, String>(0)?).unwrap_or_default();
            let counts: Vec<(String, i64)> = counts.into_iter().filter(|(_, n)| *n > 0).collect();
            let messages: i64 = counts.iter().map(|(_, n)| n).sum();
            let single = counts.len() == 1;
            for (name, n) in counts {
                let entry = types.entry(name.clone()).or_insert_with(|| TypeYield {
                    content_type: name,
                    ..Default::default()
                });
                entry.requests += 1;
                entry.messages += n as u64;
                entry.saved_estimate += saved as f64 * n as f64 / messages as f64;
                if single {
                    entry.single_type_requests += 1;
                    entry.single_type_saved += saved;
                }
            }
        }
        drop(rows);
        let mut types: Vec<TypeYield> = types.into_values().collect();
        types.sort_by(|a, b| {
            b.saved_estimate
                .total_cmp(&a.saved_estimate)
                .then(a.content_type.cmp(&b.content_type))
        });
        report.types = types;

        let asked = self.asked_refs(tenant, from_ms, to_ms)?;
        report.retrieved_tokens = asked.values().map(|a| a.returned).sum();
        let by_id: HashMap<&str, &StoredRef> =
            store.iter().map(|r| (handle_body(&r.id), r)).collect();
        let (from_s, to_s) = (from_ms.div_euclid(1_000), to_ms.div_euclid(1_000));
        let mut kinds: BTreeMap<String, (KindAskBack, Vec<i64>)> = BTreeMap::new();
        for entry in store {
            let created = entry.created_at as i64;
            if created < from_s || created >= to_s {
                continue;
            }
            let (kind, waits) = kinds.entry(entry.kind.clone()).or_default();
            kind.refs_stored += 1;
            kind.bytes_stored += entry.bytes;
            if let Some(ask) = asked.get(handle_body(&entry.id)) {
                kind.refs_asked += 1;
                waits.push((ask.first_ms.div_euclid(1_000) - created).max(0));
            }
        }
        for (id, ask) in &asked {
            let target = match by_id.get(id.as_str()) {
                Some(entry) => &mut kinds.entry(entry.kind.clone()).or_default().0,
                None => {
                    // A purged handle has no stored entry to count it under
                    // `refs_asked` above, so it is counted here.
                    report.unknown_kind.refs_asked += 1;
                    &mut report.unknown_kind
                }
            };
            target.asks += ask.asks;
            target.hits += ask.hits;
            target.returned_tokens += ask.returned;
        }
        report.unknown_kind.kind = "unknown".into();
        let mut kinds: Vec<KindAskBack> = kinds
            .into_iter()
            .map(|(name, (mut kind, mut waits))| {
                kind.kind = name;
                kind.asked_share = (kind.refs_stored > 0)
                    .then(|| kind.refs_asked as f64 / kind.refs_stored as f64);
                waits.sort_unstable();
                kind.median_first_ask_s = waits.get(waits.len().saturating_sub(1) / 2).copied();
                kind
            })
            .collect();
        kinds.sort_by(|a, b| {
            b.returned_tokens
                .cmp(&a.returned_tokens)
                .then(b.refs_stored.cmp(&a.refs_stored))
                .then(a.kind.cmp(&b.kind))
        });
        report.kinds = kinds;
        Ok(report)
    }
}

// ------------------------------------------------------------ store health

/// Entries of one kind in the store now.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KindHeld {
    pub kind: String,
    pub refs: u64,
    pub bytes: u64,
    pub expired: u64,
    /// Live entries never read since they were stored.
    pub never_read: u64,
}

/// Live entries by how old they are, or how soon they expire.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AgeBucket {
    pub label: String,
    pub upto_s: Option<u64>,
    pub refs: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StorePoint {
    pub time_ms: i64,
    pub live_refs: u64,
    pub live_bytes: u64,
}

/// What the ref store holds and how retrieval is faring against it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StoreHealth {
    pub tenant: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub now_s: u64,
    pub live_refs: u64,
    pub live_bytes: u64,
    /// Entries past their window that no sweep has removed yet.
    pub expired_refs: u64,
    pub expired_bytes: u64,
    pub never_read_share: Option<f64>,
    pub by_kind: Vec<KindHeld>,
    pub age: Vec<AgeBucket>,
    pub expiring: Vec<AgeBucket>,
    /// The whole store's size over the window, every tenant together: the
    /// sampler does not split by tenant.
    pub samples: Vec<StorePoint>,
    /// Handles the engine minted in the window, and their bytes.
    pub minted_refs: u64,
    pub minted_bytes: u64,
    /// Distinct handles retrieval asked for in the window.
    pub asked_refs: u64,
    pub asked_share: Option<f64>,
    pub asks: u64,
    pub hits: u64,
    /// Asks that found the entry past its window.
    pub expired_asks: u64,
    /// Asks for a handle the store never had or had already dropped.
    pub missing_asks: u64,
    /// Asks refused by policy rather than by the store.
    pub withheld_asks: u64,
    pub store_truncated: bool,
}

/// Age buckets, in seconds.
pub const AGE_BUCKETS: &[(&str, u64)] = &[
    ("<5m", 300),
    ("5-30m", 1_800),
    ("30m-2h", 7_200),
    ("2-6h", 21_600),
    ("6-24h", 86_400),
    (">1d", u64::MAX),
];
/// Time-to-expiry buckets, in seconds.
pub const EXPIRY_BUCKETS: &[(&str, u64)] = &[
    ("<5m", 300),
    ("5-30m", 1_800),
    ("30m-2h", 7_200),
    (">2h", u64::MAX),
];
/// The most store-size samples drawn; more are thinned evenly.
pub const MAX_SAMPLES: usize = 240;

fn age_buckets(spec: &[(&str, u64)]) -> Vec<AgeBucket> {
    spec.iter()
        .map(|(label, upto)| AgeBucket {
            label: label.to_string(),
            upto_s: (*upto != u64::MAX).then_some(*upto),
            ..Default::default()
        })
        .collect()
}

fn file(buckets: &mut [AgeBucket], seconds: u64, bytes: u64) {
    if let Some(bucket) = buckets
        .iter_mut()
        .find(|b| b.upto_s.is_none_or(|upto| seconds < upto))
    {
        bucket.refs += 1;
        bucket.bytes += bytes;
    }
}

impl StatsReader {
    /// The ref store's health for `tenant`: `store` is its census, taken at
    /// `now_s`; retrieval and minting are read over `[from_ms, to_ms)`.
    pub fn store_health(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
        store: &[StoredRef],
        store_truncated: bool,
        now_s: u64,
    ) -> Result<StoreHealth, StatsError> {
        let _snapshot = self.conn.unchecked_transaction()?;
        let mut report = StoreHealth {
            tenant: tenant.to_string(),
            from_ms,
            to_ms,
            now_s,
            age: age_buckets(AGE_BUCKETS),
            expiring: age_buckets(EXPIRY_BUCKETS),
            store_truncated,
            ..Default::default()
        };
        let mut kinds: BTreeMap<String, KindHeld> = BTreeMap::new();
        let mut never_read = 0u64;
        for entry in store {
            let kind = kinds.entry(entry.kind.clone()).or_insert_with(|| KindHeld {
                kind: entry.kind.clone(),
                ..Default::default()
            });
            kind.refs += 1;
            kind.bytes += entry.bytes;
            if entry.expired {
                kind.expired += 1;
                report.expired_refs += 1;
                report.expired_bytes += entry.bytes;
                continue;
            }
            report.live_refs += 1;
            report.live_bytes += entry.bytes;
            if entry.last_accessed <= entry.created_at {
                kind.never_read += 1;
                never_read += 1;
            }
            file(
                &mut report.age,
                now_s.saturating_sub(entry.created_at),
                entry.bytes,
            );
            file(
                &mut report.expiring,
                entry.expires_at.saturating_sub(now_s),
                entry.bytes,
            );
        }
        report.never_read_share =
            (report.live_refs > 0).then(|| never_read as f64 / report.live_refs as f64);
        let mut by_kind: Vec<KindHeld> = kinds.into_values().collect();
        by_kind.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.kind.cmp(&b.kind)));
        report.by_kind = by_kind;

        let mut stmt = self.conn.prepare_cached(
            "SELECT time_ms, live_refs, live_bytes FROM store_samples
             WHERE time_ms >= ?1 AND time_ms < ?2 ORDER BY time_ms",
        )?;
        let samples: Vec<StorePoint> = stmt
            .query_map(params![from_ms, to_ms], |row| {
                Ok(StorePoint {
                    time_ms: row.get(0)?,
                    live_refs: row.get::<_, i64>(1)?.max(0) as u64,
                    live_bytes: row.get::<_, i64>(2)?.max(0) as u64,
                })
            })?
            .collect::<Result<_, _>>()?;
        let stride = samples.len().div_ceil(MAX_SAMPLES).max(1);
        let last = samples.len().saturating_sub(1);
        report.samples = samples
            .into_iter()
            .enumerate()
            .filter(|(i, _)| i % stride == 0 || *i == last)
            .map(|(_, point)| point)
            .collect();

        let (minted, minted_bytes): (i64, i64) = self.conn.query_row(
            "SELECT coalesce(SUM(refs_emitted), 0), coalesce(SUM(ref_bytes_emitted), 0)
             FROM optimization_events WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3",
            params![tenant, from_ms, to_ms],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        report.minted_refs = minted.max(0) as u64;
        report.minted_bytes = minted_bytes.max(0) as u64;
        let (asks, hits, expired, withheld): (i64, i64, i64, i64) = self.conn.query_row(
            "SELECT COUNT(*), coalesce(SUM(found), 0), coalesce(SUM(expired), 0),
                    coalesce(SUM(withheld), 0)
             FROM retrieval_events WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3",
            params![tenant, from_ms, to_ms],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        report.asks = asks.max(0) as u64;
        report.hits = hits.max(0) as u64;
        report.expired_asks = expired.max(0) as u64;
        report.withheld_asks = withheld.max(0) as u64;
        // A withheld ask is filed as not found; it is not a missing entry.
        report.missing_asks = report
            .asks
            .saturating_sub(report.hits)
            .saturating_sub(report.expired_asks)
            .saturating_sub(report.withheld_asks);
        report.asked_refs = self.asked_refs(tenant, from_ms, to_ms)?.len() as u64;
        report.asked_share = (report.minted_refs > 0)
            .then(|| (report.asked_refs as f64 / report.minted_refs as f64).min(1.0));
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_calendar_round_trips_and_buckets_start_where_they_should() {
        for days in [-1_000, 0, 59, 60, 365, 11_016, 20_406, 20_771] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(utc_date(0), "1970-01-01");
        // 2026-11-14 is a Saturday.
        let saturday = days_from_civil(2026, 11, 14) * DAY_MS + 5_000;
        assert_eq!(utc_date(TrendBucket::Week.start(saturday)), "2026-11-09");
        assert_eq!(utc_date(TrendBucket::Month.start(saturday)), "2026-11-01");
        assert_eq!(utc_date(TrendBucket::Day.start(saturday)), "2026-11-14");
        assert_eq!(
            utc_date(TrendBucket::Month.next(TrendBucket::Month.start(saturday))),
            "2026-12-01"
        );
        let december = days_from_civil(2026, 12, 3) * DAY_MS;
        assert_eq!(
            utc_date(TrendBucket::Month.next(december - 2 * DAY_MS)),
            "2027-01-01"
        );
        // Saturday the 14th to Monday the 23rd: three Monday-started weeks.
        let spans = TrendBucket::Week.spans(saturday, saturday + 9 * DAY_MS);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].0, TrendBucket::Week.start(saturday));
        assert_eq!(
            spans[2].1,
            saturday + 9 * DAY_MS,
            "the last span stops at `to`"
        );
        assert_eq!(TrendBucket::parse("month"), Some(TrendBucket::Month));
        assert_eq!(TrendBucket::parse("year"), None);
    }

    #[test]
    fn the_dominant_type_is_the_one_with_most_messages() {
        assert_eq!(dominant_type(r#"{"json":3,"text":1}"#), "json");
        assert_eq!(dominant_type(r#"{"json":2,"log":2}"#), "json");
        assert_eq!(dominant_type("{}"), "none");
        assert_eq!(dominant_type("not json"), "none");
    }

    fn row(time_ms: i64, status: Option<i64>, upstream: &str) -> UpstreamRow {
        UpstreamRow {
            time_ms,
            request_id: format!("r{time_ms}"),
            upstream: upstream.into(),
            model: None,
            status,
            error: None,
            upstream_ms: None,
            total_ms: None,
            keep_warm: false,
            attempts: 1,
        }
    }

    #[test]
    fn failures_close_together_make_a_burst_and_stragglers_do_not() {
        let rows = [
            row(0, Some(429), "a"),
            row(30_000, Some(429), "a"),
            row(80_000, None, "b"),
            row(500_000, Some(503), "a"),
            row(900_000, Some(503), "a"),
            row(910_000, Some(503), "a"),
        ];
        let refs: Vec<&UpstreamRow> = rows.iter().collect();
        let found = bursts(&refs);
        assert_eq!(found.len(), 1, "only the first three are a burst");
        assert_eq!(
            (found[0].from_ms, found[0].to_ms, found[0].failures),
            (0, 80_000, 3)
        );
        assert_eq!(found[0].statuses.get("429"), Some(&2));
        assert_eq!(found[0].statuses.get("none"), Some(&1));
        assert_eq!(found[0].upstreams, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn a_status_mix_counts_each_class_once() {
        let mut mix = StatusMix::default();
        for status in [Some(200), Some(429), Some(404), Some(502), None] {
            mix.add(&row(0, status, "a"));
        }
        let mut retried = row(0, Some(200), "a");
        retried.attempts = 2;
        retried.error = Some("stream cut".into());
        mix.add(&retried);
        assert_eq!(
            (
                mix.requests,
                mix.ok,
                mix.rate_limited,
                mix.client_error,
                mix.server_error,
                mix.no_status
            ),
            (6, 2, 1, 1, 1, 1)
        );
        assert_eq!((mix.failed, mix.retried), (5, 1));
    }
}

#[cfg(test)]
#[path = "dashboards_tests.rs"]
mod db_tests;
