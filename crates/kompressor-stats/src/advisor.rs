//! Which of the opt-in settings would pay on this traffic.
//!
//! Every setting that edits what a client sent — pinned tool order, kept beta
//! tokens, a cache key, schema compaction, recompaction on a lapsed cache, the
//! read decisions, reasoning elision — ships off, and each is for a client the
//! statistics have shown to need it. The prefix report holds that evidence and
//! leaves the reader to work out what it means. This is the working out: one
//! [`Advice`] per setting, with a verdict, the figures behind it, and the bar
//! they were held against.
//!
//! # Reading it honestly
//!
//! - **A verdict is a threshold applied to an estimate.** The thresholds are
//!   the ones the design proposed, not measured optima, and each [`Advice`]
//!   carries its own so nobody has to take the verdict on trust.
//! - **Estimated savings are ceilings.** A loss put down to a segment is
//!   counted as fully recovered by the setting for that segment; real
//!   recovery is that or less.
//! - **Cost is input-equivalent tokens**, as on the Cache page, so a share is
//!   a share of what the input side of the bill came to.
//! - **Too little traffic is its own verdict.** Below [`MIN_COMPARED`]
//!   compared requests nothing is recommended either way.
//! - Sizes measured without acting — what compaction *would* remove, what a
//!   recompaction *would* rewrite — are recorded on every request by builds
//!   that carry this report. Older rows read as zero, which understates.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::StatsError;
use crate::cache::Cause;
use crate::prefix::{ColdScore, MANY_TOOLS, PrefixReport};
use crate::reader::{StatsReader, ratio};

/// Compared requests below which no verdict is given.
pub const MIN_COMPARED: u64 = 100;
/// A drift is worth a setting when the losses put down to it reach this share
/// of input cost.
pub const DRIFT_SHARE: f64 = 0.01;
/// Compaction is worth turning on when it would remove this share of the tool
/// definitions.
pub const COMPACT_SHARE: f64 = 0.03;
/// Lapses are worth recompacting on when they cost this share of input cost.
pub const LAPSE_SHARE: f64 = 0.01;
/// The precision a margin must show before prediction is recommended, and the
/// predictions it must show it on.
pub const PREDICT_PRECISION: f64 = 0.995;
pub const PREDICT_SAMPLES: u64 = 20;
/// The read decisions are worth it when the reads they would rewrite are this
/// share of the conversation text on lapsed turns, over at least this many.
pub const READS_SHARE: f64 = 0.20;
pub const LAPSED_TURNS: u64 = 10;
/// A model is worth naming for reasoning elision when re-sent reasoning is
/// this share of its requests, over at least this many.
pub const REASONING_SHARE: f64 = 0.05;
pub const MODEL_REQUESTS: u64 = 50;
/// A cache key is worth adding when unkeyed requests read this much less of
/// their prompt from cache than keyed ones.
pub const KEY_GAP: f64 = 0.05;
pub const KEY_REQUESTS: u64 = 50;
/// Deferral is worth building when this share of Anthropic requests carry a
/// long tool list with none deferred.
pub const UNDEFERRED_SHARE: f64 = 0.10;

/// A cache read's price beside fresh input, where a figure has to be put on
/// tokens moved from one to the other. The reports price by model; an
/// estimate that is already a ceiling does not need to.
const READ_RATE: f64 = 0.1;
/// Bytes of request JSON per token, for sizing what was measured in bytes.
const BYTES_PER_TOKEN: f64 = 4.0;

/// What to do about one setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The evidence clears the bar: turning it on should pay.
    TurnOn,
    /// The evidence is in and does not clear the bar.
    NotWorthIt,
    /// Not enough traffic of the kind this setting acts on to say.
    NotEnoughData,
}

/// One figure behind a verdict.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    pub label: String,
    pub value: f64,
    /// `requests`, `turns`, `tokens`, `bytes`, `share`, `seconds` or `count`.
    pub unit: String,
}

fn fact(label: &str, value: f64, unit: &str) -> Fact {
    Fact {
        label: label.to_string(),
        value,
        unit: unit.to_string(),
    }
}

/// The advice for one setting.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Advice {
    /// The configuration key, as written under `[proxy]`.
    pub setting: String,
    pub title: String,
    pub verdict: Verdict,
    /// The value to set, when the verdict is to turn it on.
    pub value: Option<String>,
    /// Whether it is on in this deployment now. Filled in by the server,
    /// which knows the configuration; `None` from the bare report.
    pub enabled: Option<bool>,
    /// False for a setting that does not exist yet: the advice is then about
    /// whether it would be worth building.
    pub built: bool,
    /// The figure the verdict rests on, and the bar it was held against.
    pub measure: Option<f64>,
    pub threshold: f64,
    /// What `measure` is a share of.
    pub basis: String,
    /// A ceiling on what it would save, in input-equivalent tokens over the
    /// window. `None` where no honest figure can be put on it.
    pub saving: Option<f64>,
    /// The verdict in a sentence.
    pub why: String,
    pub facts: Vec<Fact>,
}

/// What the settings that are on have done in the window.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Effects {
    /// Client tool reorders pinned back before they went upstream.
    pub tool_orders_put_back: u64,
    /// Requests whose dropped beta tokens were sent all the same.
    pub betas_put_back: u64,
    pub recompactions_observed: u64,
    pub recompactions_predicted: u64,
    pub predictions_wrong: u64,
    /// Reads rewritten across the recompacted conversations' ledgers.
    pub reads_rewritten: u64,
    pub tools_compacted_bytes: u64,
    pub reasoning_elided_bytes: u64,
}

/// Everything the advisor shows, for one tenant and one window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdvisorReport {
    pub tenant: String,
    pub from_ms: i64,
    pub to_ms: i64,
    /// False when the database holds no prefix observations at all.
    pub available: bool,
    pub requests: u64,
    pub observed: u64,
    pub compared: u64,
    /// What the input side of the client requests cost, in input-equivalent
    /// tokens: the base the cost shares are shares of.
    pub input_cost: f64,
    pub truncated: bool,
    pub advice: Vec<Advice>,
    pub effects: Effects,
}

impl AdvisorReport {
    /// Record whether `setting` is on in the deployment this report is for.
    pub fn mark_enabled(&mut self, setting: &str, enabled: bool) {
        if let Some(advice) = self.advice.iter_mut().find(|a| a.setting == setting) {
            advice.enabled = Some(enabled);
        }
    }
}

/// What the rows say beyond what the prefix report already adds up.
#[derive(Default)]
struct Sums {
    /// OpenAI requests by whether the client named a cache key: requests,
    /// prompt tokens, tokens read from cache.
    keyed: (u64, i64, i64),
    unkeyed: (u64, i64, i64),
    /// Over requests not being compacted.
    compactable_bytes: u64,
    uncompacted_tools_bytes: u64,
    uncompacted_requests: u64,
    /// Over turns that lost their prefix to expiry.
    lapsed_turns: u64,
    lapsed_text_bytes: u64,
    lapsed_stale: u64,
    lapsed_superseded: u64,
    lapsed_quiet: u64,
}

fn advice(setting: &str, title: &str, threshold: f64, basis: &str) -> Advice {
    Advice {
        setting: setting.to_string(),
        title: title.to_string(),
        verdict: Verdict::NotEnoughData,
        value: None,
        enabled: None,
        built: true,
        measure: None,
        threshold,
        basis: basis.to_string(),
        saving: None,
        why: String::new(),
        facts: Vec::new(),
    }
}

fn percent(share: f64) -> String {
    format!("{:.1}%", share * 100.0)
}

/// The advice for a setting that answers one segment's client drift.
fn for_drift(
    setting: &str,
    title: &str,
    value: &str,
    segment: &str,
    prefix: &PrefixReport,
    input_cost: f64,
) -> Advice {
    let mut out = advice(setting, title, DRIFT_SHARE, "of input cost");
    let row = prefix.drift.iter().find(|row| row.segment == segment);
    let (turns, repaired, losses, tokens, cost) = row.map_or((0, 0, 0, 0, 0.0), |row| {
        (
            row.client_turns,
            row.repaired_turns,
            row.client_losses,
            row.client_lost_tokens,
            row.client_cost,
        )
    });
    out.facts = vec![
        fact(
            "requests on which the client changed it",
            turns as f64,
            "turns",
        ),
        fact("of those, put back by the proxy", repaired as f64, "turns"),
        fact("cache losses put down to it", losses as f64, "count"),
        fact("tokens not read back", tokens as f64, "tokens"),
        fact("what those losses cost", cost, "tokens"),
    ];
    if prefix.compared < MIN_COMPARED {
        out.why = format!(
            "Only {} requests could be compared with the one before; {MIN_COMPARED} are needed.",
            prefix.compared
        );
        return out;
    }
    let share = if input_cost > 0.0 {
        cost / input_cost
    } else {
        0.0
    };
    out.measure = Some(share);
    out.saving = Some(cost);
    if share >= DRIFT_SHARE {
        out.verdict = Verdict::TurnOn;
        out.value = Some(value.to_string());
        out.why = format!(
            "Losses put down to this drift come to {} of input cost, over the {} bar.",
            percent(share),
            percent(DRIFT_SHARE)
        );
    } else {
        out.verdict = Verdict::NotWorthIt;
        out.why = if turns == 0 {
            "No client changed it between turns in this window.".to_string()
        } else {
            format!(
                "The client changed it on {turns} requests, but the losses put down to that \
                 come to {} of input cost, under the {} bar.",
                percent(share),
                percent(DRIFT_SHARE)
            )
        };
    }
    out
}

/// The smallest margin at which enough predictions were made and nearly all
/// were right.
fn best_margin(cold: &[ColdScore]) -> Option<&ColdScore> {
    cold.iter()
        .filter(|score| {
            score.asked
                && score.api_family == "anthropic_messages"
                && score.predicted >= PREDICT_SAMPLES
                && score.precision >= PREDICT_PRECISION
        })
        .min_by_key(|score| score.margin_s)
}

impl StatsReader {
    /// The advisor report for `tenant` over `[from_ms, to_ms)`.
    pub fn advisor(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<AdvisorReport, StatsError> {
        // No snapshot of its own: the prefix report takes one, and SQLite has
        // no nested read transactions. The reads after it may see a few rows
        // more, which moves an estimate by less than its own error.
        let prefix = self.prefix(tenant, from_ms, to_ms)?;
        let mut report = AdvisorReport {
            tenant: tenant.to_string(),
            from_ms,
            to_ms,
            available: prefix.available,
            requests: prefix.requests,
            observed: prefix.observed,
            compared: prefix.compared,
            input_cost: 0.0,
            truncated: prefix.truncated,
            advice: Vec::new(),
            effects: Effects::default(),
        };
        if !prefix.available {
            return Ok(report);
        }
        let cache = self.cache(tenant, from_ms, to_ms, 0, None)?;
        let input_cost = cache.clients.cost;
        report.input_cost = input_cost;
        report.truncated |= cache.truncated;

        let (losses, _) = self.cache_losses(tenant, from_ms, to_ms)?;
        let mut lapse_cost = 0.0;
        let mut lapse_tokens = 0i64;
        let mut lapses = 0u64;
        let mut lapsed: HashSet<&str> = HashSet::new();
        for loss in losses.iter().filter(|loss| loss.cause == Cause::TtlExpiry) {
            lapses += 1;
            lapse_cost += loss.cost;
            lapse_tokens += loss.lost_tokens;
            if !loss.keep_warm {
                lapsed.insert(loss.request_id.as_str());
            }
        }

        let (rows, _) = self.prefix_rows(tenant, from_ms, to_ms)?;
        let mut sums = Sums::default();
        for row in &rows {
            let seen = &row.observation;
            if let (Some(keyed), Some(read)) = (seen.client_cache_key, row.read) {
                let side = if keyed {
                    &mut sums.keyed
                } else {
                    &mut sums.unkeyed
                };
                side.0 += 1;
                side.1 += row.prompt;
                side.2 += read;
            }
            if seen.tools_compacted_bytes == 0 && seen.tools_count > 0 {
                sums.uncompacted_requests += 1;
                sums.compactable_bytes += u64::from(seen.tools_compactable_bytes);
                sums.uncompacted_tools_bytes += u64::from(seen.tools_bytes);
            }
            if lapsed.contains(row.request_id.as_str()) {
                sums.lapsed_turns += 1;
                sums.lapsed_text_bytes += u64::from(seen.text_bytes);
                sums.lapsed_stale += u64::from(seen.stale_read_bytes);
                sums.lapsed_superseded += u64::from(seen.superseded_read_bytes);
                sums.lapsed_quiet += u64::from(seen.quiet_read_bytes);
            }
        }

        report.advice = vec![
            for_drift(
                "tools_order",
                "Pin the tool order",
                "pinned",
                "tool_order",
                &prefix,
                input_cost,
            ),
            for_drift(
                "sticky_beta",
                "Keep beta tokens",
                "true",
                "beta",
                &prefix,
                input_cost,
            ),
            cache_key(&sums),
            compaction(&sums),
            recompaction(&prefix, lapses, lapse_tokens, lapse_cost, input_cost),
            read_decisions(&sums),
            reasoning(&prefix),
            deferral(&prefix),
        ];

        let turns = |segment: &str| {
            prefix
                .drift
                .iter()
                .find(|row| row.segment == segment)
                .map_or(0, |row| row.repaired_turns)
        };
        report.effects = Effects {
            tool_orders_put_back: turns("tool_order"),
            betas_put_back: turns("beta"),
            recompactions_observed: prefix.recompactions.observed,
            recompactions_predicted: prefix.recompactions.predicted,
            predictions_wrong: prefix.recompactions.predicted_wrong,
            reads_rewritten: prefix.recompactions.stale_reads,
            tools_compacted_bytes: prefix
                .composition
                .iter()
                .map(|model| model.tools_compacted_bytes)
                .sum(),
            reasoning_elided_bytes: prefix
                .composition
                .iter()
                .map(|model| model.reasoning_elided_bytes)
                .sum(),
        };
        Ok(report)
    }
}

fn cache_key(sums: &Sums) -> Advice {
    let mut out = advice(
        "prompt_cache_key",
        "Name the cache route (OpenAI)",
        KEY_GAP,
        "less of the prompt read from cache than keyed requests",
    );
    let (keyed, keyed_prompt, keyed_read) = sums.keyed;
    let (unkeyed, unkeyed_prompt, unkeyed_read) = sums.unkeyed;
    let keyed_rate = ratio(keyed_read, keyed_prompt);
    let unkeyed_rate = ratio(unkeyed_read, unkeyed_prompt);
    out.facts = vec![
        fact("requests that named a key", keyed as f64, "requests"),
        fact("their prompt read from cache", keyed_rate, "share"),
        fact("requests that named none", unkeyed as f64, "requests"),
        fact("their prompt read from cache", unkeyed_rate, "share"),
    ];
    if unkeyed < KEY_REQUESTS {
        out.why = if unkeyed == 0 && keyed > 0 {
            out.verdict = Verdict::NotWorthIt;
            "Every OpenAI request in this window named its own key.".to_string()
        } else {
            format!("Only {unkeyed} OpenAI requests named no key; {KEY_REQUESTS} are needed.")
        };
        return out;
    }
    if keyed < KEY_REQUESTS {
        out.why = format!(
            "{unkeyed} requests named no key, but only {keyed} named one, so there is \
             nothing to compare their cache reads with."
        );
        return out;
    }
    let gap = keyed_rate - unkeyed_rate;
    out.measure = Some(gap);
    if gap >= KEY_GAP {
        out.verdict = Verdict::TurnOn;
        out.value = Some("conversation".to_string());
        // The tokens that would move from fresh input to a cache read, were
        // unkeyed traffic read back at the keyed rate.
        out.saving = Some(gap * unkeyed_prompt as f64 * (1.0 - READ_RATE));
        out.why = format!(
            "Requests with no key read {} of their prompt from cache against {} for keyed ones.",
            percent(unkeyed_rate),
            percent(keyed_rate)
        );
    } else {
        out.verdict = Verdict::NotWorthIt;
        out.why = format!(
            "Requests with no key read {} of their prompt from cache, within {} of keyed ones.",
            percent(unkeyed_rate),
            percent(KEY_GAP)
        );
    }
    out
}

fn compaction(sums: &Sums) -> Advice {
    let mut out = advice(
        "tools_compact",
        "Compact tool schemas",
        COMPACT_SHARE,
        "of tool-definition bytes",
    );
    out.facts = vec![
        fact(
            "requests with tools, not compacted",
            sums.uncompacted_requests as f64,
            "requests",
        ),
        fact(
            "tool-definition bytes they carried",
            sums.uncompacted_tools_bytes as f64,
            "bytes",
        ),
        fact(
            "bytes compaction would remove",
            sums.compactable_bytes as f64,
            "bytes",
        ),
    ];
    if sums.uncompacted_requests < MIN_COMPARED {
        out.why = format!(
            "Only {} requests carried tools without compaction; {MIN_COMPARED} are needed.",
            sums.uncompacted_requests
        );
        return out;
    }
    let share = ratio(
        sums.compactable_bytes as i64,
        sums.uncompacted_tools_bytes as i64,
    );
    out.measure = Some(share);
    // Priced as a cache read: the tool block is cached after the first turn.
    out.saving = Some(sums.compactable_bytes as f64 / BYTES_PER_TOKEN * READ_RATE);
    if share >= COMPACT_SHARE {
        out.verdict = Verdict::TurnOn;
        out.value = Some("true".to_string());
        out.why = format!(
            "Schema bookkeeping is {} of the tool definitions, over the {} bar.",
            percent(share),
            percent(COMPACT_SHARE)
        );
    } else {
        out.verdict = Verdict::NotWorthIt;
        out.why = format!(
            "Schema bookkeeping is {} of the tool definitions, under the {} bar.",
            percent(share),
            percent(COMPACT_SHARE)
        );
    }
    out
}

fn recompaction(
    prefix: &PrefixReport,
    lapses: u64,
    lapse_tokens: i64,
    lapse_cost: f64,
    input_cost: f64,
) -> Advice {
    let mut out = advice(
        "cold_recompact",
        "Recompact on a lapsed cache",
        LAPSE_SHARE,
        "of input cost lost to expiry",
    );
    let margin = best_margin(&prefix.cold);
    out.facts = vec![
        fact(
            "turns that lost their prefix to expiry",
            lapses as f64,
            "turns",
        ),
        fact("tokens written again", lapse_tokens as f64, "tokens"),
        fact("what those rewrites cost", lapse_cost, "tokens"),
    ];
    if let Some(score) = margin {
        out.facts.extend([
            fact(
                "margin at which prediction held",
                f64::from(score.margin_s),
                "seconds",
            ),
            fact(
                "predictions at that margin",
                score.predicted as f64,
                "count",
            ),
            fact("their precision", score.precision, "share"),
        ]);
    }
    if prefix.compared < MIN_COMPARED {
        out.why = format!(
            "Only {} requests could be compared with the one before; {MIN_COMPARED} are needed.",
            prefix.compared
        );
        return out;
    }
    let share = if input_cost > 0.0 {
        lapse_cost / input_cost
    } else {
        0.0
    };
    out.measure = Some(share);
    if share < LAPSE_SHARE {
        out.verdict = Verdict::NotWorthIt;
        out.why = format!(
            "Expiry cost {} of input cost, under the {} bar: there are too few lapses to act on.",
            percent(share),
            percent(LAPSE_SHARE)
        );
        return out;
    }
    out.verdict = Verdict::TurnOn;
    // No saving is named: a lapsed turn is written again whatever is sent, so
    // what recompaction saves is what it removes from that write and from the
    // reads after it, and the lapse cost is only what is at stake.
    match margin {
        Some(score) => {
            out.value = Some(format!(
                "predicted, cold_recompact_margin = \"{}s\"",
                score.margin_s
            ));
            out.why = format!(
                "Expiry cost {} of input cost, and {} of {} lapses predicted at a {} s margin \
                 were real. Needs frozen_prefix = \"first_seen\".",
                percent(share),
                score.confirmed,
                score.predicted,
                score.margin_s
            );
        }
        None => {
            out.value = Some("observed".to_string());
            out.why = format!(
                "Expiry cost {} of input cost. No margin has yet shown {} precision over \
                 {PREDICT_SAMPLES} predictions, so act on the provider's own answer only. \
                 Needs frozen_prefix = \"first_seen\".",
                percent(share),
                percent(PREDICT_PRECISION)
            );
        }
    }
    out
}

fn read_decisions(sums: &Sums) -> Advice {
    let mut out = advice(
        "superseded_reads, matured_reads",
        "Rewrite finished file reads on a recompacted turn",
        READS_SHARE,
        "of conversation text on lapsed turns",
    );
    let text = sums.lapsed_text_bytes as i64;
    let stale = ratio(sums.lapsed_stale as i64, text);
    let superseded = ratio(sums.lapsed_superseded as i64, text);
    let quiet = ratio(sums.lapsed_quiet as i64, text);
    out.facts = vec![
        fact(
            "turns that lost their prefix to expiry",
            sums.lapsed_turns as f64,
            "turns",
        ),
        fact("reads a later edit invalidated", stale, "share"),
        fact("reads a later read contains", superseded, "share"),
        fact("reads left alone long enough", quiet, "share"),
    ];
    if sums.lapsed_turns < LAPSED_TURNS {
        out.why = format!(
            "Only {} observed turns lost their prefix to expiry; {LAPSED_TURNS} are needed.",
            sums.lapsed_turns
        );
        return out;
    }
    // Stale reads go with recompaction itself; these two settings add the rest.
    let share = superseded + quiet;
    out.measure = Some(share);
    // What those reads would cost to write again on the lapsed turns.
    out.saving = Some((sums.lapsed_superseded + sums.lapsed_quiet) as f64 / BYTES_PER_TOKEN);
    if share >= READS_SHARE {
        out.verdict = Verdict::TurnOn;
        let mut keys = Vec::new();
        if superseded >= READS_SHARE / 4.0 {
            keys.push("superseded_reads = true");
        }
        if quiet >= READS_SHARE / 4.0 {
            keys.push("matured_reads = true");
        }
        out.value = Some(keys.join(", "));
        out.why = format!(
            "On lapsed turns {} of the conversation text is a read a later read contains and {} \
             a read left alone. Both act only with cold_recompact on.",
            percent(superseded),
            percent(quiet)
        );
    } else {
        out.verdict = Verdict::NotWorthIt;
        out.why = format!(
            "On lapsed turns such reads are {} of the conversation text, under the {} bar.",
            percent(share),
            percent(READS_SHARE)
        );
    }
    out
}

fn reasoning(prefix: &PrefixReport) -> Advice {
    let mut out = advice(
        "reasoning_elide_models",
        "Elide earlier-turn reasoning (OpenAI Chat)",
        REASONING_SHARE,
        "of request bytes re-sent as reasoning",
    );
    let chat: Vec<_> = prefix
        .composition
        .iter()
        .filter(|model| model.api_family == "openai_chat")
        .collect();
    let requests: u64 = chat.iter().map(|model| model.requests).sum();
    let mut shares: BTreeMap<&str, f64> = BTreeMap::new();
    for model in chat.iter().filter(|model| model.requests >= MODEL_REQUESTS) {
        shares.insert(model.model.as_str(), model.reasoning_share);
    }
    out.facts = shares
        .iter()
        .map(|(model, share)| fact(&format!("reasoning in {model} requests"), *share, "share"))
        .collect();
    out.facts.insert(
        0,
        fact("OpenAI Chat requests observed", requests as f64, "requests"),
    );
    if shares.is_empty() {
        out.why =
            format!("No OpenAI Chat model has {MODEL_REQUESTS} observed requests in this window.");
        return out;
    }
    let named: Vec<&str> = shares
        .iter()
        .filter(|(_, share)| **share >= REASONING_SHARE)
        .map(|(model, _)| *model)
        .collect();
    out.measure = shares.values().copied().reduce(f64::max);
    if named.is_empty() {
        out.verdict = Verdict::NotWorthIt;
        out.why = format!(
            "No model re-sends reasoning worth {} of its requests.",
            percent(REASONING_SHARE)
        );
    } else {
        out.verdict = Verdict::TurnOn;
        out.value = Some(format!("[\"{}\"]", named.join("\", \"")));
        out.why = "These models re-send plain-text reasoning at this share of their requests. \
                   Check each provider's handling of earlier reasoning against a live request \
                   first; eliding rewrites the finished turn once per user turn."
            .to_string();
    }
    out
}

fn deferral(prefix: &PrefixReport) -> Advice {
    let mut out = advice(
        "tool deferral",
        "Defer tool definitions (Anthropic)",
        UNDEFERRED_SHARE,
        "of Anthropic requests with a long tool list and none deferred",
    );
    out.built = false;
    let anthropic: Vec<_> = prefix
        .composition
        .iter()
        .filter(|model| model.api_family == "anthropic_messages")
        .collect();
    let requests: u64 = anthropic.iter().map(|model| model.requests).sum();
    let undeferred: u64 = anthropic
        .iter()
        .map(|model| model.undeferred_requests)
        .sum();
    let bytes: u64 = anthropic
        .iter()
        .map(|model| model.undeferred_tools_bytes)
        .sum();
    out.facts = vec![
        fact("Anthropic requests observed", requests as f64, "requests"),
        fact(
            &format!("with {MANY_TOOLS} or more tools and none deferred"),
            undeferred as f64,
            "requests",
        ),
        fact("tool-definition bytes those carried", bytes as f64, "bytes"),
    ];
    if requests < MIN_COMPARED {
        out.why =
            format!("Only {requests} Anthropic requests observed; {MIN_COMPARED} are needed.");
        return out;
    }
    let share = ratio(undeferred as i64, requests as i64);
    out.measure = Some(share);
    if share >= UNDEFERRED_SHARE {
        out.verdict = Verdict::TurnOn;
        out.why = format!(
            "{} of Anthropic requests carry a long tool list whole. Deferral is not built: \
             this is the case for building it, after a probe of the live API.",
            percent(share)
        );
    } else {
        out.verdict = Verdict::NotWorthIt;
        out.why = format!(
            "{} of Anthropic requests carry a long tool list whole, under the {} bar: the \
             clients seen here defer their own tools or have few.",
            percent(share),
            percent(UNDEFERRED_SHARE)
        );
    }
    out
}
