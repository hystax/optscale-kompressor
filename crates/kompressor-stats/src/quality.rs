//! How far the other dashboards can be trusted.
//!
//! Every figure elsewhere is computed from rows the writer filed; each is only
//! as good as the columns it reads. This report measures, for the window, how
//! many rows carry the columns the dashboards depend on — provider usage, a
//! conversation, upstream timing, the run context, the engine's outcome and
//! skip reasons, a retrieval's request — and states what the database cannot
//! hold at all: events the statistics queue dropped, rows pruned by retention,
//! days not yet archived, windows too busy to read whole.
//!
//! Each figure is a [`Check`] like the health overview's, so the two pages
//! draw the same way. A coverage check turns warn and bad *below* its limits
//! (`below`); a count turns warn at or above them. Columns this build never
//! writes are reported as `info`, not as failures: an empty column nobody
//! fills is a fact about the schema, not about the data.

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::dashboards::MAX_ROWS;
use crate::health::{Check, Level, MIN_SAMPLES, Unit};
use crate::{StatsError, StatsReader};

/// What the process knows that the database does not.
#[derive(Clone, Debug, Default)]
pub struct QualityRuntime {
    pub now_ms: i64,
    pub stats_dropped: u64,
    /// Days raw rows are kept; 0 when pruning is off.
    pub retention_days: u32,
}

/// One table's rows in the window.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TableRows {
    pub table: String,
    pub rows: u64,
    pub first_ms: Option<i64>,
    pub last_ms: Option<i64>,
    /// More rows than one report reads: views over it are truncated.
    pub over_limit: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QualityReport {
    pub tenant: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub level: Level,
    pub checks: Vec<Check>,
    pub tables: Vec<TableRows>,
    /// The tenant's earliest raw row in any table, at any time.
    pub raw_from_ms: Option<i64>,
    /// The last day folded into the daily archive, `YYYY-MM-DD`.
    pub archived_through: Option<String>,
    pub retention_days: u32,
}

struct Checks(Vec<Check>);

impl Checks {
    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        id: &str,
        area: &str,
        title: &str,
        level: Level,
        value: Option<f64>,
        unit: Unit,
        detail: String,
        limits: (Option<f64>, Option<f64>),
        below: bool,
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
            warn: limits.0,
            bad: limits.1,
            below,
            view: view.into(),
        });
    }

    /// `part` of `of` rows carry something that should be there: warn below
    /// `warn`, bad below `bad`; not judged over fewer than [`MIN_SAMPLES`].
    #[allow(clippy::too_many_arguments)]
    fn coverage(
        &mut self,
        id: &str,
        area: &str,
        title: &str,
        (part, of): (u64, u64),
        noun: &str,
        warn: f64,
        bad: Option<f64>,
        view: &str,
    ) {
        let value = (of > 0).then(|| part as f64 / of as f64);
        let level = match value {
            Some(v) if of >= MIN_SAMPLES => {
                if bad.is_some_and(|bad| v < bad) {
                    Level::Bad
                } else if v < warn {
                    Level::Warn
                } else {
                    Level::Ok
                }
            }
            _ => Level::Info,
        };
        let mut detail = format!("{part} of {of} {noun}");
        if of < MIN_SAMPLES {
            detail.push_str(" — too few to judge");
        }
        self.add(
            id,
            area,
            title,
            level,
            value,
            Unit::Share,
            detail,
            (Some(warn), bad),
            true,
            view,
        );
    }

    /// A column this build may never write: reported, never judged.
    #[allow(clippy::too_many_arguments)]
    fn optional(
        &mut self,
        id: &str,
        area: &str,
        title: &str,
        (part, of): (u64, u64),
        noun: &str,
        unwritten: &str,
        view: &str,
    ) {
        let value = (of > 0).then(|| part as f64 / of as f64);
        let detail = if part == 0 && of > 0 {
            format!("0 of {of} {noun}: {unwritten}")
        } else {
            format!("{part} of {of} {noun}")
        };
        self.add(
            id,
            area,
            title,
            Level::Info,
            value,
            Unit::Share,
            detail,
            (None, None),
            false,
            view,
        );
    }
}

fn u(n: i64) -> u64 {
    n.max(0) as u64
}

impl StatsReader {
    /// Column coverage and the database's limits for `tenant` over
    /// `[from_ms, to_ms)`.
    pub fn quality(
        &self,
        tenant: &str,
        from_ms: i64,
        to_ms: i64,
        runtime: &QualityRuntime,
    ) -> Result<QualityReport, StatsError> {
        let _snapshot = self.conn.unchecked_transaction()?;
        let window = (tenant, from_ms, to_ms);
        let mut c = Checks(Vec::new());

        // ---- proxied requests. "Answered model calls" are the rows that
        // should carry usage: a model call (not a pass-through, upgrade or
        // ping) the upstream answered 2xx without an error.
        let (rows, answered, usage, conversation, engine, timing, context, stripped, shadow): (
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
        ) = self.conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(a), 0),
                    COALESCE(SUM(a AND prompt_tokens IS NOT NULL), 0),
                    COALESCE(SUM(a AND conversation IS NOT NULL), 0),
                    COALESCE(SUM(a AND EXISTS (SELECT 1 FROM optimization_events o
                       WHERE o.tenant = p.tenant AND o.request_id = p.request_id)), 0),
                    COALESCE(SUM(timing_json IS NOT NULL), 0),
                    COALESCE(SUM(EXISTS (SELECT 1 FROM request_context c
                       WHERE c.tenant = p.tenant AND c.request_id = p.request_id)), 0),
                    COALESCE(SUM(strip_usage), 0),
                    COALESCE(SUM(a AND shadow_prompt_tokens IS NOT NULL), 0)
               FROM (SELECT *, (api_family <> 'passthrough' AND keep_warm = 0
                                AND status BETWEEN 200 AND 299 AND error IS NULL) AS a
                       FROM proxy_requests
                      WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3) p",
            window,
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            },
        )?;
        let (rows, answered) = (u(rows), u(answered));
        c.coverage(
            "usage",
            "requests",
            "Provider usage reported",
            (u(usage), answered),
            "answered model calls",
            0.95,
            Some(0.80),
            "cache",
        );
        c.coverage(
            "conversation",
            "requests",
            "Filed under a conversation",
            (u(conversation), answered),
            "answered model calls",
            0.90,
            Some(0.50),
            "cache",
        );
        c.add(
            "engine_seen", "requests", "Seen by the engine", Level::Info,
            (answered > 0).then(|| u(engine) as f64 / answered as f64), Unit::Share,
            format!("{} of {answered} answered model calls had an engine run; the rest were relayed as they came", u(engine)),
            (None, None), false, "engine",
        );
        c.coverage(
            "timing",
            "requests",
            "Upstream timing recorded",
            (u(timing), rows),
            "proxied rows",
            0.90,
            Some(0.50),
            "upstream",
        );
        c.coverage(
            "context",
            "requests",
            "Run context recorded",
            (u(context), rows),
            "proxied rows",
            0.90,
            Some(0.50),
            "trends",
        );
        c.add(
            "usage_stripped", "requests", "Usage stripped from responses", Level::Info,
            Some(u(stripped) as f64), Unit::Count,
            "requests whose usage the proxy removed before the client saw it; the proxy still counted it".into(),
            (None, None), false, "cache",
        );
        c.optional("shadow", "requests", "Uncompressed prompt measured", (u(shadow), answered), "answered model calls", "this build does not measure the uncompressed prompt, so savings are the engine's estimate", "trends");

        // ---- engine runs
        let (runs, outcome, skips, duplicates): (i64, i64, i64, i64) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(outcome IS NOT NULL), 0),
                    COALESCE(SUM(skipped_tokens_by_reason IS NOT NULL), 0),
                    COUNT(*) - COUNT(DISTINCT request_id)
               FROM optimization_events WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3",
            window,
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let runs = u(runs);
        c.coverage(
            "outcome",
            "engine",
            "Outcome recorded",
            (u(outcome), runs),
            "engine runs",
            0.99,
            Some(0.90),
            "engine",
        );
        c.coverage(
            "skip_reasons",
            "engine",
            "Skip reasons recorded",
            (u(skips), runs),
            "engine runs",
            0.99,
            Some(0.90),
            "yield",
        );
        let dup_share = (runs > 0).then(|| u(duplicates) as f64 / runs as f64);
        c.add(
            "duplicate_runs", "engine", "Requests with more than one run",
            match dup_share {
                Some(v) if runs >= MIN_SAMPLES && v >= 0.10 => Level::Bad,
                Some(v) if runs >= MIN_SAMPLES && v >= 0.02 => Level::Warn,
                Some(_) if runs >= MIN_SAMPLES => Level::Ok,
                _ => Level::Info,
            },
            dup_share, Unit::Share,
            format!("{} extra runs under a request id already seen, of {runs} runs; per-request joins keep one", u(duplicates)),
            (Some(0.02), Some(0.10)), false, "engine",
        );

        // ---- retrievals
        let (asks, with_request, with_context, ranged, ranged_status): (i64, i64, i64, i64, i64) =
            self.conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(request_id IS NOT NULL), 0),
                    COALESCE(SUM(context_json IS NOT NULL), 0), COALESCE(SUM(ranged), 0),
                    COALESCE(SUM(ranged AND range_status IS NOT NULL), 0)
               FROM retrieval_events WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3",
                window,
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?;
        let asks = u(asks);
        c.coverage(
            "retrieval_request",
            "retrieval",
            "Retrievals tied to a request",
            (u(with_request), asks),
            "retrievals",
            0.95,
            Some(0.80),
            "store",
        );
        c.coverage(
            "retrieval_context",
            "retrieval",
            "Retrieval context recorded",
            (u(with_context), asks),
            "retrievals",
            0.90,
            Some(0.50),
            "store",
        );
        c.optional(
            "range_status",
            "retrieval",
            "Ranged retrievals with a range status",
            (u(ranged_status), u(ranged)),
            "ranged retrievals",
            "recorded only by builds that know the range outcome",
            "store",
        );

        // ---- what the database cannot hold
        c.add(
            "stats_dropped",
            "limits",
            "Dropped statistics",
            if runtime.stats_dropped > 0 {
                Level::Warn
            } else {
                Level::Ok
            },
            Some(runtime.stats_dropped as f64),
            Unit::Count,
            if runtime.stats_dropped > 0 {
                "events the queue dropped since start never reached the database".into()
            } else {
                "none since start".into()
            },
            (Some(1.0), None),
            false,
            "engine",
        );
        let raw_from_ms: Option<i64> = self.conn.query_row(
            "SELECT MIN(t) FROM (SELECT MIN(time_ms) t FROM optimization_events WHERE tenant = ?1
               UNION ALL SELECT MIN(time_ms) FROM retrieval_events WHERE tenant = ?1
               UNION ALL SELECT MIN(time_ms) FROM proxy_requests WHERE tenant = ?1)",
            [tenant],
            |r| r.get(0),
        )?;
        let pruned = raw_from_ms.is_some_and(|first| first > from_ms)
            && runtime.retention_days > 0
            && from_ms < runtime.now_ms - i64::from(runtime.retention_days) * 86_400_000;
        c.add(
            "retention", "limits", "Window within retention",
            if pruned { Level::Warn } else { Level::Ok },
            raw_from_ms.map(|first| (runtime.now_ms - first).max(0) as f64), Unit::Ms,
            match (pruned, runtime.retention_days) {
                (true, days) => format!("raw rows are kept {days} days; the start of this window was pruned and only archived days remain"),
                (false, 0) => "pruning is off; the value is the age of the oldest raw row".into(),
                (false, days) => format!("raw rows are kept {days} days; the value is the age of the oldest"),
            },
            (None, None), false, "trends",
        );
        let archived_through: Option<String> = self
            .conn
            .query_row(
                "SELECT MAX(day) FROM (SELECT MAX(day) day FROM daily_requests WHERE tenant = ?1
                   UNION ALL SELECT MAX(day) FROM daily_rollups WHERE tenant = ?1)",
                [tenant],
                |r| r.get(0),
            )
            .optional()?
            .flatten();

        // ---- the tables themselves
        let mut tables = Vec::new();
        for (table, limited) in [
            ("proxy_requests", true),
            ("optimization_events", true),
            ("retrieval_events", true),
            ("protected_ref_events", false),
            ("request_context", false),
        ] {
            let (n, first, last): (i64, Option<i64>, Option<i64>) = self.conn.query_row(
                &format!(
                    "SELECT COUNT(*), MIN(time_ms), MAX(time_ms) FROM {table}
                      WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3"
                ),
                window,
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            tables.push(TableRows {
                table: table.into(),
                rows: u(n),
                first_ms: first,
                last_ms: last,
                over_limit: limited && u(n) > MAX_ROWS as u64,
            });
        }
        let busy: Vec<&str> = tables
            .iter()
            .filter(|t| t.over_limit)
            .map(|t| t.table.as_str())
            .collect();
        c.add(
            "row_limit",
            "limits",
            "Window small enough to read whole",
            if busy.is_empty() {
                Level::Ok
            } else {
                Level::Warn
            },
            None,
            Unit::Count,
            if busy.is_empty() {
                format!("every table under {MAX_ROWS} rows; no report is truncated")
            } else {
                format!(
                    "{} over {MAX_ROWS} rows: reports read only the newest; pick a shorter window",
                    busy.join(", ")
                )
            },
            (None, None),
            false,
            "trends",
        );

        let checks = c.0;
        let level = checks
            .iter()
            .map(|c| c.level)
            .filter(|l| *l != Level::Info)
            .max()
            .unwrap_or(Level::Ok);
        Ok(QualityReport {
            tenant: tenant.to_string(),
            from_ms,
            to_ms,
            level,
            checks,
            tables,
            raw_from_ms,
            archived_through,
            retention_days: runtime.retention_days,
        })
    }
}

#[cfg(test)]
#[path = "quality_tests.rs"]
mod tests;
