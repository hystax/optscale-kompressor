//! The SQLite backend: the production default.
//!
//! A fragment store is a peculiar workload — write-once per handle, read rarely
//! (the model asks for a minority of what it is offered), delete by policy, and
//! absolutely not worth losing a request over. SQLite in WAL mode fits it: one
//! file to point at encrypted or ephemeral storage, no daemon to operate, and
//! readers that do not block the writer. That last property is the reason WAL
//! is not optional here: retrievals for handles minted seconds ago land while a
//! compression is still flushing its batch, and under the rollback journal each
//! of those reads would wait on the write.
//!
//! `refs.db` is deliberately a separate file from `stats.db`: different write
//! patterns, different retention, and only this one holds payload bytes, so
//! only this one has to be placed with care.

use crate::browse::{BrowseOrder, BrowsePage, BrowseQuery, RefBrowse, RefDetail, RefSummary};
use crate::clock::Clock;
use crate::types::{
    Lookup, PutMeta, RefRecord, RefStore, StoreError, StoreStats, Tenant, expires_at, is_expired,
};
use kompressor_engine::refs::RefId;
use parking_lot::Mutex;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use std::path::Path;

/// The default size cap on live fragments.
pub const DEFAULT_MAX_BYTES: u64 = 1024 * 1024 * 1024;

/// How many rows one purge or eviction statement touches.
///
/// Bounded batches keep a sweep that finds a million dead rows from holding the
/// write lock for the length of a single transaction, which would stall every
/// compression flushing behind it.
const BATCH_ROWS: usize = 5_000;

/// Schema version 1 — [DESIGN.md Appendix C.1].
///
/// `fragment` is a BLOB rather than TEXT so SQLite's type affinity cannot get
/// an opinion about payload that merely looks numeric, and `WITHOUT ROWID`
/// keeps the row in the primary-key b-tree: every access here is a point read
/// on `(tenant, ref)`, so the extra indirection would be pure cost.
const SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS refs (
  tenant        TEXT    NOT NULL,
  ref           TEXT    NOT NULL,
  fragment      BLOB    NOT NULL,
  bytes         INTEGER NOT NULL,
  kind          TEXT    NOT NULL,
  request_id    TEXT,
  created_at    INTEGER NOT NULL,
  last_accessed INTEGER NOT NULL,
  ttl_seconds   INTEGER NOT NULL,
  max_lifetime  INTEGER NOT NULL,
  PRIMARY KEY (tenant, ref)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS refs_expiry ON refs (last_accessed);
";

/// Schema migrations, applied in **numeric** order of the `u32`.
///
/// The ordering is spelled out because the usual shortcut — sorting migration
/// *filenames* as strings — puts `V10_` before `V2_`, so the tenth migration
/// runs first the moment a tenth one exists. Keying on an integer and iterating
/// the array cannot express that bug.
const MIGRATIONS: &[(u32, &str)] = &[(1, SCHEMA_V1)];

const SQL_UPSERT: &str = "
INSERT INTO refs (tenant, ref, fragment, bytes, kind, request_id,
                  created_at, last_accessed, ttl_seconds, max_lifetime)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
ON CONFLICT(tenant, ref) DO UPDATE SET
  fragment      = excluded.fragment,
  bytes         = excluded.bytes,
  last_accessed = excluded.last_accessed,
  ttl_seconds   = excluded.ttl_seconds,
  max_lifetime  = excluded.max_lifetime,
  kind          = excluded.kind,
  request_id    = coalesce(excluded.request_id, refs.request_id)
RETURNING created_at
";

const SQL_GET: &str = "
SELECT fragment, kind, request_id, created_at, last_accessed, ttl_seconds, max_lifetime
FROM refs WHERE tenant = ?1 AND ref = ?2
";

const SQL_DELETE_ONE: &str = "DELETE FROM refs WHERE tenant = ?1 AND ref = ?2";

/// The liveness predicate is repeated in SQL rather than filtered in Rust so a
/// touch cannot resurrect an entry the absolute cap already killed.
const SQL_TOUCH: &str = "
UPDATE refs SET last_accessed = ?3
WHERE tenant = ?1 AND ref = ?2
  AND last_accessed + ttl_seconds >= ?3
  AND created_at + max_lifetime >= ?3
";

/// A `WITHOUT ROWID` table has no `rowid`, so the bounded sub-select carries the
/// primary key instead — the shape the design's batch-delete sketch assumed.
const SQL_PURGE: &str = "
DELETE FROM refs WHERE (tenant, ref) IN (
  SELECT tenant, ref FROM refs
  WHERE last_accessed + ttl_seconds < ?1 OR created_at + max_lifetime < ?1
  LIMIT ?2
)
";

const SQL_LIVE_BYTES: &str = "
SELECT coalesce(sum(bytes), 0) FROM refs
WHERE last_accessed + ttl_seconds >= ?1 AND created_at + max_lifetime >= ?1
";

/// Eviction order is last access first, with the key as a tie-break: one
/// compression stores its whole batch in the same second, and without the
/// tie-break which member of that batch dies would vary run to run.
///
/// **Live rows only, and that is load-bearing.** The byte cap is enforced
/// against `SQL_LIVE_BYTES`, which excludes expired rows; an expired row among
/// the candidates had its bytes subtracted from a total that never included
/// them, so the loop stopped early and left the store over its cap. Measured:
/// cap 100, one expired 100-byte row and two live 60-byte rows, eviction
/// computed 20 bytes remaining and stopped with 120 live. Expired rows are not
/// this function's job — `purge_expired` and expiry-on-read reclaim them.
const SQL_EVICTION_CANDIDATES: &str = "
SELECT tenant, ref, bytes FROM refs
WHERE last_accessed + ttl_seconds >= ?2 AND created_at + max_lifetime >= ?2
ORDER BY last_accessed ASC, tenant ASC, ref ASC LIMIT ?1
";

const SQL_STATS: &str = "
SELECT count(*), coalesce(sum(bytes), 0), min(created_at) FROM refs
WHERE last_accessed + ttl_seconds >= ?1 AND created_at + max_lifetime >= ?1
";

/// The same, for one tenant. See [`RefStore::stats_for`].
const SQL_STATS_FOR_TENANT: &str = "
SELECT count(*), coalesce(sum(bytes), 0), min(created_at) FROM refs
WHERE tenant = ?2 AND last_accessed + ttl_seconds >= ?1 AND created_at + max_lifetime >= ?1
";

/// The row the browser reads: [`SQL_GET`] plus the handle, with the fragment
/// cut to a preview so a page never pulls whole fragments. `{head}` and
/// `{start}` are the preview and the characters before it.
const SQL_BROWSE_COLUMNS: &str = "ref, {head}, {start}, bytes, kind, request_id, \
     created_at, last_accessed, ttl_seconds, max_lifetime";

const SQL_INSPECT: &str = "
SELECT fragment, kind, request_id, created_at, last_accessed, ttl_seconds, max_lifetime
FROM refs WHERE tenant = ?1 AND ref = ?2
";

/// Knobs that are not the file path.
#[derive(Clone, Debug)]
pub struct SqliteOptions {
    /// Where TTL decisions read time from.
    pub clock: Clock,
    /// Evict least-recently-accessed rows once live fragments exceed this.
    pub max_bytes: Option<u64>,
}

impl Default for SqliteOptions {
    fn default() -> SqliteOptions {
        SqliteOptions {
            clock: Clock::system(),
            max_bytes: Some(DEFAULT_MAX_BYTES),
        }
    }
}

/// Fragments in a SQLite file.
///
/// `rusqlite::Connection` is `!Sync`, so it lives behind a mutex and every
/// operation is serialized within the process. WAL still earns its place:
/// other processes and other connections to the same file — an operator's
/// `sqlite3`, a second instance during a rolling restart — read without
/// blocking this one's writes.
#[derive(Debug)]
pub struct SqliteRefStore {
    conn: Mutex<Connection>,
    clock: Clock,
    max_bytes: Option<u64>,
}

impl SqliteRefStore {
    /// Open (creating if absent) `refs.db` at `path` with default options.
    pub fn open(path: impl AsRef<Path>) -> Result<SqliteRefStore, StoreError> {
        SqliteRefStore::open_with(path, SqliteOptions::default())
    }

    /// Open `path` with explicit options.
    pub fn open_with(
        path: impl AsRef<Path>,
        options: SqliteOptions,
    ) -> Result<SqliteRefStore, StoreError> {
        let conn = Connection::open(path)?;
        SqliteRefStore::from_connection(conn, options)
    }

    /// A private in-memory database.
    ///
    /// WAL does not apply and nothing else can attach to it, so this is a
    /// faster [`crate::MemoryRefStore`] with SQL semantics — useful for
    /// exercising the statements, not for the sharing guarantees.
    /// Every fragment this store holds, oldest first.
    ///
    /// For building a corpus out of what a real run actually elided. The
    /// [`RefStore`] trait deliberately has no such method — serving a handle is
    /// a point lookup and nothing in the request path should be able to walk
    /// the table — so it lives here, on the concrete store, where only tooling
    /// can reach it.
    ///
    /// Expiry is ignored on purpose: a dump is read long after the run that
    /// wrote it, so every row would be stale by the rules the proxy applies.
    pub fn scan(&self, tenant: &Tenant) -> Result<Vec<RefRecord>, StoreError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT ref, fragment, kind, request_id, created_at, last_accessed, ttl_seconds \
             FROM refs WHERE tenant = ?1 ORDER BY created_at, ref",
        )?;
        let rows = stmt.query_map(params![tenant.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, fragment, kind, request_id, created, accessed, ttl) = row?;
            // Stored as a BLOB. A fragment that is not UTF-8 was never text
            // the engine elided, so it is skipped rather than lossily repaired.
            let Ok(fragment) = String::from_utf8(fragment) else {
                continue;
            };
            // A row whose handle does not parse was not written by this
            // engine. Skipped rather than failed: one unreadable row in a
            // dump is not a reason to refuse the other thousand.
            let Some(id) = RefId::parse(&id) else {
                continue;
            };
            out.push(RefRecord {
                id,
                fragment,
                kind,
                request_id,
                created_at: created.max(0) as u64,
                last_accessed: accessed.max(0) as u64,
                ttl_seconds: ttl.max(0) as u64,
            });
        }
        Ok(out)
    }

    pub fn open_in_memory(options: SqliteOptions) -> Result<SqliteRefStore, StoreError> {
        let conn = Connection::open_in_memory()?;
        SqliteRefStore::from_connection(conn, options)
    }

    fn from_connection(
        conn: Connection,
        options: SqliteOptions,
    ) -> Result<SqliteRefStore, StoreError> {
        configure(&conn)?;
        migrate(&conn)?;
        Ok(SqliteRefStore {
            conn: Mutex::new(conn),
            clock: options.clock,
            max_bytes: options.max_bytes,
        })
    }

    /// Evict by last access until live bytes sit under 90% of the cap.
    fn enforce_byte_cap(&self, conn: &mut Connection, now: u64) -> Result<(), StoreError> {
        let Some(cap) = self.max_bytes else {
            return Ok(());
        };
        let now = clamp_i64(now);
        let live: i64 = conn.query_row(SQL_LIVE_BYTES, params![now], |row| row.get(0))?;
        let mut remaining = live.max(0) as u64;
        if remaining <= cap {
            return Ok(());
        }

        // 90% rather than the cap itself: a store parked exactly at the limit
        // would otherwise evict on every single write for the rest of its life.
        let target = cap / 10 * 9;
        let before = remaining;
        while remaining > target {
            let tx = conn.transaction()?;
            // Candidates are collected before anything is deleted: stepping a
            // SELECT while DELETEing from the same table leaves which rows the
            // cursor still sees up to SQLite.
            let candidates: Vec<(String, String, i64)> = {
                let mut select = tx.prepare_cached(SQL_EVICTION_CANDIDATES)?;
                let rows = select.query_map(params![BATCH_ROWS as i64, now], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
                rows.collect::<Result<_, _>>()?
            };
            if candidates.is_empty() {
                tx.commit()?;
                break;
            }
            {
                let mut delete = tx.prepare_cached(SQL_DELETE_ONE)?;
                for (tenant, id, bytes) in candidates {
                    if remaining <= target {
                        break;
                    }
                    delete.execute(params![tenant, id])?;
                    remaining = remaining.saturating_sub(bytes.max(0) as u64);
                }
            }
            tx.commit()?;
        }
        tracing::debug!(
            live_bytes_before = before,
            live_bytes_after = remaining,
            "evicted least-recently-accessed fragments to fit the byte cap"
        );
        Ok(())
    }
}

impl RefStore for SqliteRefStore {
    fn put_many(
        &self,
        tenant: &Tenant,
        refs: &[(RefId, String)],
        meta: &PutMeta,
    ) -> Result<(), StoreError> {
        self.put_many_created(tenant, refs, meta).map(|_| ())
    }

    fn put_many_created(
        &self,
        tenant: &Tenant,
        refs: &[(RefId, String)],
        meta: &PutMeta,
    ) -> Result<Option<Vec<u64>>, StoreError> {
        if refs.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let now = self.clock.now();
        let mut conn = self.conn.lock();
        let mut created = Vec::with_capacity(refs.len());
        {
            // One transaction for the batch: a request's elisions become
            // retrievable together, and fsync cost is paid once rather than per
            // fragment.
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare_cached(SQL_UPSERT)?;
                for (id, fragment) in refs {
                    // `RETURNING` reports the row as it stands after the
                    // upsert, so a re-store answers with its first creation.
                    let at: i64 = stmt.query_row(
                        params![
                            tenant.as_str(),
                            id.body(),
                            fragment.as_bytes(),
                            fragment.len() as i64,
                            meta.kind.as_str(),
                            meta.request_id.as_deref(),
                            clamp_i64(now),
                            clamp_i64(now),
                            clamp_i64(meta.ttl_seconds),
                            clamp_i64(meta.max_lifetime_seconds),
                        ],
                        |row| row.get(0),
                    )?;
                    created.push(at.max(0) as u64);
                }
            }
            tx.commit()?;
        }
        self.enforce_byte_cap(&mut conn, now)?;
        Ok(Some(created))
    }

    fn get(&self, tenant: &Tenant, id: &RefId, touch: bool) -> Result<Lookup, StoreError> {
        let now = self.clock.now();
        let conn = self.conn.lock();
        let row = {
            let mut stmt = conn.prepare_cached(SQL_GET)?;
            stmt.query_row(params![tenant.as_str(), id.body()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })
            .optional()?
        };
        let Some((fragment, kind, request_id, created_at, last_accessed, ttl, max_lifetime)) = row
        else {
            return Ok(Lookup::Miss);
        };

        let fragment = String::from_utf8(fragment).map_err(|_| StoreError::CorruptRow {
            id: id.to_string(),
            // The bytes are not quoted: a row is most likely unreadable because
            // its content is unexpected, which is exactly when putting it in a
            // log is worst.
            detail: "fragment is not valid UTF-8",
        })?;
        let mut record = RefRecord {
            id: *id,
            fragment,
            kind,
            request_id,
            created_at: created_at.max(0) as u64,
            last_accessed: last_accessed.max(0) as u64,
            ttl_seconds: ttl.max(0) as u64,
        };
        let max_lifetime = max_lifetime.max(0) as u64;

        // Liveness is decided in Rust, not in the WHERE clause, because a dead
        // row and an absent one are different answers and the SQL that filters
        // them out cannot tell the caller which it saw.
        if is_expired(&record, max_lifetime, now) {
            let expired_at = expires_at(&record, max_lifetime);
            let mut stmt = conn.prepare_cached(SQL_DELETE_ONE)?;
            stmt.execute(params![tenant.as_str(), id.body()])?;
            return Ok(Lookup::Expired { expired_at });
        }
        if touch {
            let mut stmt = conn.prepare_cached(SQL_TOUCH)?;
            stmt.execute(params![tenant.as_str(), id.body(), clamp_i64(now)])?;
            record.last_accessed = now;
        }
        Ok(Lookup::Hit(record))
    }

    fn touch_many(&self, tenant: &Tenant, ids: &[RefId]) -> Result<usize, StoreError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let now = clamp_i64(self.clock.now());
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let mut touched = 0;
        {
            let mut stmt = tx.prepare_cached(SQL_TOUCH)?;
            for id in ids {
                touched += stmt.execute(params![tenant.as_str(), id.body(), now])?;
            }
        }
        tx.commit()?;
        Ok(touched)
    }

    fn purge_expired(&self, now: u64) -> Result<u64, StoreError> {
        let mut conn = self.conn.lock();
        let mut removed = 0u64;
        loop {
            let deleted = {
                let mut stmt = conn.prepare_cached(SQL_PURGE)?;
                stmt.execute(params![clamp_i64(now), BATCH_ROWS as i64])?
            };
            removed += deleted as u64;
            // A short pass means the sweep caught up; anything else and there
            // may be another batch of dead rows behind it.
            if deleted < BATCH_ROWS {
                break;
            }
        }
        self.enforce_byte_cap(&mut conn, now)?;
        Ok(removed)
    }

    fn stats(&self) -> Result<StoreStats, StoreError> {
        let now = clamp_i64(self.clock.now());
        let conn = self.conn.lock();
        let mut stmt = conn.prepare_cached(SQL_STATS)?;
        let row: (i64, i64, Option<i64>) = stmt.query_row(params![now], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        Ok(into_stats(row))
    }

    fn stats_for(&self, tenant: &Tenant) -> Result<StoreStats, StoreError> {
        let now = clamp_i64(self.clock.now());
        let conn = self.conn.lock();
        let mut stmt = conn.prepare_cached(SQL_STATS_FOR_TENANT)?;
        let row: (i64, i64, Option<i64>) = stmt.query_row(params![now, tenant.0], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        Ok(into_stats(row))
    }

    fn browser(&self) -> Option<&dyn RefBrowse> {
        Some(self)
    }
}

/// A `WHERE` clause and the values it binds, built one filter at a time.
struct Filter {
    clause: String,
    values: Vec<Value>,
}

impl Filter {
    fn new(tenant: &Tenant) -> Filter {
        Filter {
            clause: "tenant = ?1".to_owned(),
            values: vec![Value::Text(tenant.0.clone())],
        }
    }

    /// Append `condition`, which numbers its placeholders from the index it
    /// is handed, and the values they bind in that order.
    fn and(&mut self, condition: impl FnOnce(usize) -> String, values: Vec<Value>) {
        let next = self.values.len() + 1;
        self.clause.push_str(" AND ");
        self.clause.push_str(&condition(next));
        self.values.extend(values);
    }

    fn live(&mut self, now: i64) {
        self.and(
            |n| {
                format!("last_accessed + ttl_seconds >= ?{n} AND created_at + max_lifetime >= ?{n}")
            },
            vec![Value::Integer(now)],
        );
    }
}

impl RefBrowse for SqliteRefStore {
    fn browse(&self, tenant: &Tenant, query: &BrowseQuery) -> Result<BrowsePage, StoreError> {
        let now = clamp_i64(self.clock.now());
        let mut scope = Filter::new(tenant);
        if !query.include_expired {
            scope.live(now);
        }

        let mut filter = Filter {
            clause: scope.clause.clone(),
            values: scope.values.clone(),
        };
        if let Some(kind) = &query.kind {
            filter.and(|n| format!("kind = ?{n}"), vec![Value::Text(kind.clone())]);
        }
        if let Some(request_id) = &query.request_id {
            filter.and(
                |n| format!("request_id = ?{n}"),
                vec![Value::Text(request_id.clone())],
            );
        }
        if let Some(id) = query.exact_handle() {
            filter.and(
                |n| format!("ref = ?{n}"),
                vec![Value::Text(id.body().to_owned())],
            );
        } else if let Some(needle) = query.needle() {
            // `lower()` folds ASCII only, which is what the memory backend's
            // matcher does too. A handle that cannot match passes `NULL`, and
            // `instr` of `NULL` is never positive.
            let handle = query.handle_needle().map_or(Value::Null, Value::Text);
            filter.and(
                |n| {
                    format!(
                        "(instr(lower(CAST(fragment AS TEXT)), ?{n}) > 0 \
                          OR instr(lower(ref), ?{h}) > 0 \
                          OR instr(lower(coalesce(request_id, '')), ?{n}) > 0)",
                        h = n + 1
                    )
                },
                vec![Value::Text(needle), handle],
            );
        }

        // Every order ends on the handle, as eviction does, so a page boundary
        // inside a batch written in one second is stable across requests.
        let order = match query.order {
            BrowseOrder::Newest => "created_at DESC, ref ASC",
            BrowseOrder::Oldest => "created_at ASC, ref ASC",
            BrowseOrder::RecentlyUsed => "last_accessed DESC, ref ASC",
            BrowseOrder::Largest => "bytes DESC, ref ASC",
            BrowseOrder::ExpiringSoon => {
                "min(last_accessed + ttl_seconds, created_at + max_lifetime) ASC, ref ASC"
            }
        };

        let chars = query.preview_limit();
        let conn = self.conn.lock();

        let total: i64 = conn.query_row(
            &format!("SELECT count(*) FROM refs WHERE {}", filter.clause),
            params_from_iter(filter.values.iter()),
            |row| row.get(0),
        )?;

        let kinds = {
            let mut stmt = conn.prepare(&format!(
                "SELECT kind, count(*) FROM refs WHERE {} GROUP BY kind ORDER BY kind",
                scope.clause
            ))?;
            let rows = stmt.query_map(params_from_iter(scope.values.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?.max(0) as u64,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        let mut values = filter.values;
        let mut bind = |value: Value| {
            values.push(value);
            values.len()
        };
        let needle = query.text_needle();
        let (head, start) = match &needle {
            // A window around the first match. Over TEXT, `instr` and `substr`
            // count characters, and `lower()` folds ASCII only, which keeps
            // the folded copy's positions the original's. No match in the
            // text — the hit was the handle or the request — starts at 1.
            Some(needle) => {
                let n = bind(Value::Text(needle.clone()));
                let lead = bind(Value::Integer(crate::browse::lead_in(chars) as i64));
                // One character more than shown, so a cut is detectable.
                let take = bind(Value::Integer(chars as i64 + 1));
                let at = format!("instr(lower(CAST(fragment AS TEXT)), ?{n})");
                (
                    format!(
                        "CAST(substr(CAST(fragment AS TEXT), max(1, {at} - ?{lead}), ?{take}) AS BLOB)"
                    ),
                    format!("max(0, {at} - 1 - ?{lead})"),
                )
            }
            // Four bytes a character is the most UTF-8 can spend, so this
            // byte prefix always holds `chars` characters when there are any.
            None => {
                let take = bind(Value::Integer(chars.saturating_mul(4) as i64));
                (format!("substr(fragment, 1, ?{take})"), "0".to_owned())
            }
        };
        let limit = bind(Value::Integer(query.page_limit() as i64));
        let offset = bind(Value::Integer(query.offset.min(i64::MAX as usize) as i64));
        let sql = format!(
            "SELECT {} FROM refs WHERE {} ORDER BY {order} LIMIT ?{limit} OFFSET ?{offset}",
            SQL_BROWSE_COLUMNS
                .replace("{head}", &head)
                .replace("{start}", &start),
            filter.clause,
        );

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(values.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
            ))
        })?;
        let mut entries = Vec::new();
        for row in rows {
            let (handle, head, start, bytes, kind, request_id, created, accessed, ttl, lifetime) =
                row?;
            // A row whose handle does not parse was not written by this engine;
            // listing it would offer a handle nothing can open.
            let Some(id) = RefId::parse(&handle) else {
                continue;
            };
            let bytes = bytes.max(0) as u64;
            let head = utf8_prefix(&head);
            let (preview, cut) = crate::browse::preview(&head, chars);
            let detail = RefDetail::new(
                RefRecord {
                    id,
                    fragment: String::new(),
                    kind,
                    request_id,
                    created_at: created.max(0) as u64,
                    last_accessed: accessed.max(0) as u64,
                    ttl_seconds: ttl.max(0) as u64,
                },
                lifetime.max(0) as u64,
                now.max(0) as u64,
            );
            entries.push(RefSummary {
                id,
                kind: detail.record.kind,
                request_id: detail.record.request_id,
                bytes,
                created_at: detail.record.created_at,
                last_accessed: detail.record.last_accessed,
                ttl_seconds: detail.record.ttl_seconds,
                max_lifetime_seconds: detail.max_lifetime_seconds,
                expires_at: detail.expires_at,
                expired: detail.expired,
                // A match window read one character past what it shows; a
                // prefix read a byte count, which the fragment may outrun.
                truncated: cut || (needle.is_none() && (head.len() as u64) < bytes),
                preview_start: start.max(0) as u64,
                preview,
            });
        }

        Ok(BrowsePage {
            total: total.max(0) as u64,
            entries,
            kinds,
        })
    }

    fn inspect(&self, tenant: &Tenant, id: &RefId) -> Result<Option<RefDetail>, StoreError> {
        let now = self.clock.now();
        let conn = self.conn.lock();
        let mut stmt = conn.prepare_cached(SQL_INSPECT)?;
        let row = stmt
            .query_row(params![tenant.as_str(), id.body()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })
            .optional()?;
        let Some((fragment, kind, request_id, created, accessed, ttl, lifetime)) = row else {
            return Ok(None);
        };
        // Lossy rather than `CorruptRow`: the operator looking at a damaged
        // row is the one person who should still be able to see it.
        let fragment = match String::from_utf8(fragment) {
            Ok(text) => text,
            Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
        };
        Ok(Some(RefDetail::new(
            RefRecord {
                id: *id,
                fragment,
                kind,
                request_id,
                created_at: created.max(0) as u64,
                last_accessed: accessed.max(0) as u64,
                ttl_seconds: ttl.max(0) as u64,
            },
            lifetime.max(0) as u64,
            now,
        )))
    }

    fn remove(&self, tenant: &Tenant, ids: &[RefId]) -> Result<usize, StoreError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let mut removed = 0;
        {
            let mut stmt = tx.prepare_cached(SQL_DELETE_ONE)?;
            for id in ids {
                removed += stmt.execute(params![tenant.as_str(), id.body()])?;
            }
        }
        tx.commit()?;
        Ok(removed)
    }
}

/// The longest valid UTF-8 prefix of `bytes`. A byte-counted `substr` can end
/// inside a character; the partial character is dropped, never repaired.
fn utf8_prefix(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(error) if error.valid_up_to() > 0 || error.error_len().is_none() => {
            String::from_utf8_lossy(&bytes[..error.valid_up_to()]).into_owned()
        }
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn into_stats((refs, bytes, oldest): (i64, i64, Option<i64>)) -> StoreStats {
    StoreStats {
        live_refs: refs.max(0) as u64,
        live_bytes: bytes.max(0) as u64,
        oldest_created_at: oldest.map(|v| v.max(0) as u64),
    }
}

/// Connection pragmas, applied before the schema.
fn configure(conn: &Connection) -> Result<(), StoreError> {
    // `PRAGMA journal_mode` answers with the mode it settled on, so it is a
    // query: an in-memory database keeps "memory" and that is fine.
    conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
    conn.execute_batch(
        "PRAGMA synchronous=NORMAL;
         PRAGMA busy_timeout=5000;
         PRAGMA temp_store=MEMORY;",
    )?;
    Ok(())
}

/// Bring the schema up to the last entry of [`MIGRATIONS`].
fn migrate(conn: &Connection) -> Result<(), StoreError> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let mut current = current.max(0) as u32;
    for (version, sql) in MIGRATIONS {
        if *version <= current {
            continue;
        }
        // Schema change and version bump commit together, so a crash mid-way
        // leaves a database that the next open migrates again rather than one
        // that claims a version it does not have.
        let script = format!("BEGIN;\n{sql}\nPRAGMA user_version = {version};\nCOMMIT;");
        conn.execute_batch(&script)
            .map_err(|source| StoreError::Migration {
                version: *version,
                source,
            })?;
        current = *version;
    }
    Ok(())
}

/// Narrow a `u64` timestamp or duration to SQLite's signed integer.
///
/// A `u64::MAX` TTL means "never expire"; handed to SQLite unclamped it wraps
/// to a negative number and the entry reads as expired on the spot — the exact
/// opposite of what was asked for. Clamping instead pushes the deadline past
/// any clock this will run under.
fn clamp_i64(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use crate::types::PutMeta;
    use kompressor_engine::refs::ref_for;
    use kompressor_engine::types::Kind;
    use tempfile::TempDir;

    fn meta() -> PutMeta {
        PutMeta::new(Kind::Log)
    }

    /// The byte cap counts live bytes, so eviction must spend live bytes.
    ///
    /// The twin of the memory store's test: candidates came from every row,
    /// including expired ones, so deleting an expired row debited a total that
    /// never included it and the loop stopped over the cap.
    #[test]
    fn an_expired_row_does_not_pay_for_a_live_one() {
        let dir = TempDir::new().unwrap();
        let clock = ManualClock::at(100);
        let store = SqliteRefStore::open_with(
            dir.path().join("refs.db"),
            SqliteOptions {
                clock: clock.clock(),
                max_bytes: Some(100),
            },
        )
        .unwrap();
        let tenant = Tenant::new("t");

        let dead = "x".repeat(100);
        store
            .put_many(
                &tenant,
                &[(ref_for(&dead), dead.clone())],
                &meta().with_ttl(1),
            )
            .unwrap();

        clock.set(103);
        for fill in ["a", "b"] {
            let live = fill.repeat(60);
            store
                .put_many(
                    &tenant,
                    &[(ref_for(&live), live.clone())],
                    &meta().with_ttl(10_000),
                )
                .unwrap();
        }

        let live_bytes = store.stats().unwrap().live_bytes;
        assert!(
            live_bytes <= 100,
            "cap is 100 and eviction left {live_bytes} live bytes"
        );
    }

    #[test]
    fn migrations_are_listed_in_strictly_increasing_numeric_order() {
        // The array's order *is* the apply order; an out-of-order entry would
        // reintroduce the lexical-sort bug this list exists to prevent.
        for pair in MIGRATIONS.windows(2) {
            assert!(pair[0].0 < pair[1].0, "migrations out of order");
        }
        assert_eq!(MIGRATIONS[0].0, 1, "versions start at 1");
    }

    #[test]
    fn opening_a_database_records_the_schema_version() {
        let dir = TempDir::new().unwrap();
        let store = SqliteRefStore::open(dir.path().join("refs.db")).unwrap();
        let conn = store.conn.lock();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version as u32, MIGRATIONS.last().unwrap().0);
    }

    #[test]
    fn opening_an_existing_database_twice_does_not_re_run_migrations() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("refs.db");
        let tenant = Tenant::new("acme");
        let id = ref_for("payload");
        {
            let store = SqliteRefStore::open(&path).unwrap();
            store
                .put_many(&tenant, &[(id, "payload".to_owned())], &meta())
                .unwrap();
        }
        let reopened = SqliteRefStore::open(&path).unwrap();
        assert_eq!(
            reopened.get(&tenant, &id, true).unwrap().fragment(),
            Some("payload"),
            "a second open must not drop the table it finds"
        );
    }

    #[test]
    fn a_file_backed_store_keeps_its_entries_across_a_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("refs.db");
        let tenant = Tenant::new("acme");
        let id = ref_for("durable fragment");
        let clock = ManualClock::at(1_000);
        {
            let store = SqliteRefStore::open_with(
                &path,
                SqliteOptions {
                    clock: clock.clock(),
                    ..SqliteOptions::default()
                },
            )
            .unwrap();
            store
                .put_many(&tenant, &[(id, "durable fragment".to_owned())], &meta())
                .unwrap();
        }
        clock.advance(60);
        let store = SqliteRefStore::open_with(
            &path,
            SqliteOptions {
                clock: clock.clock(),
                ..SqliteOptions::default()
            },
        )
        .unwrap();
        let hit = store.get(&tenant, &id, true).unwrap();
        assert_eq!(hit.fragment(), Some("durable fragment"));
        match hit {
            Lookup::Hit(record) => assert_eq!(record.created_at, 1_000, "created_at survives"),
            other => panic!("expected a hit, got {other:?}"),
        }
    }

    #[test]
    fn a_second_connection_to_the_same_file_sees_committed_rows() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("refs.db");
        let writer = SqliteRefStore::open(&path).unwrap();
        let reader = SqliteRefStore::open(&path).unwrap();
        let tenant = Tenant::new("acme");
        let id = ref_for("shared");

        assert_eq!(reader.get(&tenant, &id, false).unwrap(), Lookup::Miss);
        writer
            .put_many(&tenant, &[(id, "shared".to_owned())], &meta())
            .unwrap();
        // WAL is what makes this read cheap as well as correct: it does not
        // wait on the writer's connection.
        assert_eq!(
            reader.get(&tenant, &id, false).unwrap().fragment(),
            Some("shared")
        );
        assert_eq!(reader.stats().unwrap().live_refs, 1);
    }

    #[test]
    fn the_database_is_opened_in_write_ahead_logging_mode() {
        let dir = TempDir::new().unwrap();
        let store = SqliteRefStore::open(dir.path().join("refs.db")).unwrap();
        let conn = store.conn.lock();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn a_fragment_that_is_not_utf8_is_reported_rather_than_returned() {
        let dir = TempDir::new().unwrap();
        let store = SqliteRefStore::open(dir.path().join("refs.db")).unwrap();
        let tenant = Tenant::new("acme");
        let id = ref_for("whatever");
        {
            let conn = store.conn.lock();
            conn.query_row(
                SQL_UPSERT,
                params![
                    tenant.as_str(),
                    id.body(),
                    vec![0xff_u8, 0xfe],
                    2_i64,
                    "log",
                    None::<String>,
                    0_i64,
                    i64::MAX / 2,
                    i64::MAX / 4,
                    i64::MAX / 4,
                ],
                |_| Ok(()),
            )
            .unwrap();
        }
        let err = store.get(&tenant, &id, false).unwrap_err();
        assert!(matches!(err, StoreError::CorruptRow { .. }));
        // and the diagnostic carries no payload bytes
        assert!(!format!("{err}").contains('\u{fffd}'));
    }
}
