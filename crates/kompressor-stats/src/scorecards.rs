//! Three reports the other dashboards do not give, over the same tables:
//!
//! - **models**: one row per API family and model, putting side by side what
//!   is otherwise on four pages — what the provider billed, what the engine
//!   removed and how long it took, what retrieval put back, and how often the
//!   upstream failed. It answers "which model is compression paying on, and
//!   which is it only costing latency on".
//! - **activity**: the week folded onto itself, one cell per weekday and hour
//!   (UTC). Requests, billed input, cache writes and engine savings per cell —
//!   when the proxy is busy, and at which hours the cache is being rebuilt.
//! - **cohorts**: conversations grouped by how many client turns they ran.
//!   Most conversations are one or two turns and most of the bill is a handful
//!   of long ones; an average over all of them describes neither.
//!
//! Nothing here adds a table, a column or a view: `models` reads
//! `proxy_requests` and `optimization_events`, `activity` the same two, and
//! `cohorts` is `stats conversations` bucketed.

use std::collections::BTreeMap;

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::breakdown::{Billed, ConversationOrder, ConversationRow};
use crate::live::{Latency, percentiles};
use crate::traffic::{kind_of, outcome_of};
use crate::{StatsError, StatsReader};

/// Engine rows read for the latency percentiles, newest first.
pub const MAX_ENGINE_ROWS: usize = 200_000;
/// Conversations read for the cohorts, costliest first.
pub const MAX_COHORT_CONVERSATIONS: usize = 50_000;

/// Cohorts by client turns: label and exclusive upper bound.
pub const TURN_BUCKETS: &[(&str, Option<u64>)] = &[
    ("1", Some(2)),
    ("2–4", Some(5)),
    ("5–19", Some(20)),
    ("20–99", Some(100)),
    ("100–499", Some(500)),
    ("500+", None),
];

fn share(part: i64, whole: i64) -> Option<f64> {
    (whole > 0).then(|| part as f64 / whole as f64)
}

// ------------------------------------------------------------------ models

/// One API family and model.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelCard {
    pub api_family: String,
    pub model: String,
    /// Client requests the proxy relayed, and the pings it sent itself.
    pub client_requests: u64,
    pub pings: u64,
    /// Upstream-kind failures (429, 5xx, no answer, a 2xx whose stream broke)
    /// over client requests and pings.
    pub upstream_failures: u64,
    pub upstream_failure_share: Option<f64>,
    /// What the provider billed, clients and pings together.
    pub billed: Billed,
    /// Engine runs, discarded rewrites included.
    pub engine_runs: u64,
    /// Runs that rewrote at least one message.
    pub compressed_runs: u64,
    /// Estimated tokens the engine was shown and what it left, discarded
    /// rewrites excluded.
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub saved_gross: i64,
    /// `saved_gross / tokens_in`.
    pub reduction: Option<f64>,
    pub engine_ms: Latency,
    /// Engine time spent on runs that changed nothing, in milliseconds.
    pub idle_engine_ms: f64,
    pub retrieve_rounds: i64,
    /// Tokens retrieval rounds put back into the prompt.
    pub retrieved_tokens: i64,
    /// `saved_gross - retrieved_tokens`. May be negative.
    pub saved_net: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelsReport {
    pub tenant: String,
    pub from_ms: i64,
    pub to_ms: i64,
    /// Costliest first, by input-equivalent tokens billed.
    pub models: Vec<ModelCard>,
    /// Whether more than [`MAX_ENGINE_ROWS`] engine rows fell in the window, so
    /// the engine columns cover only the newest.
    pub truncated: bool,
}

#[derive(Default)]
struct EngineAcc {
    runs: u64,
    compressed: u64,
    tokens_in: i64,
    tokens_out: i64,
    samples: Vec<f64>,
    idle_ms: f64,
}

// ---------------------------------------------------------------- activity

/// One weekday and hour, UTC. `weekday` is 0 for Sunday, as SQLite's `%w`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ActivityCell {
    pub weekday: u8,
    pub hour: u8,
    /// Model calls from clients; pings and pass-through are counted apart.
    pub requests: u64,
    pub pings: u64,
    pub passthrough: u64,
    /// Every input token billed in the cell, however billed.
    pub input_tokens: i64,
    pub cache_write_tokens: i64,
    pub completion_tokens: i64,
    /// Engine savings, discarded rewrites excluded.
    pub saved_gross: i64,
    /// Distinct days in the window that put anything in this cell.
    pub days: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ActivityReport {
    pub tenant: String,
    pub from_ms: i64,
    pub to_ms: i64,
    /// Only the cells with something in them, Sunday 00:00 first.
    pub cells: Vec<ActivityCell>,
    pub requests: u64,
    pub input_tokens: i64,
    pub cache_write_tokens: i64,
    /// The cell with the most model calls.
    pub busiest: Option<ActivityCell>,
    /// Share of model calls in the busiest eight consecutive hours of the day,
    /// every weekday together, and the hour (UTC) those eight start at.
    pub peak_window_share: Option<f64>,
    pub peak_window_start_hour: Option<u8>,
}

// ----------------------------------------------------------------- cohorts

/// Conversations with a number of client turns in one bucket.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Cohort {
    pub turns: String,
    pub conversations: u64,
    pub client_turns: u64,
    pub pings: u64,
    pub errors: u64,
    pub billed: Billed,
    /// Of every cohort's input-equivalent cost.
    pub cost_share: Option<f64>,
    pub saved_gross: i64,
    pub retrieved_tokens: i64,
    pub saved_net: i64,
    /// `saved_net / billed.input`: what compression took off the tokens the
    /// provider was actually sent.
    pub net_saving_share: Option<f64>,
    pub retrieve_rounds: i64,
    /// Median wall time from a conversation's first row to its last.
    pub median_duration_ms: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CohortsReport {
    pub tenant: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub conversations: u64,
    pub cohorts: Vec<Cohort>,
    /// Smallest share of conversations that carries half the cost.
    pub half_cost_conversation_share: Option<f64>,
    /// Whether more than [`MAX_COHORT_CONVERSATIONS`] fell in the window.
    pub truncated: bool,
}

/// The index of the bucket `turns` falls in.
pub fn turn_bucket(turns: u64) -> usize {
    TURN_BUCKETS
        .iter()
        .position(|(_, upto)| upto.is_none_or(|upto| turns < upto))
        .unwrap_or(TURN_BUCKETS.len() - 1)
}

/// Conversations bucketed by client turns. Pure, so the bucketing is testable
/// without a database.
pub fn cohorts_of(rows: &[ConversationRow]) -> (Vec<Cohort>, Option<f64>) {
    let mut cohorts: Vec<Cohort> = TURN_BUCKETS
        .iter()
        .map(|(label, _)| Cohort {
            turns: (*label).to_string(),
            ..Cohort::default()
        })
        .collect();
    let mut durations: Vec<Vec<i64>> = vec![Vec::new(); TURN_BUCKETS.len()];
    for row in rows {
        let at = turn_bucket(row.client_turns);
        let cohort = &mut cohorts[at];
        cohort.conversations += 1;
        cohort.client_turns += row.client_turns;
        cohort.pings += row.pings;
        cohort.errors += row.errors;
        cohort.billed.add(&row.billed);
        cohort.saved_gross += row.saved_gross;
        cohort.retrieved_tokens += row.retrieved_tokens;
        cohort.saved_net += row.saved_net;
        cohort.retrieve_rounds += row.retrieve_rounds;
        durations[at].push((row.last_ms - row.first_ms).max(0));
    }
    let total_cost: f64 = cohorts.iter().map(|c| c.billed.cost).sum();
    for (cohort, durations) in cohorts.iter_mut().zip(durations.iter_mut()) {
        cohort.billed.finish();
        cohort.cost_share = (total_cost > 0.0).then(|| cohort.billed.cost / total_cost);
        cohort.net_saving_share = share(cohort.saved_net, cohort.billed.input);
        durations.sort_unstable();
        // Lower median: an even count takes the smaller of the middle two, so
        // the figure is always one a conversation actually had.
        cohort.median_duration_ms = durations
            .get(durations.len().saturating_sub(1) / 2)
            .copied();
    }

    // Smallest share of conversations carrying half the cost.
    let mut costs: Vec<f64> = rows.iter().map(|r| r.billed.cost).collect();
    costs.sort_by(|a, b| b.total_cmp(a));
    let half = if total_cost > 0.0 && !costs.is_empty() {
        let mut running = 0.0;
        let mut needed = costs.len();
        for (i, cost) in costs.iter().enumerate() {
            running += cost;
            if running >= total_cost / 2.0 {
                needed = i + 1;
                break;
            }
        }
        Some(needed as f64 / costs.len() as f64)
    } else {
        None
    };
    (cohorts, half)
}

/// The busiest `width` consecutive hours of a 24-hour histogram, wrapping past
/// midnight: `(start hour, share of the day's total)`.
pub fn peak_window(by_hour: &[u64; 24], width: usize) -> Option<(u8, f64)> {
    let total: u64 = by_hour.iter().sum();
    if total == 0 || width == 0 {
        return None;
    }
    let width = width.min(24);
    let (start, sum) = (0..24)
        .map(|start| {
            let sum: u64 = (0..width).map(|i| by_hour[(start + i) % 24]).sum();
            (start, sum)
        })
        // Earliest start wins a tie, so the answer does not depend on scan order.
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))?;
    Some((start as u8, sum as f64 / total as f64))
}

impl StatsReader {
    /// One row per API family and model for `tenant` over `[from_ms, to_ms)`.
    pub fn models(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<ModelsReport, StatsError> {
        let mut cards: BTreeMap<(String, String), ModelCard> = BTreeMap::new();
        fn entry<'a>(
            cards: &'a mut BTreeMap<(String, String), ModelCard>,
            family: &str,
            model: &str,
        ) -> &'a mut ModelCard {
            cards
                .entry((family.to_string(), model.to_string()))
                .or_insert_with(|| ModelCard {
                    api_family: family.to_string(),
                    model: model.to_string(),
                    ..ModelCard::default()
                })
        }

        // What was billed. Pass-through rows carry no model and no usage.
        let billing = self.cache_billing(tenant, from_ms, to_ms)?;
        for model in billing.models {
            if model.api_family == "passthrough" {
                continue;
            }
            let card = entry(&mut cards, &model.api_family, &model.model);
            card.client_requests = model.client_requests;
            card.pings = model.pings;
            card.billed.add(&model.clients);
            card.billed.add(&model.ping_usage);
            card.billed.finish();
        }

        // Failures and retrieval, grouped so a busy window is never truncated.
        {
            let mut stmt = self.conn.prepare_cached(
                "SELECT api_family, coalesce(model, ''), status, substr(error, 1, 24),
                        COUNT(*), SUM(retrieve_rounds), SUM(retrieve_tokens_injected)
                   FROM proxy_requests
                  WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3
                    AND api_family <> 'passthrough'
                  GROUP BY 1, 2, 3, 4",
            )?;
            let mut rows = stmt.query(params![tenant, from_ms, to_ms])?;
            while let Some(row) = rows.next()? {
                let family: String = row.get(0)?;
                let model: String = row.get(1)?;
                let status: Option<i64> = row.get(2)?;
                let error: Option<String> = row.get(3)?;
                let count = row.get::<_, i64>(4)?.max(0) as u64;
                let card = entry(&mut cards, &family, &model);
                if kind_of(outcome_of(status, error.as_deref())) == "upstream" {
                    card.upstream_failures += count;
                }
                card.retrieve_rounds += row.get::<_, Option<i64>>(5)?.unwrap_or(0);
                card.retrieved_tokens += row.get::<_, Option<i64>>(6)?.unwrap_or(0);
            }
        }

        // The engine. Rows are read rather than grouped because a percentile
        // needs the samples.
        let mut engine: BTreeMap<(String, String), EngineAcc> = BTreeMap::new();
        let mut truncated = false;
        {
            let mut stmt = self.conn.prepare_cached(
                "SELECT coalesce(api_family, ''), coalesce(model, ''), engine_ms, tokens_in,
                        tokens_out, coalesce(outcome, ''), messages_compressed
                   FROM optimization_events
                  WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3
                  ORDER BY time_ms DESC, id DESC LIMIT ?4",
            )?;
            let mut rows = stmt.query(params![
                tenant,
                from_ms,
                to_ms,
                (MAX_ENGINE_ROWS + 1) as i64
            ])?;
            let mut read = 0usize;
            while let Some(row) = rows.next()? {
                read += 1;
                if read > MAX_ENGINE_ROWS {
                    truncated = true;
                    break;
                }
                let family: String = row.get(0)?;
                let model: String = row.get(1)?;
                let ms: f64 = row.get(2)?;
                let tokens_in: i64 = row.get(3)?;
                let tokens_out: i64 = row.get(4)?;
                let outcome: String = row.get(5)?;
                let compressed: i64 = row.get(6)?;
                let acc = engine.entry((family, model)).or_default();
                acc.runs += 1;
                if ms.is_finite() {
                    acc.samples.push(ms);
                }
                // A discarded rewrite never reached the provider: no credit.
                if outcome != "discarded" {
                    acc.tokens_in += tokens_in;
                    acc.tokens_out += tokens_out;
                }
                if outcome == "compressed" && compressed > 0 && tokens_in > tokens_out {
                    acc.compressed += 1;
                } else if ms.is_finite() {
                    acc.idle_ms += ms;
                }
            }
        }
        for ((family, model), acc) in engine {
            let card = entry(&mut cards, &family, &model);
            card.engine_runs = acc.runs;
            card.compressed_runs = acc.compressed;
            card.tokens_in = acc.tokens_in;
            card.tokens_out = acc.tokens_out;
            card.saved_gross = acc.tokens_in - acc.tokens_out;
            card.reduction = share(card.saved_gross, card.tokens_in);
            card.engine_ms = percentiles(acc.samples);
            card.idle_engine_ms = acc.idle_ms;
        }

        let mut models: Vec<ModelCard> = cards.into_values().collect();
        for card in &mut models {
            card.saved_net = card.saved_gross - card.retrieved_tokens;
            let answered = card.client_requests + card.pings;
            card.upstream_failure_share =
                (answered > 0).then(|| card.upstream_failures as f64 / answered as f64);
        }
        models.sort_by(|a, b| {
            b.billed
                .cost
                .total_cmp(&a.billed.cost)
                .then_with(|| b.tokens_in.cmp(&a.tokens_in))
                .then_with(|| a.api_family.cmp(&b.api_family))
                .then_with(|| a.model.cmp(&b.model))
        });
        Ok(ModelsReport {
            tenant: tenant.to_string(),
            from_ms,
            to_ms,
            models,
            truncated,
        })
    }

    /// The window folded onto one week, a cell per weekday and hour (UTC).
    pub fn activity(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<ActivityReport, StatsError> {
        let mut cells: BTreeMap<(u8, u8), ActivityCell> = BTreeMap::new();
        fn cell(
            cells: &mut BTreeMap<(u8, u8), ActivityCell>,
            weekday: i64,
            hour: i64,
        ) -> &mut ActivityCell {
            let key = (weekday.clamp(0, 6) as u8, hour.clamp(0, 23) as u8);
            cells.entry(key).or_insert_with(|| ActivityCell {
                weekday: key.0,
                hour: key.1,
                ..ActivityCell::default()
            })
        }
        {
            // Input is normalised as everywhere else: Anthropic reports fresh,
            // read and written apart, OpenAI's prompt already holds its cached
            // part.
            let mut stmt = self.conn.prepare_cached(
                "SELECT CAST(strftime('%w', time_ms / 1000, 'unixepoch') AS INTEGER),
                        CAST(strftime('%H', time_ms / 1000, 'unixepoch') AS INTEGER),
                        SUM(CASE WHEN keep_warm = 0 AND api_family <> 'passthrough' THEN 1 ELSE 0 END),
                        SUM(CASE WHEN keep_warm <> 0 THEN 1 ELSE 0 END),
                        SUM(CASE WHEN keep_warm = 0 AND api_family = 'passthrough' THEN 1 ELSE 0 END),
                        SUM(CASE WHEN api_family = 'anthropic_messages'
                                 THEN max(coalesce(prompt_tokens, 0), 0)
                                      + max(coalesce(cache_read_tokens, 0), 0)
                                      + max(coalesce(cache_write_tokens, 0), 0)
                                 ELSE max(coalesce(prompt_tokens, 0), coalesce(cache_read_tokens, 0), 0)
                            END),
                        SUM(CASE WHEN api_family = 'anthropic_messages'
                                 THEN max(coalesce(cache_write_tokens, 0), 0) ELSE 0 END),
                        SUM(coalesce(completion_tokens, 0)),
                        COUNT(DISTINCT time_ms / 86400000)
                   FROM proxy_requests
                  WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3
                  GROUP BY 1, 2",
            )?;
            let mut rows = stmt.query(params![tenant, from_ms, to_ms])?;
            while let Some(row) = rows.next()? {
                let c = cell(&mut cells, row.get(0)?, row.get(1)?);
                c.requests = row.get::<_, i64>(2)?.max(0) as u64;
                c.pings = row.get::<_, i64>(3)?.max(0) as u64;
                c.passthrough = row.get::<_, i64>(4)?.max(0) as u64;
                c.input_tokens = row.get(5)?;
                c.cache_write_tokens = row.get(6)?;
                c.completion_tokens = row.get(7)?;
                c.days = row.get::<_, i64>(8)?.max(0) as u64;
            }
        }
        {
            let mut stmt = self.conn.prepare_cached(
                "SELECT CAST(strftime('%w', time_ms / 1000, 'unixepoch') AS INTEGER),
                        CAST(strftime('%H', time_ms / 1000, 'unixepoch') AS INTEGER),
                        SUM(tokens_in - tokens_out), COUNT(DISTINCT time_ms / 86400000)
                   FROM optimization_events
                  WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3
                    AND coalesce(outcome, '') <> 'discarded'
                  GROUP BY 1, 2",
            )?;
            let mut rows = stmt.query(params![tenant, from_ms, to_ms])?;
            while let Some(row) = rows.next()? {
                let c = cell(&mut cells, row.get(0)?, row.get(1)?);
                c.saved_gross = row.get::<_, Option<i64>>(2)?.unwrap_or(0);
                c.days = c.days.max(row.get::<_, i64>(3)?.max(0) as u64);
            }
        }

        let cells: Vec<ActivityCell> = cells.into_values().collect();
        let mut by_hour = [0u64; 24];
        for c in &cells {
            by_hour[c.hour as usize] += c.requests;
        }
        let peak = peak_window(&by_hour, 8);
        let busiest = cells
            .iter()
            .filter(|c| c.requests > 0)
            // Earliest cell wins a tie.
            .max_by(|a, b| {
                a.requests
                    .cmp(&b.requests)
                    .then((b.weekday, b.hour).cmp(&(a.weekday, a.hour)))
            })
            .cloned();
        Ok(ActivityReport {
            tenant: tenant.to_string(),
            from_ms,
            to_ms,
            requests: cells.iter().map(|c| c.requests).sum(),
            input_tokens: cells.iter().map(|c| c.input_tokens).sum(),
            cache_write_tokens: cells.iter().map(|c| c.cache_write_tokens).sum(),
            busiest,
            peak_window_share: peak.map(|(_, share)| share),
            peak_window_start_hour: peak.map(|(hour, _)| hour),
            cells,
        })
    }

    /// Conversations in `[from_ms, to_ms)` grouped by client turns.
    pub fn cohorts(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<CohortsReport, StatsError> {
        let report = self.conversations(
            tenant,
            from_ms,
            to_ms,
            ConversationOrder::Cost,
            MAX_COHORT_CONVERSATIONS,
        )?;
        let (cohorts, half) = cohorts_of(&report.conversations);
        Ok(CohortsReport {
            tenant: tenant.to_string(),
            from_ms,
            to_ms,
            conversations: report.conversations.len() as u64,
            cohorts,
            half_cost_conversation_share: half,
            truncated: report.total > report.conversations.len(),
        })
    }
}

#[cfg(test)]
#[path = "scorecards_tests.rs"]
mod tests;
