//! Deleting part of the statistics, and trimming what the rest carries.
//!
//! [`crate::schema::erase`] is all or nothing. This is the scalpel: the rows
//! before a day, one tenant's, one benchmark run's or one experiment's — or,
//! without deleting a single request, the provenance each one carries
//! (`request_context.context`, a copy of the effective settings per request,
//! which is most of a busy `stats.db`).
//!
//! # What is kept
//!
//! - A delete scoped *only* by a day is retention brought forward: each day
//!   is rolled up into `daily_rollups`, `daily_requests` and
//!   `daily_conversations`, and the analysis history archived, before its
//!   rows go — exactly what the hourly pass would do on that day's
//!   anniversary. Nothing a dashboard draws from aggregates is lost.
//! - A delete scoped by a tenant, a run or an experiment is meant to make
//!   those requests not have happened: their rows go and nothing is rolled
//!   up from them. Aggregates already archived — days past retention — are
//!   left as they are.
//! - Trimming archives the analysis history first, so the breakdowns by
//!   configuration, run and experiment keep every request they counted; only
//!   the raw row's copy of the settings goes.
//!
//! A run or an experiment is matched by the label in `request_context`: a
//! request recorded without one cannot be told apart and is not touched.
//!
//! # Previewing
//!
//! A dry run does the work inside a transaction and rolls it back, so the
//! counts it reports are what the real thing would do, not an estimate of
//! them, and the space is the pages it would have freed. It holds the write
//! lock while it runs, as the real one does.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params_from_iter};
use serde::{Deserialize, Serialize};

use crate::error::StatsError;

const DAY_MS: i64 = 86_400_000;

/// Which requests a [`purge`] works on. Every field given narrows it; at
/// least one must be.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PurgeScope {
    /// Requests from before this UTC day: milliseconds, rounded down to the
    /// day's midnight.
    pub before_ms: Option<i64>,
    pub tenant: Option<String>,
    /// The `run_id` and `experiment_id` labels (`stats.run_id`,
    /// `stats.experiment_id`) a request was recorded under.
    pub run_id: Option<String>,
    pub experiment_id: Option<String>,
}

impl PurgeScope {
    fn is_empty(&self) -> bool {
        self.before_ms.is_none()
            && self.tenant.is_none()
            && self.run_id.is_none()
            && self.experiment_id.is_none()
    }

    /// Scoped by a day alone: retention brought forward.
    fn by_day_only(&self) -> bool {
        self.before_ms.is_some()
            && self.tenant.is_none()
            && self.run_id.is_none()
            && self.experiment_id.is_none()
    }

    fn cutoff_ms(&self) -> Option<i64> {
        self.before_ms.map(|ms| ms.div_euclid(DAY_MS) * DAY_MS)
    }

    /// Empty strings are no filter: what a blank form field sends.
    pub fn normalized(mut self) -> PurgeScope {
        let blank = |value: &mut Option<String>| {
            if value.as_deref().is_some_and(|v| v.trim().is_empty()) {
                *value = None;
            } else if let Some(v) = value {
                *v = v.trim().to_string();
            }
        };
        blank(&mut self.tenant);
        blank(&mut self.run_id);
        blank(&mut self.experiment_id);
        self
    }
}

/// Delete the rows, or only trim the copy of the settings each carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PurgeKind {
    #[default]
    Rows,
    Context,
}

/// What a [`purge`] did, or — on a dry run — would do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Purged {
    pub kind: PurgeKind,
    pub dry_run: bool,
    /// The scope as applied: `before_ms` rounded down to midnight.
    pub scope: PurgeScope,
    /// Rows deleted (or, trimming, emptied) per table; tables it left alone
    /// are not listed.
    pub tables: BTreeMap<String, u64>,
    pub rows: u64,
    /// Bytes of the file the work turned into free pages: reused by what is
    /// written next, and handed back to the disk by a vacuum.
    pub freed_bytes: u64,
    /// Trimming: the bytes of settings emptied. They mostly shrink rows
    /// inside pages rather than free whole ones, so a vacuum is what hands
    /// them back. Absent for a delete.
    pub content_bytes: Option<u64>,
    /// Whether the days were rolled up before their rows went.
    pub rolled_up: bool,
}

/// Do — or, with `dry_run`, rehearse — `kind` over `scope` on the database
/// at `path`, in one transaction.
///
/// Whoever owns the file should hold its own writes meanwhile
/// ([`crate::StatsSink::exclusive`]): a large delete holds the write lock for
/// longer than a writer's busy timeout.
pub fn purge(
    path: &Path,
    scope: PurgeScope,
    kind: PurgeKind,
    dry_run: bool,
) -> Result<Purged, StatsError> {
    let mut scope = scope.normalized();
    if scope.is_empty() {
        return Err(StatsError::Invalid(
            "say which statistics: a day to delete before, a tenant, a run or an experiment \
             (erasing everything is the danger zone's, not this)"
                .to_string(),
        ));
    }
    scope.before_ms = scope.cutoff_ms();
    let mut conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.execute_batch("PRAGMA busy_timeout=5000;")?;
    if !crate::schema::has_table(&conn, "request_context")? {
        return Err(StatsError::Invalid(
            "this statistics database predates request provenance (schema v13); let the \
             service migrate it first"
                .to_string(),
        ));
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let free_before = free_bytes(&tx)?;
    let mut tables = BTreeMap::new();
    let mut rolled_up = false;
    let mut content_bytes = None;
    match kind {
        PurgeKind::Rows if scope.by_day_only() => {
            let cutoff = scope.before_ms.unwrap_or(0);
            let context_before = count(&tx, "request_context", &format!("time_ms < {cutoff}"))?;
            let counts = crate::retention::maintain_on(&tx, cutoff, usize::MAX)?;
            let context_after = count(&tx, "request_context", &format!("time_ms < {cutoff}"))?;
            for (table, n) in [
                ("optimization_events", counts.optimization_events),
                ("retrieval_events", counts.retrieval_events),
                ("protected_ref_events", counts.protected_ref_events),
                ("proxy_requests", counts.proxy_requests),
                ("store_samples", counts.store_samples),
            ] {
                tables.insert(table.to_string(), n as u64);
            }
            tables.insert(
                "request_context".to_string(),
                context_before.saturating_sub(context_after),
            );
            rolled_up = true;
        }
        PurgeKind::Rows => {
            // `request_context` last: the run and experiment filters of the
            // other tables are read from it.
            for table in [
                "optimization_events",
                "retrieval_events",
                "protected_ref_events",
                "proxy_requests",
                "request_context",
            ] {
                let (filter, values) = filter(&scope, table);
                let n = tx.execute(
                    &format!("DELETE FROM {table} AS t WHERE {filter}"),
                    params_from_iter(values),
                )?;
                tables.insert(table.to_string(), n as u64);
            }
        }
        PurgeKind::Context => {
            // The analysis history reads the settings of requests it has not
            // archived yet: archive them first, and nothing it reports
            // changes when the copy goes.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64);
            crate::history::archive_before(&tx, scope.before_ms.unwrap_or(now).min(now))?;
            let (filter, values) = filter(&scope, "request_context");
            let bytes: i64 = tx.query_row(
                &format!(
                    "SELECT COALESCE(SUM(length(CAST(context AS BLOB))), 0)
                     FROM request_context AS t WHERE context IS NOT NULL AND {filter}"
                ),
                params_from_iter(values.clone()),
                |row| row.get(0),
            )?;
            content_bytes = Some(bytes.max(0) as u64);
            let n = tx.execute(
                &format!(
                    "UPDATE request_context AS t SET context = NULL
                     WHERE context IS NOT NULL AND {filter}"
                ),
                params_from_iter(values),
            )?;
            tables.insert("request_context".to_string(), n as u64);
        }
    }
    tables.retain(|_, n| *n > 0);
    let freed_bytes = free_bytes(&tx)?.saturating_sub(free_before);
    if dry_run {
        tx.rollback()?;
    } else {
        tx.commit()?;
    }
    Ok(Purged {
        kind,
        dry_run,
        scope,
        rows: tables.values().sum(),
        tables,
        freed_bytes,
        content_bytes,
        rolled_up,
    })
}

/// Empty the copy of the settings on up to `limit` requests from before
/// `older_than_ms`, archiving the analysis history first; return how many.
/// The writer's hourly pass (`stats.context_retention_days`), bounded so no
/// one pass holds the lock for long.
pub(crate) fn trim_contexts(
    conn: &Connection,
    older_than_ms: i64,
    limit: usize,
) -> Result<u64, StatsError> {
    crate::history::archive_before(conn, older_than_ms)?;
    Ok(conn.execute(
        "UPDATE request_context SET context = NULL WHERE rowid IN (
           SELECT rowid FROM request_context
            WHERE time_ms < ?1 AND context IS NOT NULL LIMIT ?2)",
        rusqlite::params![older_than_ms, limit as i64],
    )? as u64)
}

/// The `WHERE` clause matching `scope` on `table` (aliased `t`) and the
/// values it binds.
fn filter(scope: &PurgeScope, table: &str) -> (String, Vec<Value>) {
    let mut clauses = Vec::new();
    let mut values = Vec::new();
    let mut bind = |value: Value| {
        values.push(value);
        format!("?{}", values.len())
    };
    if let Some(before) = scope.before_ms {
        clauses.push(format!("t.time_ms < {}", bind(Value::Integer(before))));
    }
    if let Some(tenant) = &scope.tenant {
        clauses.push(format!("t.tenant = {}", bind(Value::Text(tenant.clone()))));
    }
    for (field, label) in [
        ("run_id", &scope.run_id),
        ("experiment_id", &scope.experiment_id),
    ] {
        let Some(label) = label else { continue };
        let param = bind(Value::Text(label.clone()));
        clauses.push(if table == "request_context" {
            format!("json_extract(t.context, '$.{field}') = {param}")
        } else {
            format!(
                "EXISTS (SELECT 1 FROM request_context c
                          WHERE c.tenant = t.tenant AND c.request_id = t.request_id
                            AND json_extract(c.context, '$.{field}') = {param})"
            )
        });
    }
    (clauses.join(" AND "), values)
}

fn count(conn: &Connection, table: &str, filter: &str) -> Result<u64, StatsError> {
    let n: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE {filter}"),
        [],
        |row| row.get(0),
    )?;
    Ok(n.max(0) as u64)
}

/// Bytes in free pages, as this connection sees the file — its own
/// uncommitted work included.
fn free_bytes(conn: &Connection) -> Result<u64, StatsError> {
    let free: i64 = conn.query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
    let page: i64 = conn.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    Ok((free.max(0) as u64) * (page.max(0) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scope_is_one_filter_per_field_given() {
        let scope = PurgeScope {
            before_ms: Some(5 * DAY_MS + 123),
            tenant: Some("acme".into()),
            run_id: Some("r1".into()),
            experiment_id: None,
        };
        let (clause, values) = filter(&scope, "proxy_requests");
        assert!(clause.contains("t.time_ms < ?1"), "{clause}");
        assert!(clause.contains("t.tenant = ?2"), "{clause}");
        assert!(clause.contains("'$.run_id') = ?3"), "{clause}");
        assert!(
            clause.contains("EXISTS"),
            "a run is matched through the request's context"
        );
        assert_eq!(values.len(), 3);
        let (own, _) = filter(&scope, "request_context");
        assert!(
            !own.contains("EXISTS"),
            "the context table is matched directly"
        );
    }

    #[test]
    fn blank_fields_are_no_filter_and_a_day_rounds_down() {
        let scope = PurgeScope {
            before_ms: Some(5 * DAY_MS + 123),
            tenant: Some("  ".into()),
            run_id: Some(" r1 ".into()),
            experiment_id: Some(String::new()),
        }
        .normalized();
        assert_eq!(scope.tenant, None);
        assert_eq!(scope.experiment_id, None);
        assert_eq!(scope.run_id.as_deref(), Some("r1"));
        assert_eq!(scope.cutoff_ms(), Some(5 * DAY_MS));
        assert!(PurgeScope::default().is_empty());
        assert!(
            PurgeScope {
                before_ms: Some(1),
                ..PurgeScope::default()
            }
            .by_day_only()
        );
    }
}
