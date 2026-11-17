//! One verdict on whether kompressor is healthy, from the reports the other
//! dashboards already draw.
//!
//! Nothing here reads a table the upstream, engine and store-health reports do
//! not; it reuses those reports and judges each figure against a threshold,
//! so the health page and the detail page it links to always agree. The only
//! extra read is when the tenant was last seen, one indexed `MAX`.
//!
//! Figures the database cannot hold — engine slots busy right now, events the
//! statistics queue dropped, the store's byte cap — come from the caller.

use serde::{Deserialize, Serialize};

use crate::dashboards::{
    EnginePoint, EngineReport, FailureBurst, StoreHealth, StoredRef, UpstreamFailure,
    UpstreamPoint, UpstreamReport,
};
use crate::{StatsError, StatsReader};

/// Fewer samples than this and a share is reported, not judged: two failures
/// out of five requests says little about the upstream.
pub const MIN_SAMPLES: u64 = 20;
/// An incident that ended this recently is still "now".
pub const RECENT_INCIDENT_MS: i64 = 15 * 60_000;
/// Incidents and failures the page lists, newest first.
pub const MAX_INCIDENTS: usize = 5;
pub const MAX_FAILURES: usize = 10;

/// How a check came out. `Info` is reported without a judgement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Info,
    #[default]
    Ok,
    Warn,
    Bad,
}

/// How a check's value reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Unit {
    Share,
    Ms,
    Count,
    Bytes,
}

/// One judged figure.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Check {
    pub id: String,
    /// The group it is drawn in: upstream, engine, retrieval, store, data.
    pub area: String,
    pub title: String,
    pub level: Level,
    pub value: Option<f64>,
    pub unit: Unit,
    /// Where the value comes from, in words: "12 of 4,210 requests".
    pub detail: String,
    /// The value at which the check turns warn and bad, when it has them.
    pub warn: Option<f64>,
    pub bad: Option<f64>,
    /// Whether the check turns warn and bad *below* its limits — a coverage
    /// that should be high — rather than at or above them.
    #[serde(default)]
    pub below: bool,
    /// The dashboard that explains the figure.
    pub view: String,
}

/// What [`assess`] needs besides the reports: what only the process knows.
#[derive(Clone, Debug, Default)]
pub struct Runtime {
    pub now_ms: i64,
    pub stats_dropped: u64,
    pub inflight_limit: usize,
    pub inflight_free: usize,
    /// The ref store's byte cap; 0 when it has none.
    pub store_max_bytes: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HealthReport {
    pub tenant: String,
    pub from_ms: i64,
    pub to_ms: i64,
    /// The worst judged level; `ok` when nothing could be judged.
    pub level: Level,
    pub checks: Vec<Check>,
    /// The newest proxied request and compression run of the tenant, at any
    /// time, not only in the window.
    pub last_request_ms: Option<i64>,
    pub last_compression_ms: Option<i64>,
    pub bucket_ms: i64,
    pub upstream_timeline: Vec<UpstreamPoint>,
    pub engine_timeline: Vec<EnginePoint>,
    pub incidents: Vec<FailureBurst>,
    pub failures: Vec<UpstreamFailure>,
    /// Whether any report read only the newest rows of a busy window.
    pub truncated: bool,
}

fn judge(value: f64, warn: f64, bad: Option<f64>) -> Level {
    if bad.is_some_and(|bad| value >= bad) {
        Level::Bad
    } else if value >= warn {
        Level::Warn
    } else {
        Level::Ok
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", group_digits(n), if n == 1 { one } else { many })
}

fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

struct Builder(Vec<Check>);

impl Builder {
    /// A share of `of`, judged only once there are [`MIN_SAMPLES`].
    #[allow(clippy::too_many_arguments)]
    fn share(
        &mut self,
        id: &str,
        area: &str,
        title: &str,
        part: u64,
        of: u64,
        noun: (&str, &str),
        warn: f64,
        bad: Option<f64>,
        view: &str,
    ) {
        let value = (of > 0).then(|| part as f64 / of as f64);
        let level = match value {
            Some(v) if of >= MIN_SAMPLES => judge(v, warn, bad),
            _ => Level::Info,
        };
        let mut detail = format!("{} of {}", group_digits(part), plural(of, noun.0, noun.1));
        if of < MIN_SAMPLES {
            detail.push_str(" — too few to judge");
        }
        self.push(
            id,
            area,
            title,
            level,
            value,
            Unit::Share,
            detail,
            (Some(warn), bad),
            view,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        id: &str,
        area: &str,
        title: &str,
        level: Level,
        value: Option<f64>,
        unit: Unit,
        detail: String,
        (warn, bad): (Option<f64>, Option<f64>),
        view: &str,
    ) {
        self.0.push(Check {
            id: id.into(),
            area: area.into(),
            title: title.into(),
            level,
            value,
            unit,
            detail,
            warn,
            bad,
            below: false,
            view: view.into(),
        });
    }
}

/// Judge the window's reports. Pure, so the thresholds can be tested
/// without a database.
pub fn assess(
    upstream: &UpstreamReport,
    engine: &EngineReport,
    store: &StoreHealth,
    runtime: &Runtime,
) -> HealthReport {
    let mut b = Builder(Vec::new());
    let t = &upstream.totals;

    // ---- upstream
    b.share(
        "upstream_incidents",
        "upstream",
        "Upstream incidents",
        t.rate_limited + t.server_error + t.no_status,
        t.requests,
        ("request", "requests"),
        0.01,
        Some(0.05),
        "upstream",
    );
    if let Some(last) = upstream.bursts.first() {
        let ago = runtime.now_ms - last.to_ms;
        let level = if ago <= RECENT_INCIDENT_MS {
            Level::Bad
        } else {
            Level::Warn
        };
        let statuses: Vec<String> = last
            .statuses
            .iter()
            .map(|(status, n)| format!("{status} × {n}"))
            .collect();
        b.push(
            "incident_bursts",
            "upstream",
            "Failure bursts",
            level,
            Some(upstream.bursts.len() as f64),
            Unit::Count,
            format!(
                "latest ended {} min ago: {}",
                (ago.max(0) / 60_000),
                statuses.join(", ")
            ),
            (None, None),
            "upstream",
        );
    } else {
        b.push(
            "incident_bursts",
            "upstream",
            "Failure bursts",
            Level::Ok,
            Some(0.0),
            Unit::Count,
            "no run of 429, 5xx or unanswered requests".into(),
            (None, None),
            "upstream",
        );
    }
    b.share(
        "client_errors",
        "upstream",
        "Refused requests (4xx)",
        t.client_error,
        t.requests,
        ("request", "requests"),
        0.05,
        Some(0.20),
        "upstream",
    );
    b.share(
        "retries",
        "upstream",
        "Retried requests",
        t.retried,
        t.requests,
        ("request", "requests"),
        0.02,
        Some(0.10),
        "upstream",
    );
    b.push(
        "upstream_p95",
        "upstream",
        "Upstream p95",
        Level::Info,
        upstream.upstream_ms.p95,
        Unit::Ms,
        format!(
            "p50 {} over {}; model time, not judged",
            upstream
                .upstream_ms
                .p50
                .map_or_else(|| "–".to_string(), |v| format!("{v:.0} ms")),
            plural(upstream.upstream_ms.samples, "request", "requests")
        ),
        (None, None),
        "upstream",
    );

    // ---- engine
    let engine_level = match engine.latency.p95 {
        Some(p95) if engine.runs >= MIN_SAMPLES => judge(p95, 500.0, Some(2_000.0)),
        _ => Level::Info,
    };
    b.push(
        "engine_p95",
        "engine",
        "Engine p95",
        engine_level,
        engine.latency.p95,
        Unit::Ms,
        format!(
            "{}; max {}",
            plural(engine.runs, "run", "runs"),
            engine
                .latency
                .max
                .map_or_else(|| "–".to_string(), |v| format!("{v:.0} ms"))
        ),
        (Some(500.0), Some(2_000.0)),
        "engine",
    );
    if runtime.inflight_limit > 0 {
        let busy = runtime.inflight_limit.saturating_sub(runtime.inflight_free);
        let share = busy as f64 / runtime.inflight_limit as f64;
        b.push(
            "engine_slots",
            "engine",
            "Engine slots busy now",
            judge(share, 0.75, Some(0.95)),
            Some(share),
            Unit::Share,
            format!("{busy} of {}", runtime.inflight_limit),
            (Some(0.75), Some(0.95)),
            "engine",
        );
    }

    // ---- retrieval
    let asks = store.asks;
    b.share(
        "retrieval_misses",
        "retrieval",
        "Retrieval misses",
        store.expired_asks + store.missing_asks,
        asks,
        ("ask", "asks"),
        0.05,
        Some(0.20),
        "store",
    );
    b.share(
        "retrieval_withheld",
        "retrieval",
        "Withheld by budget",
        store.withheld_asks,
        asks,
        ("ask", "asks"),
        0.30,
        None,
        "store",
    );

    // ---- store
    // The cap is the whole store's, so compare it with the sampler's
    // all-tenant size; fall back to this tenant's census before any sample.
    let held = store
        .samples
        .last()
        .map_or(store.live_bytes, |s| s.live_bytes);
    if runtime.store_max_bytes > 0 {
        let share = held as f64 / runtime.store_max_bytes as f64;
        b.push(
            "store_fill",
            "store",
            "Ref store fill",
            judge(share, 0.80, Some(0.95)),
            Some(share),
            Unit::Share,
            format!(
                "{} of {} bytes; past the cap the least recently read are evicted",
                group_digits(held),
                group_digits(runtime.store_max_bytes)
            ),
            (Some(0.80), Some(0.95)),
            "store",
        );
    } else {
        b.push(
            "store_fill",
            "store",
            "Ref store size",
            Level::Info,
            Some(held as f64),
            Unit::Bytes,
            "no byte cap configured".into(),
            (None, None),
            "store",
        );
    }

    // ---- data
    b.push(
        "stats_dropped",
        "data",
        "Dropped statistics",
        if runtime.stats_dropped > 0 {
            Level::Warn
        } else {
            Level::Ok
        },
        Some(runtime.stats_dropped as f64),
        Unit::Count,
        if runtime.stats_dropped > 0 {
            "events lost since start; every figure is an undercount".into()
        } else {
            "none since start".into()
        },
        (Some(1.0), None),
        "engine",
    );
    let truncated = upstream.truncated || engine.truncated || store.store_truncated;
    b.push(
        "coverage",
        "data",
        "Rows read",
        if truncated { Level::Warn } else { Level::Ok },
        None,
        Unit::Count,
        if truncated {
            "a report read only the newest rows; pick a shorter window".into()
        } else {
            "every row in the window".into()
        },
        (None, None),
        "trends",
    );

    let checks = b.0;
    let level = checks
        .iter()
        .map(|c| c.level)
        .filter(|l| *l != Level::Info)
        .max()
        .unwrap_or(Level::Ok);
    HealthReport {
        tenant: upstream.tenant.clone(),
        from_ms: upstream.from_ms,
        to_ms: upstream.to_ms,
        level,
        checks,
        last_request_ms: None,
        last_compression_ms: None,
        bucket_ms: upstream.bucket_ms,
        upstream_timeline: upstream.timeline.clone(),
        engine_timeline: engine.timeline.clone(),
        incidents: upstream
            .bursts
            .iter()
            .take(MAX_INCIDENTS)
            .cloned()
            .collect(),
        failures: upstream
            .failures
            .iter()
            .take(MAX_FAILURES)
            .cloned()
            .collect(),
        truncated,
    }
}

impl StatsReader {
    /// The health verdict for `tenant` over `[from_ms, to_ms)`. `store` is the
    /// tenant's ref-store census, as for [`StatsReader::store_health`].
    pub fn health(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
        store: &[StoredRef],
        store_truncated: bool,
        runtime: &Runtime,
    ) -> Result<HealthReport, StatsError> {
        let upstream = self.upstream_health(tenant, from_ms, to_ms)?;
        let engine = self.engine(tenant, from_ms, to_ms)?;
        let now_s = (runtime.now_ms / 1_000).max(0) as u64;
        let store = self.store_health(tenant, from_ms, to_ms, store, store_truncated, now_s)?;
        let mut report = assess(&upstream, &engine, &store, runtime);
        report.last_request_ms = self.conn.query_row(
            "SELECT MAX(time_ms) FROM proxy_requests WHERE tenant = ?1 AND keep_warm = 0",
            [tenant],
            |row| row.get(0),
        )?;
        report.last_compression_ms = self.conn.query_row(
            "SELECT MAX(time_ms) FROM optimization_events WHERE tenant = ?1",
            [tenant],
            |row| row.get(0),
        )?;
        Ok(report)
    }
}

#[cfg(test)]
#[path = "health_tests.rs"]
mod tests;
