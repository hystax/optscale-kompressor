//! Daily experiment economy. Raw events and their archived copies are disjoint;
//! conversation identities use set union rather than adding daily counts.
use crate::{StatsError, StatsReader, cache::rates, context::RunContext};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
const DAY: i64 = 86_400_000;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct HistoryFilter {
    pub model: Option<String>,
    pub mode: Option<String>,
    pub surface: Option<String>,
    pub experiment_id: Option<String>,
    pub run_id: Option<String>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
pub struct Dimension {
    pub model: String,
    pub mode: String,
    pub surface: String,
    pub build_version: String,
    pub config_fingerprint: String,
    pub experiment_id: String,
    pub run_id: String,
    pub settings_json: String,
}
impl Dimension {
    fn matches(&self, f: &HistoryFilter) -> bool {
        [
            (f.model.as_ref(), &self.model),
            (f.mode.as_ref(), &self.mode),
            (f.surface.as_ref(), &self.surface),
            (f.experiment_id.as_ref(), &self.experiment_id),
            (f.run_id.as_ref(), &self.run_id),
        ]
        .iter()
        .all(|(wanted, actual)| wanted.is_none_or(|w| w == *actual))
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct HistoryMetrics {
    pub client_requests: u64,
    pub compressions: u64,
    pub saved_gross: i64,
    pub retrieved_tokens: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub usage_rows: u64,
    pub ping_requests: u64,
    pub input_equivalent_cost: f64,
    pub ping_input_equivalent_cost: f64,
    pub errors: u64,
    pub retrieval_rounds: i64,
    pub latency_samples: u64,
    pub latency_ms_total: f64,
}
impl HistoryMetrics {
    pub fn add(&mut self, other: &Self) {
        macro_rules! add {($($f:ident),*)=>{$(self.$f+=other.$f;)*};}
        add!(
            client_requests,
            compressions,
            saved_gross,
            retrieved_tokens,
            input_tokens,
            output_tokens,
            usage_rows,
            ping_requests,
            input_equivalent_cost,
            ping_input_equivalent_cost,
            errors,
            retrieval_rounds,
            latency_samples,
            latency_ms_total
        );
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryRow {
    pub dimension: Dimension,
    pub metrics: HistoryMetrics,
    pub conversations: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistorySummary {
    pub from_ms: i64,
    pub to_ms: i64,
    pub metrics: HistoryMetrics,
    pub conversations: usize,
    pub net_reduction: i64,
    pub net_per_client: Option<f64>,
    pub input_per_client: Option<f64>,
    pub input_per_conversation: Option<f64>,
    pub error_share: Option<f64>,
    pub mean_latency_ms: Option<f64>,
    pub rows: Vec<HistoryRow>,
    pub archived_partial_days_omitted: usize,
}
type Groups = BTreeMap<Dimension, (HistoryMetrics, BTreeSet<String>)>;
fn dimension(
    model: Option<String>,
    mode: Option<String>,
    surface: String,
    ctx: Option<String>,
) -> Result<Dimension, StatsError> {
    let c: Option<RunContext> = ctx.as_deref().map(serde_json::from_str).transpose()?;
    let effective_mode = mode.or_else(|| {
        c.as_ref().and_then(|c| {
            if surface == "proxy"
                && c.settings.pointer("/proxy/compress") == Some(&serde_json::Value::Bool(false))
            {
                Some("off".into())
            } else {
                c.settings
                    .pointer("/engine/mode")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            }
        })
    });
    let mut d = Dimension {
        model: model.unwrap_or_else(|| "unrecorded".into()),
        mode: effective_mode.unwrap_or_else(|| "unrecorded".into()),
        surface,
        ..Default::default()
    };
    if let Some(c) = c {
        d.build_version = c.build_version;
        d.config_fingerprint = c.config_fingerprint;
        d.experiment_id = c.experiment_id.unwrap_or_default();
        d.run_id = c.run_id;
        d.settings_json = serde_json::to_string(&c.settings)?;
    }
    Ok(d)
}

/// [`dimension`], remembered per distinct input. A run context is a few
/// kilobytes of JSON shared by every request of a run, so parsing and
/// re-serialising it per row was most of the cost of a long window; a window
/// holds a handful of distinct contexts, not one per row.
#[derive(Default)]
struct Dimensions(std::collections::HashMap<DimensionInput, Dimension>);

/// What [`dimension`] is computed from: model, mode, surface, run context.
type DimensionInput = (Option<String>, Option<String>, String, Option<String>);

impl Dimensions {
    fn get(
        &mut self,
        model: Option<String>,
        mode: Option<String>,
        surface: String,
        ctx: Option<String>,
    ) -> Result<Dimension, StatsError> {
        let key = (model, mode, surface, ctx);
        if let Some(found) = self.0.get(&key) {
            return Ok(found.clone());
        }
        let (model, mode, surface, ctx) = key.clone();
        let made = dimension(model, mode, surface, ctx)?;
        self.0.insert(key, made.clone());
        Ok(made)
    }
}

// Aggregating the three streams independently avoids multiplying usage by the
// number of optimization/retrieval rows sharing a request ID.
fn raw(
    conn: &Connection,
    tenant: &str,
    from: i64,
    to: i64,
    unarchived_only: bool,
) -> Result<Groups, StatsError> {
    let mut groups = Groups::new();
    let mut dimensions = Dimensions::default();
    let mut stmt=conn.prepare("SELECT coalesce(o.model,c.model),o.mode,o.surface,c.context,o.tokens_in-o.tokens_out,o.outcome
      FROM optimization_events o LEFT JOIN request_context c ON c.tenant=o.tenant AND c.request_id=o.request_id
      WHERE o.tenant=?1 AND o.time_ms>=?2 AND o.time_ms<?3 AND (?4=0 OR o.analysis_archived=0)")?;
    let mut rows = stmt.query(params![tenant, from, to, unarchived_only])?;
    while let Some(r) = rows.next()? {
        let d = dimensions.get(r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)?;
        let m = &mut groups.entry(d).or_default().0;
        m.compressions += 1;
        if r.get::<_, Option<String>>(5)?.as_deref() != Some("discarded") {
            m.saved_gross += r.get::<_, i64>(4)?;
        }
    }
    let mut stmt=conn.prepare("SELECT coalesce(p.model,c.model),coalesce(c.mode,(SELECT MAX(o.mode) FROM optimization_events o WHERE o.tenant=p.tenant AND o.request_id=p.request_id)),c.context,p.api_family,p.keep_warm,p.prompt_tokens,p.cache_read_tokens,p.cache_write_tokens,p.cache_write_1h_tokens,p.completion_tokens,p.status,p.error,p.total_ms,p.retrieve_rounds,p.conversation
      FROM proxy_requests p LEFT JOIN request_context c ON c.tenant=p.tenant AND c.request_id=p.request_id
      WHERE p.tenant=?1 AND p.time_ms>=?2 AND p.time_ms<?3 AND (?4=0 OR p.analysis_archived=0)")?;
    let mut rows = stmt.query(params![tenant, from, to, unarchived_only])?;
    while let Some(r) = rows.next()? {
        let d = dimensions.get(r.get(0)?, r.get(1)?, "proxy".into(), r.get(2)?)?;
        let rate = rates(&r.get::<_, String>(3)?, Some(&d.model));
        let (m, convs) = groups.entry(d).or_default();
        let ping = r.get::<_, bool>(4)?;
        if ping {
            m.ping_requests += 1;
        } else {
            m.client_requests += 1;
            if let Some(c) = r.get::<_, Option<String>>(14)? {
                convs.insert(c);
            }
            let status: Option<i64> = r.get(10)?;
            if status.is_none_or(|s| s >= 400) || r.get::<_, Option<String>>(11)?.is_some() {
                m.errors += 1;
            }
            m.retrieval_rounds += r.get::<_, i64>(13)?;
            if let Some(ms) = r
                .get::<_, Option<f64>>(12)?
                .filter(|ms| ms.is_finite() && *ms >= 0.0)
            {
                m.latency_samples += 1;
                m.latency_ms_total += ms;
            }
        }
        // Mirror provider-specific accounting: OpenAI prompt_tokens already
        // includes cache reads; Anthropic's input_tokens excludes reads/writes.
        if let Some(usage) = crate::cache::normalize_usage(
            &r.get::<_, String>(3)?,
            r.get(5)?,
            r.get(6)?,
            r.get(7)?,
            r.get(8)?,
        ) {
            let cost = usage.cost(rate);
            m.usage_rows += 1;
            m.input_tokens += usage.input();
            m.output_tokens += r.get::<_, Option<i64>>(9)?.unwrap_or(0);
            m.input_equivalent_cost += cost;
            if ping {
                m.ping_input_equivalent_cost += cost;
            }
        }
    }
    let mut stmt=conn.prepare("SELECT coalesce(c.model,(SELECT MAX(o.model) FROM optimization_events o WHERE o.tenant=r.tenant AND o.request_id=r.request_id),(SELECT MAX(p.model) FROM proxy_requests p WHERE p.tenant=r.tenant AND p.request_id=r.request_id)),coalesce(c.mode,(SELECT MAX(o.mode) FROM optimization_events o WHERE o.tenant=r.tenant AND o.request_id=r.request_id)),r.surface,coalesce(c.context,r.context_json),r.returned_tokens
      FROM retrieval_events r LEFT JOIN request_context c ON c.tenant=r.tenant AND c.request_id=r.request_id
      WHERE r.tenant=?1 AND r.time_ms>=?2 AND r.time_ms<?3 AND r.found=1 AND r.withheld=0 AND (?4=0 OR r.analysis_archived=0)")?;
    let mut rows = stmt.query(params![tenant, from, to, unarchived_only])?;
    while let Some(r) = rows.next()? {
        let d = dimensions.get(r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)?;
        groups.entry(d).or_default().0.retrieved_tokens += r.get::<_, i64>(4)?;
    }
    Ok(groups)
}
fn quotient(n: f64, d: u64) -> Option<f64> {
    (d > 0).then(|| n / d as f64)
}
fn summarize(from: i64, to: i64, groups: Groups, omitted: usize) -> HistorySummary {
    let mut total = HistoryMetrics::default();
    let mut convs = BTreeSet::new();
    let mut rows = Vec::new();
    for (dimension, (metrics, ids)) in groups {
        total.add(&metrics);
        convs.extend(ids.iter().cloned());
        rows.push(HistoryRow {
            dimension,
            metrics,
            conversations: ids.len(),
        });
    }
    let net = total.saved_gross - total.retrieved_tokens;
    HistorySummary {
        from_ms: from,
        to_ms: to,
        net_reduction: net,
        net_per_client: quotient(net as f64, total.client_requests),
        input_per_client: quotient(total.input_tokens as f64, total.client_requests),
        input_per_conversation: quotient(total.input_tokens as f64, convs.len() as u64),
        error_share: quotient(total.errors as f64, total.client_requests),
        mean_latency_ms: quotient(total.latency_ms_total, total.latency_samples),
        metrics: total,
        conversations: convs.len(),
        rows,
        archived_partial_days_omitted: omitted,
    }
}
impl StatsReader {
    pub fn history(
        &self,
        tenant: &str,
        from: i64,
        to: i64,
        filter: &HistoryFilter,
    ) -> Result<HistorySummary, StatsError> {
        let _snapshot = self.conn.unchecked_transaction()?;
        history_on(&self.conn, tenant, from, to, filter)
    }
}
pub(crate) fn history_on(
    conn: &Connection,
    tenant: &str,
    from: i64,
    to: i64,
    filter: &HistoryFilter,
) -> Result<HistorySummary, StatsError> {
    let mut groups = raw(conn, tenant, from, to, true)?;
    groups.retain(|d, _| d.matches(filter));
    let mut omitted = BTreeSet::new();
    let mut stmt=conn.prepare("SELECT day_ms,dimension,metrics FROM daily_analysis WHERE tenant=?1 AND day_ms<?3 AND day_ms+86400000>?2")?;
    let mut rows = stmt.query(params![tenant, from, to])?;
    while let Some(r) = rows.next()? {
        let day: i64 = r.get(0)?;
        let key: String = r.get(1)?;
        let d: Dimension = serde_json::from_str(&key)?;
        if !d.matches(filter) {
            continue;
        }
        if day < from || day + DAY > to {
            omitted.insert(day);
            continue;
        }
        let metrics: HistoryMetrics = serde_json::from_str(&r.get::<_, String>(2)?)?;
        let (m, convs) = groups.entry(d).or_default();
        m.add(&metrics);
        let mut cs=conn.prepare("SELECT conversation FROM daily_analysis_conversations WHERE tenant=?1 AND day_ms=?2 AND dimension=?3")?;
        for id in cs.query_map(params![tenant, day, key], |r| r.get::<_, String>(0))? {
            convs.insert(id?);
        }
    }
    Ok(summarize(from, to, groups, omitted.len()))
}

/// Called in the same transaction as retention deletion. Incremental addition
/// plus an event flag permits late events and makes repeated passes idempotent.
pub(crate) fn archive_before(conn: &Connection, cutoff: i64) -> Result<(), StatsError> {
    let mut stmt=conn.prepare("SELECT DISTINCT tenant,(time_ms-((time_ms%86400000)+86400000)%86400000) FROM (SELECT tenant,time_ms FROM optimization_events WHERE time_ms<?1 AND analysis_archived=0 UNION ALL SELECT tenant,time_ms FROM proxy_requests WHERE time_ms<?1 AND analysis_archived=0 UNION ALL SELECT tenant,time_ms FROM retrieval_events WHERE time_ms<?1 AND analysis_archived=0)")?;
    let days = stmt
        .query_map([cutoff], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (tenant, day) in days {
        for (d, (metrics, convs)) in raw(conn, &tenant, day, cutoff.min(day + DAY), true)? {
            let key = serde_json::to_string(&d)?;
            let existing:Option<String>=conn.query_row("SELECT metrics FROM daily_analysis WHERE tenant=?1 AND day_ms=?2 AND dimension=?3",params![tenant,day,key],|r|r.get(0)).optional()?;
            let mut merged: HistoryMetrics = existing
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?
                .unwrap_or_default();
            merged.add(&metrics);
            conn.execute("INSERT INTO daily_analysis(day_ms,tenant,dimension,metrics) VALUES(?1,?2,?3,?4) ON CONFLICT(day_ms,tenant,dimension) DO UPDATE SET metrics=excluded.metrics",params![day,tenant,key,serde_json::to_string(&merged)?])?;
            for id in convs {
                conn.execute("INSERT OR IGNORE INTO daily_analysis_conversations(day_ms,tenant,dimension,conversation) VALUES(?1,?2,?3,?4)",params![day,tenant,key,id])?;
            }
        }
        for table in ["optimization_events", "proxy_requests", "retrieval_events"] {
            conn.execute(&format!("UPDATE {table} SET analysis_archived=1 WHERE tenant=?1 AND time_ms>=?2 AND time_ms<?3"),params![tenant,day,cutoff.min(day+DAY)])?;
        }
    }
    Ok(())
}
use rusqlite::OptionalExtension;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetricChange {
    pub current: Option<f64>,
    pub baseline: Option<f64>,
    pub absolute: Option<f64>,
    pub relative: Option<f64>,
}
impl MetricChange {
    fn new(current: Option<f64>, baseline: Option<f64>) -> Self {
        Self {
            current,
            baseline,
            absolute: current.zip(baseline).map(|(c, b)| c - b),
            relative: current
                .zip(baseline)
                .and_then(|(c, b)| (b != 0.0).then(|| (c - b) / b.abs())),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryComparison {
    pub current: HistorySummary,
    pub baseline: HistorySummary,
    pub changes: BTreeMap<String, MetricChange>,
}
impl StatsReader {
    pub fn compare_history(
        &self,
        tenant: &str,
        current: (i64, i64),
        baseline: (i64, i64),
        filter: &HistoryFilter,
    ) -> Result<HistoryComparison, StatsError> {
        let _snapshot = self.conn.unchecked_transaction()?;
        let current = history_on(&self.conn, tenant, current.0, current.1, filter)?;
        let baseline = history_on(&self.conn, tenant, baseline.0, baseline.1, filter)?;
        let values = |s: &HistorySummary| {
            [
                ("Client requests", Some(s.metrics.client_requests as f64)),
                ("Conversations", Some(s.conversations as f64)),
                (
                    "Gross reduction (estimated)",
                    Some(s.metrics.saved_gross as f64),
                ),
                ("Retrieval debit", Some(s.metrics.retrieved_tokens as f64)),
                ("Net reduction (estimated)", Some(s.net_reduction as f64)),
                ("Net per client request", s.net_per_client),
                ("Provider input per client request", s.input_per_client),
                ("Provider input per conversation", s.input_per_conversation),
                (
                    "Input-equivalent cost",
                    Some(s.metrics.input_equivalent_cost),
                ),
                (
                    "Keep-warm input-equivalent cost",
                    Some(s.metrics.ping_input_equivalent_cost),
                ),
                ("Mean client latency (ms)", s.mean_latency_ms),
                ("Client error share", s.error_share),
            ]
        };
        let changes = values(&current)
            .into_iter()
            .zip(values(&baseline))
            .map(|((k, c), (_, b))| (k.to_string(), MetricChange::new(c, b)))
            .collect();
        Ok(HistoryComparison {
            current,
            baseline,
            changes,
        })
    }
}
