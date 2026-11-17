//! What the proxy relays, and which of its "errors" are errors.
//!
//! A proxy in front of an agent relays far more than model calls: profile and
//! settings fetches, telemetry, conditional `GET`s that come back `304`,
//! WebSocket upgrades that answer `101`. Counted naively — any status outside
//! 2xx, or any error text — those swamp the figure that matters, and a
//! dashboard that shows a 6 % error rate when the model calls all succeeded is
//! a dashboard nobody trusts.
//!
//! Every `proxy_requests` row in the window is put in one **class**:
//!
//! - `model`: a model call the proxy understood (any API family but
//!   `passthrough`), not a keep-warm ping, not a WebSocket upgrade;
//! - `keep_warm`: a ping the proxy sent itself;
//! - `websocket`: an upgrade the upstream answered `101`;
//! - `passthrough`: everything else the proxy relayed unread.
//!
//! and one **outcome**, which falls in one of three **kinds**:
//!
//! - `expected`: `ok` (2xx), `not_modified` (304), `redirect` (other 3xx),
//!   `switching` (101) — not failures, whatever a status-class count says;
//! - `client`: `refused` (4xx other than 429) and `client_disconnect` (the
//!   client hung up mid-stream) — the request or the client, not the upstream;
//! - `upstream`: `rate_limited` (429), `server_error` (5xx), `no_answer` (no
//!   status at all) and `broken` (a 2xx whose stream then failed).
//!
//! The *real* error share is the `upstream` kind over the model calls and
//! pings; the *naive* one is what a status-class count would report over every
//! row. Both are given so the gap is visible.
//!
//! One grouped query, so a busy window is never truncated.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::live::bucket_for;
use crate::{StatsError, StatsReader};

/// Endpoint groups and error messages listed, most frequent first.
pub const MAX_ENDPOINTS: usize = 40;
pub const MAX_ERRORS: usize = 15;
/// Characters of an error message kept when grouping.
const ERROR_CHARS: usize = 90;

pub const CLASSES: [&str; 4] = ["model", "keep_warm", "websocket", "passthrough"];
pub const OUTCOMES: [&str; 11] = [
    "ok",
    "not_modified",
    "redirect",
    "switching",
    "refused",
    "client_disconnect",
    "rate_limited",
    "server_error",
    "no_answer",
    "broken",
    "other",
];

/// The class of a row. See the module documentation.
pub fn class_of(api_family: &str, keep_warm: bool, status: Option<i64>) -> &'static str {
    if keep_warm {
        "keep_warm"
    } else if status == Some(101) {
        "websocket"
    } else if api_family == "passthrough" {
        "passthrough"
    } else {
        "model"
    }
}

/// The outcome of a row. `error` is the error text, if any.
pub fn outcome_of(status: Option<i64>, error: Option<&str>) -> &'static str {
    if let Some(error) = error {
        if error.starts_with("client disconnected") {
            return "client_disconnect";
        }
        if status.is_some_and(|s| (200..300).contains(&s)) {
            return "broken";
        }
    }
    match status {
        None => "no_answer",
        Some(101) => "switching",
        Some(304) => "not_modified",
        Some(429) => "rate_limited",
        Some(s) if (200..300).contains(&s) => "ok",
        Some(s) if (300..400).contains(&s) => "redirect",
        Some(s) if (400..500).contains(&s) => "refused",
        Some(s) if s >= 500 => "server_error",
        Some(_) => "other",
    }
}

/// The kind an outcome falls in: `expected`, `client` or `upstream`.
pub fn kind_of(outcome: &str) -> &'static str {
    match outcome {
        "ok" | "not_modified" | "redirect" | "switching" => "expected",
        "refused" | "client_disconnect" | "other" => "client",
        _ => "upstream",
    }
}

/// Whether a naive status-class count would call the row a failure.
fn naive_failure(status: Option<i64>, has_error: bool) -> bool {
    has_error || status.is_none_or(|s| !(200..300).contains(&s))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ClassTraffic {
    pub class: String,
    pub requests: u64,
    pub share: Option<f64>,
    /// Rows per outcome; outcomes with none are left out.
    pub outcomes: BTreeMap<String, u64>,
    pub expected: u64,
    pub client: u64,
    pub upstream: u64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
}

/// One upstream, API family and status, with how many rows came back so.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EndpointTraffic {
    pub class: String,
    pub upstream: String,
    pub api_family: String,
    pub status: Option<i64>,
    pub outcome: String,
    pub kind: String,
    pub requests: u64,
    pub last_ms: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ErrorMessage {
    pub message: String,
    pub outcome: String,
    pub requests: u64,
    pub last_ms: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TrafficPoint {
    pub bucket_ms: i64,
    pub model: u64,
    pub keep_warm: u64,
    pub websocket: u64,
    pub passthrough: u64,
    pub client: u64,
    pub upstream: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TrafficReport {
    pub tenant: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub bucket_ms: i64,
    pub requests: u64,
    pub classes: Vec<ClassTraffic>,
    /// Rows a status-class count would call failed, over every row.
    pub naive_failures: u64,
    pub naive_error_share: Option<f64>,
    /// Upstream-kind outcomes of model calls and pings, over those rows.
    pub real_failures: u64,
    pub real_error_share: Option<f64>,
    /// Client-kind outcomes over every row.
    pub client_failures: u64,
    /// Expected rows outside 2xx — 304s, 101s, other 3xx — over every row:
    /// what a status-class count wrongly calls trouble.
    pub noise: u64,
    pub noise_share: Option<f64>,
    pub endpoints: Vec<EndpointTraffic>,
    pub endpoints_total: u64,
    pub errors: Vec<ErrorMessage>,
    pub timeline: Vec<TrafficPoint>,
}

fn share(part: u64, of: u64) -> Option<f64> {
    (of > 0).then(|| part as f64 / of as f64)
}

/// Error text shortened for grouping, digits folded so request ids and
/// byte counts do not split one message into many.
fn error_key(error: &str) -> String {
    let mut out = String::new();
    let mut last_digit = false;
    for c in error.chars() {
        if out.chars().count() >= ERROR_CHARS {
            out.push('…');
            break;
        }
        if c.is_ascii_digit() {
            if !last_digit {
                out.push('#');
            }
            last_digit = true;
        } else {
            out.push(c);
            last_digit = false;
        }
    }
    out
}

impl StatsReader {
    /// What the proxy relayed for `tenant` over `[from_ms, to_ms)`.
    pub fn traffic(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<TrafficReport, StatsError> {
        let _snapshot = self.conn.unchecked_transaction()?;
        let bucket_ms = bucket_for(to_ms.saturating_sub(from_ms));
        let mut report = TrafficReport {
            tenant: tenant.to_string(),
            from_ms,
            to_ms,
            bucket_ms,
            ..Default::default()
        };
        let mut classes: HashMap<&'static str, ClassTraffic> = CLASSES
            .iter()
            .map(|c| {
                (
                    *c,
                    ClassTraffic {
                        class: c.to_string(),
                        ..Default::default()
                    },
                )
            })
            .collect();
        let mut endpoints: HashMap<
            (&'static str, String, String, Option<i64>, &'static str),
            EndpointTraffic,
        > = HashMap::new();
        let mut errors: HashMap<(String, &'static str), ErrorMessage> = HashMap::new();
        let mut timeline: BTreeMap<i64, TrafficPoint> = BTreeMap::new();
        let mut model_and_pings = 0_u64;

        // Grouped in SQL; the error text stays in the key only when there is
        // one, so ordinary rows collapse to a handful of groups.
        let mut stmt = self.conn.prepare_cached(
            "SELECT (time_ms - ?2) / ?4, api_family, keep_warm, status, upstream, error,
                    COUNT(*), SUM(COALESCE(prompt_tokens, 0)), SUM(COALESCE(completion_tokens, 0)),
                    MAX(time_ms)
               FROM proxy_requests
              WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3
              GROUP BY 1, 2, 3, 4, 5, 6",
        )?;
        let rows = stmt.query_map((tenant, from_ms, to_ms, bucket_ms), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? != 0,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, i64>(6)? as u64,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
            ))
        })?;
        for row in rows {
            let (
                bucket,
                api_family,
                keep_warm,
                status,
                upstream,
                error,
                n,
                prompt,
                completion,
                last,
            ) = row?;
            let class = class_of(&api_family, keep_warm, status);
            let outcome = outcome_of(status, error.as_deref());
            let kind = kind_of(outcome);

            report.requests += n;
            if naive_failure(status, error.is_some()) {
                report.naive_failures += n;
            }
            if kind == "expected" && outcome != "ok" {
                report.noise += n;
            }
            if kind == "client" {
                report.client_failures += n;
            }
            if matches!(class, "model" | "keep_warm") {
                model_and_pings += n;
                if kind == "upstream" {
                    report.real_failures += n;
                }
            }

            let c = classes.get_mut(class).expect("every class is listed");
            c.requests += n;
            *c.outcomes.entry(outcome.to_string()).or_default() += n;
            match kind {
                "expected" => c.expected += n,
                "client" => c.client += n,
                _ => c.upstream += n,
            }
            c.prompt_tokens += prompt;
            c.completion_tokens += completion;

            let e = endpoints
                .entry((class, upstream.clone(), api_family.clone(), status, outcome))
                .or_insert_with(|| EndpointTraffic {
                    class: class.to_string(),
                    upstream,
                    api_family,
                    status,
                    outcome: outcome.to_string(),
                    kind: kind.to_string(),
                    ..Default::default()
                });
            e.requests += n;
            e.last_ms = e.last_ms.max(last);

            if let Some(error) = error {
                let m = errors
                    .entry((error_key(&error), outcome))
                    .or_insert_with(|| ErrorMessage {
                        message: error_key(&error),
                        outcome: outcome.to_string(),
                        ..Default::default()
                    });
                m.requests += n;
                m.last_ms = m.last_ms.max(last);
            }

            let point = timeline.entry(from_ms + bucket * bucket_ms).or_default();
            match class {
                "model" => point.model += n,
                "keep_warm" => point.keep_warm += n,
                "websocket" => point.websocket += n,
                _ => point.passthrough += n,
            }
            match kind {
                "client" => point.client += n,
                "upstream" => point.upstream += n,
                _ => {}
            }
        }

        report.naive_error_share = share(report.naive_failures, report.requests);
        report.real_error_share = share(report.real_failures, model_and_pings);
        report.noise_share = share(report.noise, report.requests);
        report.classes = CLASSES
            .iter()
            .filter_map(|c| classes.remove(c))
            .map(|mut c| {
                c.share = share(c.requests, report.requests);
                c
            })
            .collect();
        let mut list: Vec<EndpointTraffic> = endpoints.into_values().collect();
        list.sort_by(|a, b| {
            b.requests
                .cmp(&a.requests)
                .then(b.last_ms.cmp(&a.last_ms))
                .then(a.upstream.cmp(&b.upstream))
        });
        report.endpoints_total = list.len() as u64;
        list.truncate(MAX_ENDPOINTS);
        report.endpoints = list;
        let mut list: Vec<ErrorMessage> = errors.into_values().collect();
        list.sort_by(|a, b| b.requests.cmp(&a.requests).then(b.last_ms.cmp(&a.last_ms)));
        list.truncate(MAX_ERRORS);
        report.errors = list;
        report.timeline = timeline
            .into_iter()
            .map(|(bucket_ms, point)| TrafficPoint { bucket_ms, ..point })
            .collect();
        Ok(report)
    }
}

#[cfg(test)]
#[path = "traffic_tests.rs"]
mod tests;
