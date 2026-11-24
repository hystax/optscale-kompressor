//! Historical analysis. Credits and debits are aggregated before joining so
//! multiple engine events or retrievals never multiply a request's usage.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::cache::{CacheTokens, ConversationCache, MAX_TURNS, Turn};
use crate::{StatsError, StatsReader};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Efficiency {
    pub conversation: Option<String>,
    pub models: Vec<String>,
    pub first_ms: i64,
    pub last_ms: i64,
    pub client_turns: u64,
    pub pings: u64,
    pub compressions: u64,
    pub saved_gross: i64,
    pub retrieved_tokens: i64,
    pub saved_net: i64,
    pub retrieval_rounds: i64,
    pub errors: u64,
    pub latency_samples: u64,
    pub latency_ms_total: f64,
    pub cache: Option<ConversationCache>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Opportunity {
    pub name: String,
    pub messages: i64,
    pub tokens: i64,
    pub requests: u64,
    pub example_request: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnalysisTurn {
    pub time_ms: i64,
    pub request_id: String,
    pub model: Option<String>,
    pub keep_warm: bool,
    pub status: Option<i64>,
    pub error: Option<String>,
    pub compressions: u64,
    pub saved_gross: i64,
    pub retrieved_tokens: i64,
    pub saved_net: i64,
    pub retrieval_rounds: i64,
    pub total_ms: Option<f64>,
    pub cache: Option<Turn>,
    pub references: Vec<String>,
    pub references_truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnalysisReport {
    pub tenant: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub conversations: Vec<Efficiency>,
    pub unattributed: Efficiency,
    pub outcomes: Vec<Opportunity>,
    pub skips: Vec<Opportunity>,
    pub breakdowns: Vec<Opportunity>,
    pub turns: Option<Vec<AnalysisTurn>>,
    pub turns_total: usize,
    pub turns_truncated: bool,
    pub cache_truncated: bool,
    pub usage: CacheTokens,
    pub ping_usage: CacheTokens,
    pub raw_available_from_ms: Option<i64>,
}

// Include all three event streams, including requests with no compression and
// retrievals without a join key. The latter remain in the unattributed bucket.
//
// One pass: the three streams are projected onto a common shape and grouped
// once by request id. The previous form emulated a full outer join — three
// grouped CTEs, a UNION of their keys, then three LEFT JOINs on `IS`, which
// SQLite served by materialising each CTE and building an automatic index on
// it. `GROUP BY` treats NULL request ids as one group exactly as `IS` matched
// them, and every per-stream MIN/MAX/SUM is taken over that stream's rows
// alone because the other streams contribute NULL or 0 to it.
const REQUESTS: &str = "
WITH u AS (
 SELECT request_id, time_ms o_time, NULL r_time, NULL p_time, model o_model, NULL p_model,
  NULL conversation, 1 compressions,
  CASE WHEN coalesce(outcome,'') <> 'discarded' THEN tokens_in-tokens_out ELSE 0 END gross,
  0 debit, 0 clients, 0 pings, 0 rounds, 0 errors, 0 latency_samples, 0 latency,
  NULL status, NULL error
 FROM optimization_events WHERE tenant=?1 AND time_ms>=?2 AND time_ms<?3
 UNION ALL
 SELECT request_id, NULL, time_ms, NULL, NULL, NULL, NULL, 0, 0,
  CASE WHEN found=1 AND withheld=0 THEN returned_tokens ELSE 0 END,
  0, 0, 0, 0, 0, 0, NULL, NULL
 FROM retrieval_events WHERE tenant=?1 AND time_ms>=?2 AND time_ms<?3
 UNION ALL
 SELECT request_id, NULL, NULL, time_ms, NULL, model, conversation, 0, 0, 0,
  CASE WHEN keep_warm=0 THEN 1 ELSE 0 END, keep_warm,
  CASE WHEN keep_warm=0 THEN retrieve_rounds ELSE 0 END,
  CASE WHEN keep_warm=0 AND (error IS NOT NULL OR status IS NULL OR status>=400) THEN 1 ELSE 0 END,
  CASE WHEN keep_warm=0 AND total_ms IS NOT NULL THEN 1 ELSE 0 END,
  CASE WHEN keep_warm=0 THEN coalesce(total_ms,0) ELSE 0 END,
  status, error
 FROM proxy_requests WHERE tenant=?1 AND time_ms>=?2 AND time_ms<?3
)
SELECT coalesce(MIN(p_time), MIN(o_time), MIN(r_time)), request_id,
 coalesce(MAX(p_model), MAX(o_model)), MAX(conversation),
 SUM(clients), SUM(pings), SUM(compressions), SUM(gross), SUM(debit),
 SUM(rounds), SUM(errors), SUM(latency_samples), SUM(latency), MAX(status), MAX(error)
FROM u GROUP BY request_id ORDER BY 1 DESC, request_id DESC";

/// The outcome and model/mode/surface breakdowns, from one read of the window.
///
/// `AS MATERIALIZED` makes SQLite read `optimization_events` once into a
/// temporary table that the four groupings then share, where four separate
/// statements each scanned the window. `kind` keeps the outcome rows apart
/// and preserves the order the breakdowns were listed in before.
const BREAKDOWNS: &str = "
WITH o AS MATERIALIZED (
 SELECT request_id, messages_total, tokens_in, outcome, model, mode, surface
 FROM optimization_events WHERE tenant=?1 AND time_ms>=?2 AND time_ms<?3
)
SELECT kind, name, SUM(messages_total), SUM(tokens_in), COUNT(DISTINCT request_id), MIN(request_id)
FROM (
 SELECT 0 kind, coalesce(outcome,'unrecorded') name, request_id, messages_total, tokens_in FROM o
 UNION ALL SELECT 1, 'model: ' || coalesce(model,'unrecorded'), request_id, messages_total, tokens_in FROM o
 UNION ALL SELECT 2, 'mode: ' || coalesce(mode,'unrecorded'), request_id, messages_total, tokens_in FROM o
 UNION ALL SELECT 3, 'surface: ' || coalesce(surface,'unrecorded'), request_id, messages_total, tokens_in FROM o
)
GROUP BY kind, name ORDER BY kind, 4 DESC, name";

impl StatsReader {
    pub fn analysis(
        &self,
        tenant: &str,
        from: i64,
        to: i64,
        detail: Option<&str>,
    ) -> Result<AnalysisReport, StatsError> {
        // A read transaction keeps all panels on the same committed snapshot.
        let tx = self.conn.unchecked_transaction()?;
        let cache = self.cache(tenant, from, to, usize::MAX, detail)?;
        let mut groups = BTreeMap::<Option<String>, Efficiency>::new();
        let mut turns = Vec::new();
        let mut turns_total = 0;
        // The cache report's turns by request id: looked up once per detail
        // turn, which a linear search made quadratic in the turn count.
        let cache_turns: std::collections::HashMap<&str, _> = cache
            .turns
            .iter()
            .flatten()
            .map(|t| (t.request_id.as_str(), t))
            .collect();
        let mut stmt = self.conn.prepare_cached(REQUESTS)?;
        let mut rows = stmt.query((tenant, from, to))?;
        while let Some(row) = rows.next()? {
            let time: i64 = row.get(0)?;
            let request: Option<String> = row.get(1)?;
            let model: Option<String> = row.get(2)?;
            let conversation: Option<String> = row.get(3)?;
            let clients = row.get::<_, i64>(4)? as u64;
            let pings = row.get::<_, i64>(5)? as u64;
            let compressions = row.get::<_, i64>(6)? as u64;
            let gross: i64 = row.get(7)?;
            let debit: i64 = row.get(8)?;
            let rounds: i64 = row.get(9)?;
            let g = groups
                .entry(conversation.clone())
                .or_insert_with(|| Efficiency {
                    conversation: conversation.clone(),
                    first_ms: time,
                    last_ms: time,
                    ..Default::default()
                });
            g.first_ms = g.first_ms.min(time);
            g.last_ms = g.last_ms.max(time);
            if let Some(m) = &model
                && !g.models.contains(m)
            {
                g.models.push(m.clone());
                g.models.sort();
            }
            g.client_turns += clients;
            g.pings += pings;
            g.compressions += compressions;
            g.saved_gross += gross;
            g.retrieved_tokens += debit;
            g.saved_net += gross - debit;
            g.retrieval_rounds += rounds;
            g.errors += row.get::<_, i64>(10)? as u64;
            g.latency_samples += row.get::<_, i64>(11)? as u64;
            g.latency_ms_total += row.get::<_, f64>(12)?;
            if detail.is_some() && conversation.as_deref() == detail {
                turns_total += 1;
                if turns.len() < MAX_TURNS {
                    let request_id = request.unwrap_or_default();
                    let cache_turn = cache_turns.get(request_id.as_str()).map(|t| (*t).clone());
                    turns.push(AnalysisTurn {
                        time_ms: time,
                        request_id,
                        model,
                        keep_warm: pings > 0 && clients == 0,
                        status: row.get(13)?,
                        error: row.get(14)?,
                        compressions,
                        saved_gross: gross,
                        retrieved_tokens: debit,
                        saved_net: gross - debit,
                        retrieval_rounds: rounds,
                        total_ms: if row.get::<_, i64>(11)? as u64 > 0 {
                            Some(row.get(12)?)
                        } else {
                            None
                        },
                        cache: cache_turn,
                        references: Vec::new(),
                        references_truncated: false,
                    });
                }
            }
        }
        drop(rows);
        drop(stmt);
        // Fetch only references for displayed turns. Never load fragment content.
        let mut refs = self.conn.prepare("SELECT DISTINCT ref FROM retrieval_events WHERE tenant=?1 AND request_id=?2 AND time_ms>=?3 AND time_ms<?4 ORDER BY ref LIMIT 101")?;
        for turn in &mut turns {
            turn.references = refs
                .query_map((tenant, &turn.request_id, from, to), |r| r.get(0))?
                .collect::<Result<Vec<String>, _>>()?;
            turn.references_truncated = turn.references.len() > 100;
            turn.references.truncate(100);
        }
        turns.reverse();
        let unattributed = groups.remove(&None).unwrap_or_default();
        for c in &cache.conversations {
            if let Some(g) = groups.get_mut(&Some(c.conversation.clone())) {
                g.cache = Some(c.clone());
            }
        }
        let mut conversations: Vec<_> = groups.into_values().collect();
        conversations.sort_by_key(|c| c.saved_net);
        let (outcomes, breakdowns) = self.analysis_breakdowns(tenant, from, to)?;
        let skips = self.analysis_skips(tenant, from, to)?;
        let raw_available_from_ms = self.conn.query_row("SELECT MIN(t) FROM (SELECT MIN(time_ms) t FROM optimization_events WHERE tenant=?1 UNION ALL SELECT MIN(time_ms) FROM retrieval_events WHERE tenant=?1 UNION ALL SELECT MIN(time_ms) FROM proxy_requests WHERE tenant=?1)", [tenant], |r| r.get(0))?;
        let report = AnalysisReport {
            tenant: tenant.into(),
            from_ms: from,
            to_ms: to,
            conversations,
            unattributed,
            outcomes,
            skips,
            breakdowns,
            turns: detail.map(|_| turns),
            turns_total,
            turns_truncated: turns_total > MAX_TURNS,
            cache_truncated: cache.truncated,
            usage: cache.totals,
            ping_usage: cache.pings,
            raw_available_from_ms,
        };
        tx.commit()?;
        Ok(report)
    }

    fn analysis_breakdowns(
        &self,
        tenant: &str,
        from: i64,
        to: i64,
    ) -> Result<(Vec<Opportunity>, Vec<Opportunity>), StatsError> {
        let mut outcomes = Vec::new();
        let mut breakdowns = Vec::new();
        let mut stmt = self.conn.prepare_cached(BREAKDOWNS)?;
        let mut rows = stmt.query((tenant, from, to))?;
        while let Some(r) = rows.next()? {
            let item = Opportunity {
                name: r.get(1)?,
                messages: r.get(2)?,
                tokens: r.get(3)?,
                requests: r.get::<_, i64>(4)? as u64,
                example_request: r.get(5)?,
            };
            if r.get::<_, i64>(0)? == 0 {
                outcomes.push(item);
            } else {
                breakdowns.push(item);
            }
        }
        Ok((outcomes, breakdowns))
    }

    fn analysis_skips(
        &self,
        tenant: &str,
        from: i64,
        to: i64,
    ) -> Result<Vec<Opportunity>, StatsError> {
        // `json_extract` on the key the outer `json_each` is already at, as
        // `v_skip_reasons` does, instead of a correlated `json_each` that
        // re-parsed the token map once per (row, reason).
        Ok(self
            .conn
            .prepare_cached(
                "SELECT j.key, SUM(j.value),
                        SUM(coalesce(json_extract(o.skipped_tokens_by_reason, '$.\"' || j.key || '\"'), 0)),
                        COUNT(DISTINCT o.request_id), MIN(o.request_id)
                 FROM optimization_events o, json_each(o.skipped_by_reason) j
                 WHERE o.tenant=?1 AND o.time_ms>=?2 AND o.time_ms<?3
                 GROUP BY j.key ORDER BY 3 DESC, 1",
            )?
            .query_map((tenant, from, to), |r| {
                Ok(Opportunity {
                    name: r.get(0)?,
                    messages: r.get(1)?,
                    tokens: r.get(2)?,
                    requests: r.get::<_, i64>(3)? as u64,
                    example_request: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }
}
