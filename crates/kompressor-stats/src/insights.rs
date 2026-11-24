//! Retained-event diagnostics. Rankings are bounded and expose omitted rows;
//! unlike historical totals, these require request/turn ordering.
use crate::{StatsError, StatsReader, context::ProxyTiming};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
const LIMIT: usize = 100;
const MAX_LATENCY_ROWS: usize = 200_000;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RetrievalReport {
    pub attempts: u64,
    pub hits: u64,
    pub missing: u64,
    pub expired: u64,
    pub withheld: u64,
    pub ranged_tokens: i64,
    pub full_tokens: i64,
    pub saved_gross: i64,
    pub retrieved_tokens: i64,
    pub debit_to_gross: Option<f64>,
    pub requests: Vec<RetrievalRequest>,
    pub requests_total: usize,
    pub repeated_refs: Vec<RepeatedRef>,
    pub repeated_refs_total: usize,
    pub rounds: Vec<CountBucket>,
    pub raw_only: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetrievalRequest {
    pub request_id: String,
    pub conversation: Option<String>,
    pub time_ms: i64,
    pub saved_gross: i64,
    pub retrieved_tokens: i64,
    pub net_reduction: i64,
    pub debit_to_gross: Option<f64>,
    pub rounds: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepeatedRef {
    pub conversation: Option<String>,
    pub reference: String,
    pub asks: u64,
    pub returned_tokens: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CountBucket {
    pub name: String,
    pub count: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Distribution {
    pub samples: usize,
    pub mean: Option<f64>,
    pub p50: Option<f64>,
    pub p95: Option<f64>,
    pub p99: Option<f64>,
    pub max: Option<f64>,
}
fn distribution(mut values: Vec<f64>) -> Distribution {
    values.retain(|v| v.is_finite() && *v >= 0.0);
    values.sort_by(f64::total_cmp);
    let n = values.len();
    if n == 0 {
        return Distribution::default();
    }
    let rank = |p: f64| {
        values[((n as f64 * p).ceil() as usize)
            .saturating_sub(1)
            .min(n - 1)]
    };
    Distribution {
        samples: n,
        mean: Some(values.iter().sum::<f64>() / n as f64),
        p50: Some(rank(0.5)),
        p95: Some(rank(0.95)),
        p99: Some(rank(0.99)),
        max: values.last().copied(),
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LatencyGroup {
    pub dimension: String,
    pub name: String,
    pub requests: u64,
    pub errors: u64,
    pub latency: Distribution,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SlowRequest {
    pub request_id: String,
    pub conversation: Option<String>,
    pub time_ms: i64,
    pub model: Option<String>,
    pub total_ms: f64,
    pub rounds: i64,
    pub error: bool,
    pub timing: Option<ProxyTiming>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LatencyReport {
    pub requests: u64,
    pub rows_total: u64,
    pub truncated: bool,
    pub errors: u64,
    pub with_timing: u64,
    pub attempt_details_omitted: u64,
    pub total: Distribution,
    pub upstream_head_wait: Distribution,
    pub first_head: Distribution,
    pub engine: Distribution,
    pub retrieval_execution: Distribution,
    pub attempt_wait: Distribution,
    pub groups: Vec<LatencyGroup>,
    pub slowest: Vec<SlowRequest>,
    pub errors_over_time: Vec<ErrorBucket>,
    pub raw_only: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorBucket {
    pub time_ms: i64,
    pub requests: u64,
    pub errors: u64,
}
fn ratio(n: i64, d: i64) -> Option<f64> {
    (d > 0).then(|| n as f64 / d as f64)
}

impl StatsReader {
    pub fn retrieval_analysis(
        &self,
        tenant: &str,
        from: i64,
        to: i64,
    ) -> Result<RetrievalReport, StatsError> {
        let _snapshot = self.conn.unchecked_transaction()?;
        let mut result = RetrievalReport {
            raw_only: true,
            ..Default::default()
        };
        let (attempts,hits,missing,expired,withheld,ranged,full):(i64,i64,i64,i64,i64,i64,i64)=self.conn.query_row("SELECT coalesce(SUM(1-withheld),0),coalesce(SUM(CASE WHEN withheld=0 THEN found ELSE 0 END),0),coalesce(SUM(CASE WHEN withheld=0 AND found=0 AND expired=0 THEN 1 ELSE 0 END),0),coalesce(SUM(CASE WHEN withheld=0 AND found=0 AND expired=1 THEN 1 ELSE 0 END),0),coalesce(SUM(withheld),0),coalesce(SUM(CASE WHEN withheld=0 AND found=1 AND ranged=1 THEN returned_tokens ELSE 0 END),0),coalesce(SUM(CASE WHEN withheld=0 AND found=1 AND ranged=0 THEN returned_tokens ELSE 0 END),0) FROM retrieval_events WHERE tenant=?1 AND time_ms>=?2 AND time_ms<?3",params![tenant,from,to],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?)))?;
        result.attempts = attempts as u64;
        result.hits = hits as u64;
        result.missing = missing as u64;
        result.expired = expired as u64;
        result.withheld = withheld as u64;
        result.ranged_tokens = ranged;
        result.full_tokens = full;
        result.retrieved_tokens = ranged + full;
        result.saved_gross=self.conn.query_row("SELECT coalesce(SUM(tokens_in-tokens_out),0) FROM optimization_events WHERE tenant=?1 AND time_ms>=?2 AND time_ms<?3 AND coalesce(outcome,'')<>'discarded'",params![tenant,from,to],|r|r.get(0))?;
        result.debit_to_gross = ratio(result.retrieved_tokens, result.saved_gross);
        let sql = "WITH o AS (SELECT request_id,SUM(CASE WHEN coalesce(outcome,'')<>'discarded' THEN tokens_in-tokens_out ELSE 0 END) gross FROM optimization_events WHERE tenant=?1 AND time_ms>=?2 AND time_ms<?3 GROUP BY request_id), r AS (SELECT request_id,MIN(time_ms) time_ms,SUM(CASE WHEN found=1 AND withheld=0 THEN returned_tokens ELSE 0 END) debit FROM retrieval_events WHERE tenant=?1 AND time_ms>=?2 AND time_ms<?3 AND request_id IS NOT NULL GROUP BY request_id), p AS (SELECT request_id,MAX(conversation) conversation,SUM(retrieve_rounds) rounds FROM proxy_requests WHERE tenant=?1 AND time_ms>=?2 AND time_ms<?3 AND keep_warm=0 GROUP BY request_id) SELECT r.request_id,p.conversation,r.time_ms,coalesce(o.gross,0),r.debit,coalesce(p.rounds,0) FROM r LEFT JOIN o ON o.request_id=r.request_id LEFT JOIN p ON p.request_id=r.request_id ORDER BY r.debit-coalesce(o.gross,0) DESC,r.request_id";
        let mut stmt = self.conn.prepare(sql)?;
        let mut rows = stmt.query(params![tenant, from, to])?;
        while let Some(r) = rows.next()? {
            result.requests_total += 1;
            if result.requests.len() < LIMIT {
                let gross = r.get(3)?;
                let debit = r.get(4)?;
                result.requests.push(RetrievalRequest {
                    request_id: r.get(0)?,
                    conversation: r.get(1)?,
                    time_ms: r.get(2)?,
                    saved_gross: gross,
                    retrieved_tokens: debit,
                    net_reduction: gross - debit,
                    debit_to_gross: ratio(debit, gross),
                    rounds: r.get(5)?,
                });
            }
        }
        // A missing fingerprint cannot establish that two asks share a
        // conversation: keep those under their individual request IDs.
        let sql = "WITH asks AS (SELECT r.ref,r.returned_tokens,r.found,r.withheld,r.request_id,(SELECT p.conversation FROM proxy_requests p WHERE p.tenant=r.tenant AND p.request_id=r.request_id AND p.keep_warm=0 LIMIT 1) conversation FROM retrieval_events r WHERE r.tenant=?1 AND r.time_ms>=?2 AND r.time_ms<?3) SELECT conversation,ref,COUNT(*),SUM(CASE WHEN found=1 AND withheld=0 THEN returned_tokens ELSE 0 END) FROM asks WHERE request_id IS NOT NULL GROUP BY coalesce(conversation,'request:'||request_id),ref HAVING COUNT(*)>1 ORDER BY 4 DESC,2";
        let mut stmt = self.conn.prepare(sql)?;
        let mut rows = stmt.query(params![tenant, from, to])?;
        while let Some(r) = rows.next()? {
            result.repeated_refs_total += 1;
            if result.repeated_refs.len() < LIMIT {
                result.repeated_refs.push(RepeatedRef {
                    conversation: r.get(0)?,
                    reference: r.get(1)?,
                    asks: r.get::<_, i64>(2)? as u64,
                    returned_tokens: r.get(3)?,
                });
            }
        }
        let mut stmt=self.conn.prepare("SELECT CASE WHEN retrieve_rounds=0 THEN '0' WHEN retrieve_rounds=1 THEN '1' WHEN retrieve_rounds<=3 THEN '2–3' WHEN retrieve_rounds<=7 THEN '4–7' ELSE '8+' END,COUNT(*) FROM proxy_requests WHERE tenant=?1 AND time_ms>=?2 AND time_ms<?3 AND keep_warm=0 GROUP BY 1 ORDER BY MIN(retrieve_rounds)")?;
        result.rounds = stmt
            .query_map(params![tenant, from, to], |r| {
                Ok(CountBucket {
                    name: r.get(0)?,
                    count: r.get::<_, i64>(1)? as u64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(result)
    }

    pub fn latency_analysis(
        &self,
        tenant: &str,
        from: i64,
        to: i64,
    ) -> Result<LatencyReport, StatsError> {
        let _snapshot = self.conn.unchecked_transaction()?;
        let rows_total:i64=self.conn.query_row("SELECT COUNT(*) FROM proxy_requests WHERE tenant=?1 AND time_ms>=?2 AND time_ms<?3 AND keep_warm=0",params![tenant,from,to],|r|r.get(0))?;
        let mut stmt=self.conn.prepare("SELECT p.request_id,p.conversation,p.time_ms,p.model,p.total_ms,p.upstream_ms,p.retrieve_rounds,p.status,p.error,CASE WHEN p.api_family='anthropic_messages' AND p.prompt_tokens IS NOT NULL THEN p.prompt_tokens+coalesce(p.cache_read_tokens,0)+coalesce(p.cache_write_tokens,0) ELSE p.prompt_tokens END,p.timing_json,(SELECT SUM(o.engine_ms) FROM optimization_events o WHERE o.tenant=p.tenant AND o.request_id=p.request_id AND o.time_ms>=?2 AND o.time_ms<?3) FROM proxy_requests p WHERE p.tenant=?1 AND p.time_ms>=?2 AND p.time_ms<?3 AND p.keep_warm=0 ORDER BY p.time_ms DESC,p.id DESC LIMIT ?4")?;
        let mut rows = stmt.query(params![tenant, from, to, MAX_LATENCY_ROWS as i64])?;
        let (mut total, mut upstream, mut first, mut engine, mut retrieve, mut attempts) = (
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let mut groups = BTreeMap::<(String, String), (u64, u64, Vec<f64>)>::new();
        let mut slow = Vec::new();
        let mut timeline = BTreeMap::<i64, (u64, u64)>::new();
        let (mut count, mut errors, mut with_timing) = (0, 0, 0);
        let mut attempt_details_omitted = 0;
        let bucket = crate::live::bucket_for(to.saturating_sub(from));
        while let Some(r) = rows.next()? {
            count += 1;
            let status: Option<i64> = r.get(7)?;
            let error = status.is_none_or(|s| s >= 400) || r.get::<_, Option<String>>(8)?.is_some();
            if error {
                errors += 1;
            }
            let time: i64 = r.get(2)?;
            let b = from + (time - from) / bucket * bucket;
            let t = timeline.entry(b).or_default();
            t.0 += 1;
            t.1 += u64::from(error);
            let model: Option<String> = r.get(3)?;
            let rounds: i64 = r.get(6)?;
            let ms = r
                .get::<_, Option<f64>>(4)?
                .filter(|v| v.is_finite() && *v >= 0.0);
            if let Some(v) = ms {
                total.push(v);
            }
            if let Some(v) = r.get::<_, Option<f64>>(5)? {
                upstream.push(v);
            }
            if let Some(v) = r.get::<_, Option<f64>>(11)? {
                engine.push(v);
            }
            let timing: Option<ProxyTiming> = r
                .get::<_, Option<String>>(10)?
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?;
            if let Some(t) = &timing {
                with_timing += 1;
                attempt_details_omitted += t.attempts_total.saturating_sub(t.attempts.len() as u64);
                if let Some(v) = t.first_head_ms {
                    first.push(v);
                }
                if t.retrieval_calls > 0 {
                    retrieve.push(t.retrieval_execution_ms);
                }
                attempts.extend(t.attempts.iter().map(|a| a.duration_ms));
            }
            let prompt: Option<i64> = r.get(9)?;
            let size = match prompt {
                None => "unreported",
                Some(n) if n < 4096 => "under 4K",
                Some(n) if n < 16384 => "4K–16K",
                Some(n) if n < 65536 => "16K–64K",
                Some(_) => "64K+",
            };
            for (dim, name) in [
                (
                    "model",
                    model.clone().unwrap_or_else(|| "unrecorded".into()),
                ),
                ("retrieval rounds", rounds.to_string()),
                ("provider prompt tokens", size.into()),
                (
                    "status",
                    status.map_or_else(|| "no response".into(), |s| format!("{}xx", s / 100)),
                ),
            ] {
                let g = groups.entry((dim.into(), name)).or_default();
                g.0 += 1;
                g.1 += u64::from(error);
                if let Some(v) = ms {
                    g.2.push(v);
                }
            }
            if let Some(total_ms) = ms
                && (slow.len() < 50
                    || slow
                        .last()
                        .is_some_and(|r: &SlowRequest| total_ms > r.total_ms))
            {
                slow.push(SlowRequest {
                    request_id: r.get(0)?,
                    conversation: r.get(1)?,
                    time_ms: time,
                    model,
                    total_ms,
                    rounds,
                    error,
                    timing,
                });
                slow.sort_by(|a, b| b.total_ms.total_cmp(&a.total_ms));
                slow.truncate(50);
            }
        }
        slow.sort_by(|a, b| b.total_ms.total_cmp(&a.total_ms));
        slow.truncate(50);
        Ok(LatencyReport {
            requests: count,
            rows_total: rows_total as u64,
            truncated: rows_total as usize > MAX_LATENCY_ROWS,
            errors,
            with_timing,
            attempt_details_omitted,
            total: distribution(total),
            upstream_head_wait: distribution(upstream),
            first_head: distribution(first),
            engine: distribution(engine),
            retrieval_execution: distribution(retrieve),
            attempt_wait: distribution(attempts),
            groups: groups
                .into_iter()
                .map(
                    |((dimension, name), (requests, errors, values))| LatencyGroup {
                        dimension,
                        name,
                        requests,
                        errors,
                        latency: distribution(values),
                    },
                )
                .collect(),
            slowest: slow,
            errors_over_time: timeline
                .into_iter()
                .map(|(time_ms, (requests, errors))| ErrorBucket {
                    time_ms,
                    requests,
                    errors,
                })
                .collect(),
            raw_only: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn percentiles_use_nearest_rank_and_ignore_invalid_samples() {
        let d = distribution(vec![f64::NAN, -1.0, 0.0, 10.0, 20.0, 30.0]);
        assert_eq!(d.samples, 4);
        assert_eq!(d.p50, Some(10.0));
        assert_eq!(d.p95, Some(30.0));
        assert_eq!(d.mean, Some(15.0));
        assert_eq!(distribution(vec![]).mean, None);
    }
    #[test]
    fn zero_or_negative_credit_has_no_amplification_ratio() {
        assert_eq!(ratio(100, 0), None);
        assert_eq!(ratio(100, -1), None);
        assert_eq!(ratio(300, 100), Some(3.0));
    }
}
