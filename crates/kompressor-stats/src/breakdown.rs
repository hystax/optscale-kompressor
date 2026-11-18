//! Three CLI reports over columns every schema since v12 has: what the
//! provider billed and what the cache saved (`stats cache`), why messages were
//! left alone (`stats skips`), and what each conversation cost (`stats
//! conversations`).
//!
//! Nothing here writes or migrates. Cost is in input-equivalent tokens — every
//! token priced as a multiple of the model's fresh-input price — from the same
//! table the proxy prices its own decisions with, so a figure here can be
//! compared with the bill without a price list per currency.

use crate::{StatsError, StatsReader};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Cache prices relative to the model's fresh-input price.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Prices {
    pub read: f64,
    pub write_5m: f64,
    pub write_1h: f64,
}

/// The prices the proxy charges a row of `api_family` and `model` at.
///
/// A mirror of the proxy's `CachePrices::for_model`; a proxy unit test keeps
/// the two in step. Anthropic writes at 1.25x input (2x for one-hour entries)
/// and reads at a tenth, a twentieth for Claude Opus and Sonnet 5.5 and a
/// fortieth for Fable and Mythos 5.1. OpenAI reads at a tenth and reports no
/// writes. `anthropic/<id>` is priced as `<id>`.
pub fn prices(api_family: &str, model: Option<&str>) -> Prices {
    if api_family != "anthropic_messages" {
        return Prices {
            read: 0.1,
            write_5m: 1.0,
            write_1h: 1.0,
        };
    }
    let model = model.unwrap_or_default();
    let model = model.strip_prefix("anthropic/").unwrap_or(model);
    let read = if model.starts_with("claude-opus-5-5") || model.starts_with("claude-sonnet-5-5") {
        0.05
    } else if model.starts_with("claude-fable-5-1") || model.starts_with("claude-mythos-5-1") {
        0.025
    } else {
        0.1
    };
    Prices {
        read,
        write_5m: 1.25,
        write_1h: 2.0,
    }
}

/// Billed input split by how it was billed, with its price.
///
/// Anthropic reports fresh input, reads and writes as three disjoint counts;
/// OpenAI's prompt already contains its cached part. Both are normalised to
/// `uncached + read + write_5m + write_1h = input` per row, in SQL, before
/// anything is summed.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Billed {
    /// Rows the provider reported usage for.
    pub requests: u64,
    pub uncached: i64,
    pub read: i64,
    pub write_5m: i64,
    pub write_1h: i64,
    pub output: i64,
    /// Every input token, however billed.
    pub input: i64,
    /// `read / input`.
    pub read_share: f64,
    /// Input in input-equivalent tokens.
    pub cost: f64,
    /// The same input with no cache: every token at the fresh price.
    pub baseline: f64,
    /// `1 - cost / baseline`. Negative when writes cost more than reads saved.
    pub saved_share: f64,
}

impl Billed {
    fn add(&mut self, other: &Billed) {
        self.requests += other.requests;
        self.uncached += other.uncached;
        self.read += other.read;
        self.write_5m += other.write_5m;
        self.write_1h += other.write_1h;
        self.output += other.output;
        self.input += other.input;
        self.cost += other.cost;
    }

    fn finish(&mut self) {
        self.read_share = share(self.read, self.input);
        self.baseline = self.input as f64;
        self.saved_share = if self.baseline > 0.0 {
            1.0 - self.cost / self.baseline
        } else {
            0.0
        };
    }
}

/// `stats cache`: one model's usage.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelBilling {
    pub api_family: String,
    pub model: String,
    /// Client requests, usage reported or not.
    pub client_requests: u64,
    /// Keep-warm pings the proxy sent itself.
    pub pings: u64,
    /// Client traffic.
    pub clients: Billed,
    /// Ping traffic, kept apart because no client asked for it.
    pub ping_usage: Billed,
}

/// `stats cache`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CacheBilling {
    pub tenant: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub models: Vec<ModelBilling>,
    /// Clients and pings together, every model.
    pub total: Billed,
    /// Pings alone, every model.
    pub pings: Billed,
}

/// `stats skips`: one reason.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SkipReason {
    pub reason: String,
    pub messages: i64,
    /// Estimated tokens in those messages. Zero for `opaque`, which has no
    /// text to measure.
    pub tokens: i64,
    /// Of every skipped token in the window.
    pub token_share: f64,
}

/// `stats skips`: one engine outcome.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OutcomeRow {
    pub outcome: String,
    pub requests: u64,
    pub tokens_in: i64,
    pub saved: i64,
}

/// `stats skips`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SkipReport {
    pub tenant: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub compressions: u64,
    pub messages_total: i64,
    pub messages_compressed: i64,
    pub messages_skipped: i64,
    /// Messages inside the frozen prefix, which a cache-safe proxy leaves alone
    /// on purpose.
    pub frozen_messages: i64,
    pub skipped_tokens: i64,
    /// Largest token pool first: where the remaining headroom is.
    pub reasons: Vec<SkipReason>,
    pub outcomes: Vec<OutcomeRow>,
}

/// `stats conversations`: one conversation.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConversationRow {
    pub conversation: String,
    pub first_ms: i64,
    pub last_ms: i64,
    pub models: Vec<String>,
    pub client_turns: u64,
    pub pings: u64,
    pub errors: u64,
    pub retrieve_rounds: i64,
    /// Engine credit, discarded rewrites excluded.
    pub saved_gross: i64,
    /// Tokens retrieval put back.
    pub retrieved_tokens: i64,
    pub saved_net: i64,
    /// Clients and pings together.
    pub billed: Billed,
}

/// How `stats conversations` orders its rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConversationOrder {
    /// Most input-equivalent cost first.
    #[default]
    Cost,
    /// Most client turns first.
    Turns,
    /// Least net saving first: where compression paid least.
    Saved,
    /// Most recently active first.
    Recent,
}

/// `stats conversations`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConversationReport {
    pub tenant: String,
    pub from_ms: i64,
    pub to_ms: i64,
    /// Conversations in the window, before the limit.
    pub total: usize,
    pub conversations: Vec<ConversationRow>,
}

/// Per-row normalised usage columns of `proxy_requests`, summed: rows with
/// usage, uncached, read, write 5m, write 1h, output.
const BILLED_COLUMNS: &str = "
 SUM(CASE WHEN prompt_tokens IS NOT NULL OR cache_read_tokens IS NOT NULL
               OR cache_write_tokens IS NOT NULL THEN 1 ELSE 0 END),
 SUM(CASE WHEN api_family = 'anthropic_messages' THEN max(coalesce(prompt_tokens, 0), 0)
          ELSE max(coalesce(prompt_tokens, 0) - max(coalesce(cache_read_tokens, 0), 0), 0) END),
 SUM(max(coalesce(cache_read_tokens, 0), 0)),
 SUM(CASE WHEN api_family = 'anthropic_messages'
          THEN max(coalesce(cache_write_tokens, 0), 0)
               - min(max(coalesce(cache_write_1h_tokens, 0), 0), max(coalesce(cache_write_tokens, 0), 0))
          ELSE 0 END),
 SUM(CASE WHEN api_family = 'anthropic_messages'
          THEN min(max(coalesce(cache_write_1h_tokens, 0), 0), max(coalesce(cache_write_tokens, 0), 0))
          ELSE 0 END),
 SUM(coalesce(completion_tokens, 0))";

/// A [`Billed`] from the six [`BILLED_COLUMNS`] starting at `at`, priced.
fn billed(row: &rusqlite::Row<'_>, at: usize, prices: Prices) -> rusqlite::Result<Billed> {
    let requests: i64 = row.get(at)?;
    let uncached: i64 = row.get(at + 1)?;
    let read: i64 = row.get(at + 2)?;
    let write_5m: i64 = row.get(at + 3)?;
    let write_1h: i64 = row.get(at + 4)?;
    Ok(Billed {
        requests: requests.max(0) as u64,
        uncached,
        read,
        write_5m,
        write_1h,
        output: row.get(at + 5)?,
        input: uncached + read + write_5m + write_1h,
        cost: uncached as f64
            + read as f64 * prices.read
            + write_5m as f64 * prices.write_5m
            + write_1h as f64 * prices.write_1h,
        ..Billed::default()
    })
}

impl StatsReader {
    /// `stats cache`: billed input by model, clients and keep-warm pings apart.
    pub fn cache_billing(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<CacheBilling, StatsError> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT api_family, coalesce(model, ''), keep_warm, COUNT(*), {BILLED_COLUMNS}
               FROM proxy_requests
              WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3
              GROUP BY api_family, coalesce(model, ''), keep_warm"
        ))?;
        let mut models: BTreeMap<(String, String), ModelBilling> = BTreeMap::new();
        let mut rows = stmt.query(params![tenant, from_ms, to_ms])?;
        while let Some(row) = rows.next()? {
            let family: String = row.get(0)?;
            let model: String = row.get(1)?;
            let ping = row.get::<_, i64>(2)? != 0;
            let count = row.get::<_, i64>(3)?.max(0) as u64;
            let usage = billed(row, 4, prices(&family, Some(&model)))?;
            let entry = models
                .entry((family.clone(), model.clone()))
                .or_insert_with(|| ModelBilling {
                    api_family: family,
                    model,
                    ..ModelBilling::default()
                });
            if ping {
                entry.pings += count;
                entry.ping_usage.add(&usage);
            } else {
                entry.client_requests += count;
                entry.clients.add(&usage);
            }
        }
        let mut report = CacheBilling {
            tenant: tenant.to_string(),
            from_ms,
            to_ms,
            ..CacheBilling::default()
        };
        for mut model in models.into_values() {
            report.total.add(&model.clients);
            report.total.add(&model.ping_usage);
            report.pings.add(&model.ping_usage);
            model.clients.finish();
            model.ping_usage.finish();
            report.models.push(model);
        }
        report.total.finish();
        report.pings.finish();
        report.models.sort_by(|a, b| {
            let cost = |m: &ModelBilling| m.clients.cost + m.ping_usage.cost;
            cost(b)
                .total_cmp(&cost(a))
                .then_with(|| a.model.cmp(&b.model))
        });
        Ok(report)
    }

    /// `stats skips`: why messages reached the engine and left unchanged.
    pub fn skips(&self, tenant: &str, from_ms: i64, to_ms: i64) -> Result<SkipReport, StatsError> {
        let mut report = SkipReport {
            tenant: tenant.to_string(),
            from_ms,
            to_ms,
            ..SkipReport::default()
        };
        (
            report.compressions,
            report.messages_total,
            report.messages_compressed,
            report.messages_skipped,
            report.frozen_messages,
        ) = self.conn.query_row(
            "SELECT COUNT(*), coalesce(SUM(messages_total), 0), coalesce(SUM(messages_compressed), 0),
                    coalesce(SUM(messages_skipped), 0), coalesce(SUM(frozen_messages), 0)
               FROM optimization_events
              WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3",
            params![tenant, from_ms, to_ms],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?.max(0) as u64,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;

        // The key is quoted in the path: reasons such as `declined:text` and
        // `protected:role:system` carry punctuation a bare JSON path label
        // would have to be trusted to accept.
        let mut stmt = self.conn.prepare_cached(
            "SELECT j.key, SUM(j.value),
                    SUM(coalesce(json_extract(o.skipped_tokens_by_reason, '$.\"' || j.key || '\"'), 0))
               FROM optimization_events o, json_each(o.skipped_by_reason) j
              WHERE o.tenant = ?1 AND o.time_ms >= ?2 AND o.time_ms < ?3
                AND o.skipped_by_reason IS NOT NULL
              GROUP BY j.key",
        )?;
        report.reasons = stmt
            .query_map(params![tenant, from_ms, to_ms], |row| {
                Ok(SkipReason {
                    reason: row.get(0)?,
                    messages: row.get(1)?,
                    tokens: row.get(2)?,
                    token_share: 0.0,
                })
            })?
            .collect::<Result<_, _>>()?;
        report.skipped_tokens = report.reasons.iter().map(|r| r.tokens).sum();
        for reason in &mut report.reasons {
            reason.token_share = share(reason.tokens, report.skipped_tokens);
        }
        report.reasons.sort_by(|a, b| {
            b.tokens
                .cmp(&a.tokens)
                .then(b.messages.cmp(&a.messages))
                .then_with(|| a.reason.cmp(&b.reason))
        });

        let mut stmt = self.conn.prepare_cached(
            "SELECT coalesce(outcome, 'unrecorded'), COUNT(*), coalesce(SUM(tokens_in), 0),
                    coalesce(SUM(tokens_in - tokens_out), 0)
               FROM optimization_events
              WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3
              GROUP BY 1 ORDER BY 2 DESC, 1",
        )?;
        report.outcomes = stmt
            .query_map(params![tenant, from_ms, to_ms], |row| {
                Ok(OutcomeRow {
                    outcome: row.get(0)?,
                    requests: row.get::<_, i64>(1)?.max(0) as u64,
                    tokens_in: row.get(2)?,
                    saved: row.get(3)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(report)
    }

    /// `stats conversations`: each conversation's turns, savings and bill,
    /// ordered by `order`, at most `limit` rows.
    ///
    /// Engine credits and retrieval debits are attributed through the request
    /// ids of the conversation's client rows, each request once however many
    /// proxy rows share it. Rows with no conversation (passthrough, and the
    /// `compress = false` control arm) are not listed.
    pub fn conversations(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
        order: ConversationOrder,
        limit: usize,
    ) -> Result<ConversationReport, StatsError> {
        let mut by_id: BTreeMap<String, ConversationRow> = BTreeMap::new();
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT conversation, api_family, coalesce(model, ''), MIN(time_ms), MAX(time_ms),
                    SUM(keep_warm = 0), SUM(keep_warm),
                    SUM(CASE WHEN keep_warm = 0
                              AND (status IS NULL OR status >= 400
                                   OR (error IS NOT NULL AND error <> '')) THEN 1 ELSE 0 END),
                    SUM(CASE WHEN keep_warm = 0 THEN retrieve_rounds ELSE 0 END),
                    {BILLED_COLUMNS}
               FROM proxy_requests
              WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3 AND conversation IS NOT NULL
              GROUP BY conversation, api_family, coalesce(model, '')"
        ))?;
        let mut rows = stmt.query(params![tenant, from_ms, to_ms])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let family: String = row.get(1)?;
            let model: String = row.get(2)?;
            let usage = billed(row, 9, prices(&family, Some(&model)))?;
            let first: i64 = row.get(3)?;
            let last: i64 = row.get(4)?;
            let entry = by_id.entry(id.clone()).or_insert_with(|| ConversationRow {
                conversation: id,
                first_ms: first,
                last_ms: last,
                ..ConversationRow::default()
            });
            entry.first_ms = entry.first_ms.min(first);
            entry.last_ms = entry.last_ms.max(last);
            if !model.is_empty() && !entry.models.contains(&model) {
                entry.models.push(model);
                entry.models.sort();
            }
            entry.client_turns += row.get::<_, i64>(5)?.max(0) as u64;
            entry.pings += row.get::<_, i64>(6)?.max(0) as u64;
            entry.errors += row.get::<_, i64>(7)?.max(0) as u64;
            entry.retrieve_rounds += row.get::<_, i64>(8)?;
            entry.billed.add(&usage);
        }

        // Credits and debits per conversation. The DISTINCT pairs keep a
        // request counted once; a request id seen under two conversations
        // (which the tracker does not produce) would credit both.
        let served = self.served();
        let mut stmt = self.conn.prepare_cached(&format!(
            "WITH pc AS (
               SELECT DISTINCT request_id, conversation FROM proxy_requests
                WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3
                  AND conversation IS NOT NULL AND keep_warm = 0),
             o AS (
               SELECT request_id,
                      SUM(CASE WHEN coalesce(outcome, '') <> 'discarded'
                               THEN tokens_in - tokens_out ELSE 0 END) AS gross
                 FROM optimization_events
                WHERE tenant = ?1 AND request_id IN (SELECT request_id FROM pc)
                GROUP BY request_id),
             r AS (
               SELECT request_id,
                      SUM(CASE WHEN {served} AND found = 1 THEN returned_tokens ELSE 0 END) AS debit
                 FROM retrieval_events
                WHERE tenant = ?1 AND request_id IN (SELECT request_id FROM pc)
                GROUP BY request_id)
             SELECT pc.conversation, coalesce(SUM(o.gross), 0), coalesce(SUM(r.debit), 0)
               FROM pc LEFT JOIN o ON o.request_id = pc.request_id
                       LEFT JOIN r ON r.request_id = pc.request_id
              GROUP BY pc.conversation"
        ))?;
        let mut rows = stmt.query(params![tenant, from_ms, to_ms])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            if let Some(entry) = by_id.get_mut(&id) {
                entry.saved_gross = row.get(1)?;
                entry.retrieved_tokens = row.get(2)?;
            }
        }

        let mut conversations: Vec<ConversationRow> = by_id
            .into_values()
            .map(|mut c| {
                c.saved_net = c.saved_gross - c.retrieved_tokens;
                c.billed.finish();
                c
            })
            .collect();
        match order {
            ConversationOrder::Cost => {
                conversations.sort_by(|a, b| b.billed.cost.total_cmp(&a.billed.cost))
            }
            ConversationOrder::Turns => {
                conversations.sort_by_key(|c| std::cmp::Reverse(c.client_turns))
            }
            ConversationOrder::Saved => conversations.sort_by_key(|c| c.saved_net),
            ConversationOrder::Recent => {
                conversations.sort_by_key(|c| std::cmp::Reverse(c.last_ms))
            }
        }
        let total = conversations.len();
        conversations.truncate(limit);
        Ok(ConversationReport {
            tenant: tenant.to_string(),
            from_ms,
            to_ms,
            total,
            conversations,
        })
    }
}

fn share(part: i64, whole: i64) -> f64 {
    if whole > 0 {
        part as f64 / whole as f64
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;
    use tempfile::TempDir;

    /// A database migrated to this line's schema, with rows written as plain
    /// SQL against the v12 columns every line shares.
    fn db() -> (TempDir, rusqlite::Connection, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("stats.db");
        let conn = schema::open_read_write(&path).unwrap();
        (dir, conn, path)
    }

    #[allow(clippy::too_many_arguments)]
    fn proxy(
        conn: &rusqlite::Connection,
        time: i64,
        id: &str,
        family: &str,
        model: &str,
        conversation: Option<&str>,
        usage: (i64, i64, i64, Option<i64>, i64),
        keep_warm: bool,
    ) {
        let (prompt, read, write, write_1h, completion) = usage;
        conn.execute(
            "INSERT INTO proxy_requests (time_ms, tenant, request_id, api_family, model, upstream,
               status, stream, prompt_tokens, completion_tokens, cache_read_tokens,
               cache_write_tokens, cache_write_1h_tokens, retrieve_rounds, conversation, keep_warm)
             VALUES (?1, '', ?2, ?3, ?4, 'u', 200, 1, ?5, ?6, ?7, ?8, ?9, 1, ?10, ?11)",
            params![
                time,
                id,
                family,
                model,
                prompt,
                completion,
                read,
                write,
                write_1h,
                conversation,
                keep_warm as i64
            ],
        )
        .unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    fn compression(
        conn: &rusqlite::Connection,
        time: i64,
        id: &str,
        tin: i64,
        tout: i64,
        skips: &str,
        skip_tokens: &str,
        outcome: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO optimization_events (time_ms, tenant, request_id, surface, mode, tokens_in,
               tokens_out, chars_in, chars_out, messages_total, messages_compressed, messages_skipped,
               refs_emitted, ref_bytes_emitted, content_types, protected_by_reason, engine_ms,
               outcome, skipped_by_reason, frozen_messages, skipped_tokens_by_reason)
             VALUES (?1, '', ?2, 'proxy', 'recoverable', ?3, ?4, 0, 0, 10, 2, 8, 0, 0, '{}', '{}', 1.0,
               ?5, ?6, 3, ?7)",
            params![time, id, tin, tout, outcome, skips, skip_tokens],
        )
        .unwrap();
    }

    fn retrieval(conn: &rusqlite::Connection, time: i64, id: &str, tokens: i64, withheld: bool) {
        conn.execute(
            "INSERT INTO retrieval_events (time_ms, tenant, request_id, surface, ref, found,
               fragment_bytes, fragment_chars, fragment_tokens, returned_chars, returned_tokens, withheld)
             VALUES (?1, '', ?2, 'proxy', 'k_x', 1, 0, 0, 0, 0, ?3, ?4)",
            params![time, id, tokens, withheld as i64],
        )
        .unwrap();
    }

    #[test]
    fn cache_billing_normalises_each_family_and_prices_pings_apart() {
        let (_dir, conn, path) = db();
        let opus = "claude-opus-5-5";
        // Anthropic: 100 fresh, 1,000 read, 200 written of which 50 for an hour.
        proxy(
            &conn,
            1,
            "a",
            "anthropic_messages",
            opus,
            Some("c1"),
            (100, 1_000, 200, Some(50), 7),
            false,
        );
        // A ping that read the prefix back.
        proxy(
            &conn,
            2,
            "p",
            "anthropic_messages",
            opus,
            Some("c1"),
            (0, 1_200, 0, None, 1),
            true,
        );
        // OpenAI: the prompt of 1,000 already holds its 800 cached tokens.
        proxy(
            &conn,
            3,
            "b",
            "openai_responses",
            "gpt-5.5",
            Some("c2"),
            (1_000, 800, 0, None, 5),
            false,
        );
        drop(conn);

        let report = StatsReader::open(&path)
            .unwrap()
            .cache_billing("", 0, 10)
            .unwrap();
        let anthropic = report.models.iter().find(|m| m.model == opus).unwrap();
        assert_eq!((anthropic.client_requests, anthropic.pings), (1, 1));
        let c = &anthropic.clients;
        assert_eq!(
            (c.uncached, c.read, c.write_5m, c.write_1h, c.input),
            (100, 1_000, 150, 50, 1_300)
        );
        assert!((c.cost - (100.0 + 50.0 + 187.5 + 100.0)).abs() < 1e-9);
        assert!((anthropic.ping_usage.cost - 60.0).abs() < 1e-9);

        let openai = report.models.iter().find(|m| m.model == "gpt-5.5").unwrap();
        assert_eq!(
            (
                openai.clients.uncached,
                openai.clients.read,
                openai.clients.input
            ),
            (200, 800, 1_000)
        );
        assert!((openai.clients.cost - 280.0).abs() < 1e-9);

        assert_eq!(report.pings.read, 1_200);
        assert_eq!(report.total.input, 1_300 + 1_200 + 1_000);
        assert!(report.total.saved_share > 0.0);
        // Costliest model first.
        assert_eq!(report.models[0].model, opus);
    }

    #[test]
    fn skips_rank_reasons_by_tokens_and_list_outcomes() {
        let (_dir, conn, path) = db();
        compression(
            &conn,
            1,
            "a",
            100,
            60,
            r#"{"declined:text":4,"opaque":3}"#,
            r#"{"declined:text":900,"opaque":0}"#,
            Some("compressed"),
        );
        compression(
            &conn,
            2,
            "b",
            50,
            50,
            r#"{"below_min_chars":9}"#,
            r#"{"below_min_chars":100}"#,
            Some("discarded"),
        );
        drop(conn);

        let report = StatsReader::open(&path).unwrap().skips("", 0, 10).unwrap();
        assert_eq!(report.compressions, 2);
        assert_eq!(report.frozen_messages, 6);
        assert_eq!(report.skipped_tokens, 1_000);
        let names: Vec<_> = report.reasons.iter().map(|r| r.reason.as_str()).collect();
        assert_eq!(names, ["declined:text", "below_min_chars", "opaque"]);
        assert!((report.reasons[0].token_share - 0.9).abs() < 1e-9);
        assert_eq!(report.reasons[2].messages, 3);
        let outcomes: Vec<_> = report
            .outcomes
            .iter()
            .map(|o| (o.outcome.as_str(), o.saved))
            .collect();
        assert!(outcomes.contains(&("compressed", 40)) && outcomes.contains(&("discarded", 0)));
    }

    #[test]
    fn conversations_join_credits_and_debits_once_per_request() {
        let (_dir, conn, path) = db();
        let opus = "claude-opus-5-5";
        proxy(
            &conn,
            1,
            "a",
            "anthropic_messages",
            opus,
            Some("c1"),
            (1_000, 0, 1_000, None, 1),
            false,
        );
        proxy(
            &conn,
            5,
            "b",
            "anthropic_messages",
            opus,
            Some("c1"),
            (10, 1_000, 10, None, 1),
            false,
        );
        proxy(
            &conn,
            6,
            "p",
            "anthropic_messages",
            opus,
            Some("c1"),
            (0, 1_010, 0, None, 1),
            true,
        );
        proxy(
            &conn,
            2,
            "z",
            "anthropic_messages",
            opus,
            Some("c2"),
            (50, 0, 0, None, 1),
            false,
        );
        proxy(
            &conn,
            3,
            "n",
            "anthropic_messages",
            opus,
            None,
            (999, 0, 0, None, 1),
            false,
        );
        compression(&conn, 1, "a", 500, 200, "{}", "{}", None);
        compression(&conn, 5, "b", 500, 200, "{}", "{}", None);
        retrieval(&conn, 5, "b", 40, false);
        retrieval(&conn, 5, "b", 999, true);
        drop(conn);

        let reader = StatsReader::open(&path).unwrap();
        let report = reader
            .conversations("", 0, 10, ConversationOrder::Cost, 10)
            .unwrap();
        assert_eq!(report.total, 2, "a row with no conversation is not listed");
        let c1 = &report.conversations[0];
        assert_eq!(c1.conversation, "c1");
        assert_eq!((c1.client_turns, c1.pings, c1.retrieve_rounds), (2, 1, 2));
        assert_eq!(
            (c1.saved_gross, c1.retrieved_tokens, c1.saved_net),
            (600, 40, 560)
        );
        assert_eq!((c1.first_ms, c1.last_ms), (1, 6));
        assert_eq!(c1.models, [opus]);

        let recent = reader
            .conversations("", 0, 10, ConversationOrder::Turns, 1)
            .unwrap();
        assert_eq!((recent.total, recent.conversations.len()), (2, 1));
        assert_eq!(recent.conversations[0].conversation, "c1");
    }
}
