//! Retention: roll a day up, then let the rows go.
//!
//! An event store that only ever grows is an outage waiting for a quiet
//! weekend, but deleting raw events deletes the answer to "what did we save
//! last quarter". So the two operations here are a pair, and the order matters:
//! roll a day up first, prune afterwards. `daily_rollups` is small enough to
//! keep forever and is the only thing that can answer a question from beyond
//! the retention window.

use crate::error::StatsError;
use crate::writer::StatsWriter;
use rusqlite::params;
use serde::{Deserialize, Serialize};

/// Rows removed by one [`StatsWriter::prune`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneCounts {
    /// Compression events removed.
    pub optimization_events: usize,
    /// Retrieval events removed.
    pub retrieval_events: usize,
    /// Protected-handle events removed.
    pub protected_ref_events: usize,
    /// Proxy rows removed.
    pub proxy_requests: usize,
    /// Store samples removed.
    pub store_samples: usize,
}

impl PruneCounts {
    /// Every row removed, across all five tables.
    pub fn total(&self) -> usize {
        self.optimization_events
            + self.retrieval_events
            + self.protected_ref_events
            + self.proxy_requests
            + self.store_samples
    }
}

/// Days archived in one maintenance pass. Thirty is a month of backlog per
/// hourly pass: a year of unpruned history catches up in half a day of
/// uptime, and no single transaction is large enough to stall the writer.
const MAX_DAYS_PER_PASS: usize = 30;

impl StatsWriter {
    /// Delete every event older than `older_than_ms`.
    ///
    /// One transaction across all five tables so a report cannot be taken
    /// half-way through a prune and see a day that still has its retrievals but
    /// has lost the compressions they were a debit against — which would read
    /// as a large negative saving that never happened.
    ///
    /// `daily_rollups` is never pruned: it is one row per day, tenant, mode and
    /// surface, and keeping it is what makes pruning safe.
    pub fn prune(&mut self, older_than_ms: i64) -> Result<PruneCounts, StatsError> {
        let conn = self.connection_mut();
        let tx = conn.transaction()?;
        let counts = prune_on(&tx, older_than_ms)?;
        tx.commit()?;
        Ok(counts)
    }

    /// Recompute `daily_rollups` for `day` (`YYYY-MM-DD`, UTC).
    ///
    /// Full recompute and replace rather than an incremental add, so running it
    /// twice for the same day is a no-op rather than a doubling — which matters
    /// because the daily job will be re-run by hand the first time anyone
    /// doubts a number.
    ///
    /// Retrievals are attributed to the mode of the compression that shares
    /// their `request_id`, and to `unknown` when there is no such row. Dropping
    /// the unattributable ones instead would quietly delete debits and leave
    /// only credits, making the preserved aggregate flattering rather than
    /// merely incomplete.
    pub fn rollup(&mut self, day: &str) -> Result<usize, StatsError> {
        let conn = self.connection_mut();
        let tx = conn.transaction()?;
        // A recompute is only a recompute while the rows it computes from are
        // still here. Once retention has archived and pruned a day, this would
        // delete the archive and rebuild it from nothing — turning the one
        // surviving copy into zero. Refuse instead: there is nothing to
        // recompute from, and the existing archive is already the answer.
        let raw: i64 = tx.query_row(
            "SELECT EXISTS (
               SELECT 1 FROM optimization_events
                WHERE date(time_ms/1000,'unixepoch') = ?1
               UNION ALL
               SELECT 1 FROM retrieval_events
                WHERE date(time_ms/1000,'unixepoch') = ?1)",
            params![day],
            |row| row.get(0),
        )?;
        if raw == 0 {
            return Ok(0);
        }
        tx.execute("DELETE FROM daily_rollups WHERE day = ?1", params![day])?;
        let written = rollup_on(&tx, day, true)?;
        for table in ["optimization_events", "retrieval_events"] {
            tx.execute(
                &format!(
                    "UPDATE {table} SET rolled_up = 1 WHERE date(time_ms/1000,'unixepoch') = ?1"
                ),
                params![day],
            )?;
        }
        tx.commit()?;
        Ok(written)
    }
    /// Archive and remove complete UTC days outside the raw retention window.
    /// Rollups and deletion commit together; retries never double-count rows.
    pub fn maintain(&mut self, older_than_ms: i64) -> Result<PruneCounts, StatsError> {
        self.maintain_bounded(older_than_ms, MAX_DAYS_PER_PASS)
    }

    /// [`StatsWriter::maintain`], with the number of days archived in one
    /// transaction capped.
    ///
    /// The cap exists because the first pass after an upgrade can face years of
    /// rows, and all of this — every day's rollup plus the delete — is one
    /// transaction holding the write lock. Unbounded, that stalls the writer
    /// thread and backs events up behind it. Capped, a long backlog is caught
    /// up an hour at a time and nothing waits. Pruning still uses the full
    /// cutoff when the backlog is clear, so the steady state is one pass.
    pub fn maintain_bounded(
        &mut self,
        older_than_ms: i64,
        max_days: usize,
    ) -> Result<PruneCounts, StatsError> {
        // A zero cap archives nothing, and the horizon below would still fall
        // through to the full cutoff — deleting every eligible row with no copy
        // taken. Nothing to do is the only safe reading of "zero days".
        if max_days == 0 {
            return Ok(PruneCounts::default());
        }
        let cutoff = older_than_ms.div_euclid(86_400_000) * 86_400_000;
        let tx = self.connection_mut().transaction()?;
        let days = {
            let mut statement = tx.prepare(
                "SELECT DISTINCT date(time_ms/1000,'unixepoch') FROM (
                   SELECT time_ms FROM optimization_events WHERE time_ms < ?1
                   UNION ALL SELECT time_ms FROM retrieval_events WHERE time_ms < ?1
                   UNION ALL SELECT time_ms FROM proxy_requests WHERE time_ms < ?1
                 ) ORDER BY 1",
            )?;

            statement
                .query_map(params![cutoff], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        // When the backlog is longer than one pass allows, prune only as far as
        // the days actually archived: deleting past that would drop rows whose
        // rollup has not been written yet.
        let capped = days.len() > max_days;
        let days = &days[..days.len().min(max_days)];
        for day in days {
            rollup_on(&tx, day, false)?;
            requests_rollup_on(&tx, day, false)?;
            conversations_rollup_on(&tx, day, false)?;
        }
        let horizon = match (capped, days.last()) {
            (true, Some(last)) => day_after_ms(&tx, last)?.min(cutoff),
            _ => cutoff,
        };
        let counts = prune_on(&tx, horizon)?;
        tx.commit()?;
        Ok(counts)
    }
}

fn rollup_on(
    conn: &rusqlite::Connection,
    day: &str,
    include_rolled: bool,
) -> Result<usize, StatsError> {
    Ok(conn.execute(
        "INSERT INTO daily_rollups
                 (day, tenant, mode, surface, requests, tokens_in, tokens_out,
                  retrievals, retrieval_hits, retrieved_tokens)
             SELECT day, tenant, mode, surface,
                    SUM(requests), SUM(tokens_in), SUM(tokens_out),
                    SUM(retrievals), SUM(hits), SUM(retrieved)
             FROM (
               SELECT date(time_ms/1000,'unixepoch') AS day, tenant, mode, surface,
                      1 AS requests, tokens_in AS tokens_in, (CASE WHEN coalesce(outcome,'') = 'discarded' THEN tokens_in ELSE tokens_out END) AS tokens_out,
                      0 AS retrievals, 0 AS hits, 0 AS retrieved
                 FROM optimization_events
                WHERE date(time_ms/1000,'unixepoch') = ?1 AND (?2 OR rolled_up = 0)
               UNION ALL
               SELECT date(r.time_ms/1000,'unixepoch') AS day, r.tenant,
                      COALESCE((SELECT o.mode FROM optimization_events o
                                WHERE o.tenant = r.tenant AND o.request_id = r.request_id
                                ORDER BY o.time_ms, o.id LIMIT 1), 'unknown') AS mode,
                      r.surface, 0, 0, 0,
                      1 AS retrievals, r.found AS hits,
                      CASE WHEN r.found = 1 THEN r.returned_tokens ELSE 0 END AS retrieved
                 FROM retrieval_events r
                WHERE date(r.time_ms/1000,'unixepoch') = ?1 AND (?2 OR r.rolled_up = 0)
             )
             GROUP BY day, tenant, mode, surface
             ON CONFLICT(day, tenant, mode, surface) DO UPDATE SET
               requests = daily_rollups.requests + excluded.requests,
               tokens_in = daily_rollups.tokens_in + excluded.tokens_in,
               tokens_out = daily_rollups.tokens_out + excluded.tokens_out,
               retrievals = daily_rollups.retrievals + excluded.retrievals,
               retrieval_hits = daily_rollups.retrieval_hits + excluded.retrieval_hits,
               retrieved_tokens = daily_rollups.retrieved_tokens + excluded.retrieved_tokens",
        params![day, include_rolled],
    )?)
}

/// The billed-prompt side of a day, archived before the raw rows go.
///
/// Separate from [`rollup_on`] because the grain differs: a proxy request
/// carries no `mode` or `surface`. Only additive quantities live in the table;
/// conversation identities go to `daily_conversations`, where a set union keeps
/// the count exact across however many passes a day takes to archive.
fn requests_rollup_on(
    conn: &rusqlite::Connection,
    day: &str,
    include_rolled: bool,
) -> Result<usize, StatsError> {
    Ok(conn.execute(
        "INSERT INTO daily_requests
             (day, tenant, requests, prompt_tokens, cache_read_tokens,
              cache_write_tokens, completion_tokens)
         SELECT date(time_ms/1000,'unixepoch') AS day, tenant,
                COUNT(*),
                COALESCE(SUM(prompt_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cache_write_tokens), 0),
                COALESCE(SUM(completion_tokens), 0)
           FROM proxy_requests
          WHERE date(time_ms/1000,'unixepoch') = ?1 AND (?2 OR rolled_up = 0)
          GROUP BY day, tenant
         ON CONFLICT(day, tenant) DO UPDATE SET
           requests = daily_requests.requests + excluded.requests,
           prompt_tokens = daily_requests.prompt_tokens + excluded.prompt_tokens,
           cache_read_tokens = daily_requests.cache_read_tokens + excluded.cache_read_tokens,
           cache_write_tokens = daily_requests.cache_write_tokens + excluded.cache_write_tokens,
           completion_tokens = daily_requests.completion_tokens + excluded.completion_tokens",
        params![day, include_rolled],
    )?)
}

/// The conversations active on a day, unioned into the archive.
///
/// `INSERT OR IGNORE` against the primary key makes this idempotent: a day
/// archived in two passes, or a pass repeated after a crash, converges on the
/// same set. A row with no conversation is skipped — a passthrough request
/// belongs to no conversation, and counting it as one would inflate exactly the
/// number this table exists to get right.
///
/// The `conversation IS NOT NULL` filter is belt-and-braces: the column is
/// `NOT NULL` and `INSERT OR IGNORE` already drops the violation, so removing
/// the filter changes no behaviour and no test can tell the difference. It
/// stays because depending on a constraint violation being silently swallowed
/// is a fragile way to express "skip these", and it would stop being true the
/// day someone made the column nullable.
fn conversations_rollup_on(
    conn: &rusqlite::Connection,
    day: &str,
    include_rolled: bool,
) -> Result<usize, StatsError> {
    Ok(conn.execute(
        "INSERT OR IGNORE INTO daily_conversations (day, tenant, conversation)
         SELECT date(time_ms/1000,'unixepoch'), tenant, conversation
           FROM proxy_requests
          WHERE date(time_ms/1000,'unixepoch') = ?1
            AND conversation IS NOT NULL
            AND (?2 OR rolled_up = 0)",
        params![day, include_rolled],
    )?)
}

/// Midnight UTC after `day`, in epoch milliseconds — the exclusive upper bound
/// for the rows a pass that stopped at `day` is allowed to delete.
fn day_after_ms(conn: &rusqlite::Connection, day: &str) -> Result<i64, StatsError> {
    Ok(conn.query_row(
        "SELECT CAST(strftime('%s', ?1, '+1 day') AS INTEGER) * 1000",
        params![day],
        |row| row.get(0),
    )?)
}

fn prune_on(conn: &rusqlite::Connection, older_than_ms: i64) -> Result<PruneCounts, StatsError> {
    let counts = PruneCounts {
        optimization_events: conn.execute(
            "DELETE FROM optimization_events WHERE time_ms < ?1",
            params![older_than_ms],
        )?,
        retrieval_events: conn.execute(
            "DELETE FROM retrieval_events WHERE time_ms < ?1",
            params![older_than_ms],
        )?,
        protected_ref_events: conn.execute(
            "DELETE FROM protected_ref_events WHERE time_ms < ?1",
            params![older_than_ms],
        )?,
        proxy_requests: conn.execute(
            "DELETE FROM proxy_requests WHERE time_ms < ?1",
            params![older_than_ms],
        )?,
        store_samples: conn.execute(
            "DELETE FROM store_samples WHERE time_ms < ?1",
            params![older_than_ms],
        )?,
    };
    Ok(counts)
}
