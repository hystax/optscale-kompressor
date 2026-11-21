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

use crate::browse::{
    BrowseOrder, BrowsePage, BrowseQuery, MAX_CENSUS_ENTRIES, RefBrowse, RefCensus, RefCensusEntry,
    RefDetail, RefSummary,
};
use crate::clock::Clock;
use crate::types::{
    Lookup, PutMeta, RefRecord, RefStore, StoreError, StoreStats, Tenant, expires_at, is_expired,
};
use kompressor_engine::refs::RefId;
use parking_lot::{Mutex, MutexGuard};
use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, params_from_iter};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

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

// `created_at` survives a re-store so the absolute lifetime cannot be reset
// every turn — but only while the row is alive. A row already past its window
// and not yet swept is the same thing as no row: keeping its `created_at` would
// hand back a handle that is expired the moment it is minted, and whether that
// happened would depend on when the sweep last ran. Every right-hand side reads
// the row as it was before the update, so the test sees the old deadlines.
const SQL_UPSERT: &str = "
INSERT INTO refs (tenant, ref, fragment, bytes, kind, request_id,
                  created_at, last_accessed, ttl_seconds, max_lifetime)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
ON CONFLICT(tenant, ref) DO UPDATE SET
  created_at    = CASE WHEN min(refs.last_accessed + refs.ttl_seconds,
                                refs.created_at + refs.max_lifetime) < excluded.created_at
                       THEN excluded.created_at ELSE refs.created_at END,
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

const SQL_DELETE_ONE: &str = "DELETE FROM refs WHERE tenant = ?1 AND ref = ?2 RETURNING bytes";

/// The stored size of one row, read before an upsert so the byte counter can
/// charge the difference rather than the whole fragment again.
const SQL_BYTES_OF: &str = "SELECT bytes FROM refs WHERE tenant = ?1 AND ref = ?2";

/// Every row's bytes, live or not: what [`SqliteRefStore`] seeds its byte
/// counter from, and reconciles it against on each purge.
const SQL_TOTAL_BYTES: &str = "SELECT coalesce(sum(bytes), 0) FROM refs";

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

/// Total and live bytes in one pass: the cap is enforced against the live
/// figure, and the total re-seeds the byte counter the scan was run to check.
const SQL_CAP_BYTES: &str = "
SELECT coalesce(sum(bytes), 0),
       coalesce(sum(CASE WHEN last_accessed + ttl_seconds >= ?1
                          AND created_at + max_lifetime >= ?1 THEN bytes ELSE 0 END), 0)
FROM refs
";

/// The cheapest statement that still proves the table is readable.
const SQL_PING: &str = "SELECT count(*) FROM (SELECT 1 FROM refs LIMIT 1)";

/// Eviction order is last access first, with the key as a tie-break: one
/// compression stores its whole batch in the same second, and without the
/// tie-break which member of that batch dies would vary run to run.
///
/// **Live rows only, and that is load-bearing.** The byte cap is enforced
/// against the live half of `SQL_CAP_BYTES`, which excludes expired rows; an expired row among
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

/// Bookkeeping only: the fragment column is never read, so a census of a
/// gigabyte store moves a few megabytes.
const SQL_CENSUS: &str = "
SELECT ref, kind, bytes, created_at, last_accessed, ttl_seconds, max_lifetime
FROM refs WHERE tenant = ?1
ORDER BY created_at DESC, ref
LIMIT ?2
";

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
    /// An upper bound on the bytes of every row, expired ones included,
    /// maintained by this process's own writes and only changed while `conn`
    /// is locked.
    ///
    /// It exists so a put does not have to sum the whole table to learn that
    /// the store is nowhere near its cap: live bytes can never exceed stored
    /// bytes, so a bound under the cap settles the question with no scan. The
    /// exact scan still runs once the bound crosses the cap, and re-seeds it.
    /// Another process writing the same file is not seen here; the purge pass
    /// re-reads the true total, which bounds how long that can mislead.
    stored_bytes: AtomicU64,
    /// A second, read-only connection for the ref browser, when the store is
    /// a file.
    ///
    /// A browse search lowercases every fragment it scans and sorts on columns
    /// no index covers; on the main connection that held the lock every put
    /// and retrieval waits on for as long as an operator's search took. WAL
    /// lets this connection read a committed snapshot beside the writer. An
    /// in-memory store cannot be opened twice and browses on `conn`.
    browse_conn: Option<Mutex<Connection>>,
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
        let path = path.as_ref();
        let conn = Connection::open(path)?;
        let mut store = SqliteRefStore::from_connection(conn, options)?;
        // Opened after the main connection has created and migrated the file.
        // A failure here costs only the isolation, never the store.
        store.browse_conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .and_then(|reader| {
            reader.execute_batch("PRAGMA busy_timeout=5000; PRAGMA temp_store=MEMORY;")?;
            Ok(reader)
        })
        .map_err(|error| {
            tracing::debug!(%error, "ref browser shares the store connection");
        })
        .ok()
        .map(Mutex::new);
        Ok(store)
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
        let stored: i64 = conn.query_row(SQL_TOTAL_BYTES, [], |row| row.get(0))?;
        Ok(SqliteRefStore {
            conn: Mutex::new(conn),
            clock: options.clock,
            max_bytes: options.max_bytes,
            stored_bytes: AtomicU64::new(stored.max(0) as u64),
            browse_conn: None,
        })
    }

    /// The connection browse reads go through: the read-only one when there is
    /// one, otherwise the store's own.
    fn reader(&self) -> MutexGuard<'_, Connection> {
        self.browse_conn.as_ref().unwrap_or(&self.conn).lock()
    }

    /// Take `bytes` off the stored-bytes bound, never below zero.
    fn release(&self, bytes: i64) {
        // Every caller holds the connection lock, which is what serialises
        // writes to the bound, so a load and a store cannot interleave.
        let held = self.stored_bytes.load(Ordering::Relaxed);
        self.stored_bytes
            .store(held.saturating_sub(bytes.max(0) as u64), Ordering::Relaxed);
    }

    /// Evict by last access until live bytes sit under 90% of the cap.
    fn enforce_byte_cap(&self, conn: &mut Connection, now: u64) -> Result<(), StoreError> {
        let Some(cap) = self.max_bytes else {
            return Ok(());
        };
        // Live bytes are a subset of stored bytes, so a bound under the cap
        // answers without reading the table.
        if self.stored_bytes.load(Ordering::Relaxed) <= cap {
            return Ok(());
        }
        let now = clamp_i64(now);
        let (total, live): (i64, i64) = conn.query_row(SQL_CAP_BYTES, params![now], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        self.stored_bytes
            .store(total.max(0) as u64, Ordering::Relaxed);
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
                    delete
                        .query_row(params![tenant, id], |row| row.get::<_, i64>(0))
                        .optional()?;
                    remaining = remaining.saturating_sub(bytes.max(0) as u64);
                    self.release(bytes);
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
        let mut added: i64 = 0;
        {
            // One transaction for the batch: a request's elisions become
            // retrievable together, and fsync cost is paid once rather than per
            // fragment.
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare_cached(SQL_UPSERT)?;
                let mut sized = tx.prepare_cached(SQL_BYTES_OF)?;
                for (id, fragment) in refs {
                    // A point read on the primary key: a re-store, which is
                    // most of them on a long conversation, charges only the
                    // difference (normally none) instead of the whole fragment.
                    let previous: i64 = sized
                        .query_row(params![tenant.as_str(), id.body()], |row| row.get(0))
                        .optional()?
                        .unwrap_or(0);
                    added += fragment.len() as i64 - previous.max(0);
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
        // Charged only after the commit: a batch that rolled back stored nothing.
        if added >= 0 {
            self.stored_bytes.fetch_add(added as u64, Ordering::Relaxed);
        } else {
            self.release(-added);
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
            if let Some(bytes) = stmt
                .query_row(params![tenant.as_str(), id.body()], |row| {
                    row.get::<_, i64>(0)
                })
                .optional()?
            {
                self.release(bytes);
            }
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
        // The periodic sweep is where the bound is made exact again: it is off
        // the request path, and it also picks up rows another process wrote or
        // deleted.
        let stored: i64 = conn.query_row(SQL_TOTAL_BYTES, [], |row| row.get(0))?;
        self.stored_bytes
            .store(stored.max(0) as u64, Ordering::Relaxed);
        self.enforce_byte_cap(&mut conn, now)?;
        Ok(removed)
    }

    fn reclaim(&self) -> Result<u64, StoreError> {
        let conn = self.conn.lock();
        let number = |pragma: &str| -> Result<i64, StoreError> {
            Ok(conn.query_row(&format!("PRAGMA {pragma}"), [], |row| row.get(0))?)
        };
        // Only a file in incremental mode can give pages back piecemeal; one
        // that is not yet waits for a vacuum to convert it.
        if number("auto_vacuum")? != 2 {
            return Ok(0);
        }
        let before = number("freelist_count")?;
        if before == 0 {
            return Ok(0);
        }
        // It frees one page per step: stepped to the end, not run once.
        let mut statement = conn.prepare(&format!("PRAGMA incremental_vacuum({RECLAIM_PAGES})"))?;
        let mut rows = statement.query([])?;
        while rows.next()?.is_some() {}
        drop(rows);
        drop(statement);
        let after = number("freelist_count")?;
        Ok(((before - after).max(0) * number("page_size")?.max(0)) as u64)
    }

    fn ping(&self) -> Result<(), StoreError> {
        let conn = self.conn.lock();
        conn.prepare_cached(SQL_PING)?
            .query_row([], |row| row.get::<_, i64>(0))?;
        Ok(())
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
    fn census(&self, tenant: &Tenant) -> Result<RefCensus, StoreError> {
        let now = self.clock.now();
        let conn = self.reader();
        let mut stmt = conn.prepare_cached(SQL_CENSUS)?;
        let mut entries = stmt
            .query_map(
                params![tenant.as_str(), (MAX_CENSUS_ENTRIES + 1) as i64],
                |row| {
                    let created = row.get::<_, i64>(3)?.max(0) as u64;
                    let accessed = row.get::<_, i64>(4)?.max(0) as u64;
                    let ttl = row.get::<_, i64>(5)?.max(0) as u64;
                    let lifetime = row.get::<_, i64>(6)?.max(0) as u64;
                    let expires_at = accessed
                        .saturating_add(ttl)
                        .min(created.saturating_add(lifetime));
                    Ok(RefCensusEntry {
                        id: row.get(0)?,
                        kind: row.get(1)?,
                        bytes: row.get::<_, i64>(2)?.max(0) as u64,
                        created_at: created,
                        last_accessed: accessed,
                        expires_at,
                        expired: expires_at < now,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let truncated = entries.len() > MAX_CENSUS_ENTRIES;
        entries.truncate(MAX_CENSUS_ENTRIES);
        Ok(RefCensus { entries, truncated })
    }

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
        let narrowed = filter.clause != scope.clause;
        let conn = self.reader();

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
        // Unfiltered, the facets already partition exactly the rows a count
        // would read again; only a narrowed listing needs its own count.
        let total: i64 = if narrowed {
            conn.query_row(
                &format!("SELECT count(*) FROM refs WHERE {}", filter.clause),
                params_from_iter(filter.values.iter()),
                |row| row.get(0),
            )?
        } else {
            kinds.iter().map(|(_, n)| *n as i64).sum()
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
        let conn = self.reader();
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
        let mut freed: i64 = 0;
        {
            let mut stmt = tx.prepare_cached(SQL_DELETE_ONE)?;
            for id in ids {
                if let Some(bytes) = stmt
                    .query_row(params![tenant.as_str(), id.body()], |row| {
                        row.get::<_, i64>(0)
                    })
                    .optional()?
                {
                    removed += 1;
                    freed += bytes.max(0);
                }
            }
        }
        tx.commit()?;
        self.release(freed);
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

/// Write a consistent copy of the store at `source` to `destination`, which
/// must not exist, and return the schema version the copy holds.
///
/// `VACUUM INTO` on a read-only connection, for the reasons
/// `kompressor_stats::schema::snapshot` gives: one snapshot of the file
/// whatever is being written meanwhile, the WAL folded in, and a result that
/// is a single self-contained file. Checked (`PRAGMA quick_check`) before it
/// is returned.
///
/// The copy holds every tenant's fragments, expired ones included: it is the
/// file, not a view of it.
pub fn snapshot(source: &Path, destination: &Path) -> Result<u32, StoreError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(source, flags)?;
    conn.execute_batch("PRAGMA busy_timeout=5000;")?;
    conn.execute("VACUUM INTO ?1", [destination.to_string_lossy().as_ref()])?;
    drop(conn);
    let copy = Connection::open_with_flags(destination, flags)?;
    let verdict: String = copy.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if verdict != "ok" {
        return Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some(format!("the copy failed its check: {verdict}")),
        )));
    }
    let version: i64 = copy.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    Ok(u32::try_from(version).unwrap_or(0))
}

/// The schema version a store this build opens is brought to.
pub fn latest_version() -> u32 {
    MIGRATIONS.last().map_or(0, |(version, _)| *version)
}

/// What SQLite says about a database file. Any SQLite file: the statistics
/// database is described through this too, by whoever holds its path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DatabaseFacts {
    /// `PRAGMA user_version`: the schema the file holds.
    pub schema_version: u32,
    pub page_size: u64,
    pub page_count: u64,
    /// Pages in the file that hold nothing: what a `VACUUM` would give back.
    pub free_pages: u64,
    /// Whether the file is in write-ahead-log mode.
    pub wal: bool,
    /// Whether it holds planner statistics: [`optimize`] has run on it.
    pub analyzed: bool,
    /// `PRAGMA auto_vacuum`: 0 none, 1 full, 2 incremental — whether freed
    /// pages can go back to the disk without a full vacuum.
    pub auto_vacuum: u8,
}

impl DatabaseFacts {
    /// Bytes a `VACUUM` would return to the filesystem.
    pub fn reclaimable_bytes(&self) -> u64 {
        self.free_pages * self.page_size
    }
}

fn read_only(path: &Path) -> Result<Connection, StoreError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.execute_batch("PRAGMA busy_timeout=5000;")?;
    Ok(conn)
}

/// Read [`DatabaseFacts`] from the file at `path`, as one more reader: four
/// header fields, no table is scanned.
pub fn facts(path: &Path) -> Result<DatabaseFacts, StoreError> {
    let conn = read_only(path)?;
    let number = |pragma: &str| -> Result<u64, StoreError> {
        let value: i64 = conn.query_row(&format!("PRAGMA {pragma}"), [], |row| row.get(0))?;
        Ok(value.max(0) as u64)
    };
    let mode: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    Ok(DatabaseFacts {
        schema_version: number("user_version")?.min(u64::from(u32::MAX)) as u32,
        page_size: number("page_size")?,
        page_count: number("page_count")?,
        free_pages: number("freelist_count")?,
        wal: mode.eq_ignore_ascii_case("wal"),
        analyzed: number("table_info(sqlite_stat1)").is_ok(),
        auto_vacuum: number("auto_vacuum")?.min(2) as u8,
    })
}

/// Run `PRAGMA integrity_check` on the file at `path` and return what it
/// found: empty when the database is sound, otherwise up to a hundred
/// problems in SQLite's own words.
///
/// The full check, not `quick_check`: every page, every index against its
/// table. It reads the whole file — seconds for hundreds of megabytes — on a
/// read-only connection, so writers carry on.
pub fn integrity_check(path: &Path) -> Result<Vec<String>, StoreError> {
    let conn = read_only(path)?;
    let mut statement = conn.prepare("PRAGMA integrity_check")?;
    let lines = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(lines.into_iter().filter(|line| line != "ok").collect())
}

/// Whether another connection — another process, in practice the running
/// service — has the database at `path` open.
///
/// SQLite will not take a database out of write-ahead-log mode while anyone
/// else has it open, however idle, and says so at once; nothing else it
/// offers sees an idle connection. So this asks for exactly that and, when it
/// is granted, puts the mode back: the file is left as it was found.
///
/// `false` for a file that is missing, is not in WAL mode, or cannot be read
/// as a database at all — none of which a running service is quietly using.
pub fn in_use(path: &Path) -> bool {
    let Ok(conn) = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return false;
    };
    let mode = |sql: &str| conn.query_row(sql, [], |row| row.get::<_, String>(0));
    match mode("PRAGMA journal_mode") {
        Ok(current) if current.eq_ignore_ascii_case("wal") => {}
        _ => return false,
    }
    match mode("PRAGMA journal_mode=DELETE") {
        Ok(now) if now.eq_ignore_ascii_case("delete") => {
            let _ = mode("PRAGMA journal_mode=WAL");
            false
        }
        // Still "wal": refused, without an error, because someone has it.
        Ok(_) => true,
        Err(rusqlite::Error::SqliteFailure(error, _)) => matches!(
            error.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        ),
        Err(_) => false,
    }
}

/// What a checkpoint did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checkpointed {
    /// False when a reader or writer was in the way and part of the log is
    /// still to be folded in. Nothing is lost; the next one finishes it.
    pub complete: bool,
    /// Frames the log held, and how many of them reached the database.
    pub log_frames: u64,
    pub moved_frames: u64,
}

/// Fold the write-ahead log of the database at `path` into the file and
/// truncate the log (`PRAGMA wal_checkpoint(TRUNCATE)`).
///
/// SQLite does this itself as the log grows; doing it by hand matters before
/// copying a file by other means, and after a burst left a large log behind.
/// It waits up to two seconds for readers to move on and then says it could
/// not finish rather than holding writers any longer.
pub fn checkpoint(path: &Path) -> Result<Checkpointed, StoreError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.execute_batch("PRAGMA busy_timeout=2000;")?;
    let (busy, log, moved): (i64, i64, i64) =
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    Ok(Checkpointed {
        complete: busy == 0,
        // -1 for a database that is not in WAL mode: nothing to do.
        log_frames: log.max(0) as u64,
        moved_frames: moved.max(0) as u64,
    })
}

fn read_write(path: &Path) -> Result<Connection, StoreError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.execute_batch("PRAGMA busy_timeout=5000;")?;
    Ok(conn)
}

/// The size on disk of the database at `path`: the file and its write-ahead
/// log. Zero for one that is not there.
pub fn on_disk(path: &Path) -> u64 {
    let len = |path: &Path| std::fs::metadata(path).map_or(0, |meta| meta.len());
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    len(path) + len(Path::new(&wal))
}

/// What a vacuum did to the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vacuumed {
    /// The database's size on disk before and after, write-ahead log
    /// included: rows not yet folded into the file are part of it.
    pub bytes_before: u64,
    pub bytes_after: u64,
    /// False when a reader kept the rebuilt pages in the write-ahead log:
    /// the file shrinks at the next checkpoint instead of now.
    pub folded: bool,
}

/// Rebuild the database at `path` without its free pages (`VACUUM`), then
/// fold the write-ahead log in so the file itself shrinks.
///
/// A vacuum writes the whole database again: it needs about the file's size
/// free on its disk, twice that while the rebuilt pages wait in the log, and
/// it is the one writer for as long as it takes. Readers carry on. Whoever
/// else writes to the file waits out its busy timeout and then fails, so a
/// service that owns the file holds its own writes for the duration rather
/// than race this (`StatsSink::exclusive` in the statistics crate).
pub fn vacuum(path: &Path) -> Result<Vacuumed, StoreError> {
    vacuum_with(path, None)
}

/// [`vacuum`], converting the file to `auto_vacuum = INCREMENTAL` on the way:
/// the one moment that can be done on a file that already has tables. For
/// the ref store, whose sweep then hands pages back by itself.
pub fn vacuum_incremental(path: &Path) -> Result<Vacuumed, StoreError> {
    vacuum_with(path, Some("INCREMENTAL"))
}

fn vacuum_with(path: &Path, auto_vacuum: Option<&str>) -> Result<Vacuumed, StoreError> {
    let size = || on_disk(path);
    let bytes_before = size();
    let conn = read_write(path)?;
    if let Some(mode) = auto_vacuum {
        conn.execute_batch(&format!("PRAGMA auto_vacuum={mode};"))?;
    }
    conn.execute_batch("VACUUM;")?;
    drop(conn);
    let folded = checkpoint(path)?.complete;
    Ok(Vacuumed {
        bytes_before,
        bytes_after: size(),
        folded,
    })
}

/// Gather the statistics the query planner chooses indexes by (`ANALYZE`),
/// and return how many tables and indexes it now has figures for.
///
/// Bounded (`analysis_limit`): it samples each index rather than reading all
/// of it, so it is quick on a large file. The figures are kept in the file
/// and go stale as the data's shape changes; running it again refreshes them.
pub fn optimize(path: &Path) -> Result<u64, StoreError> {
    let conn = read_write(path)?;
    conn.execute_batch("PRAGMA analysis_limit=1000; ANALYZE;")?;
    let described: i64 =
        conn.query_row("SELECT count(*) FROM sqlite_stat1", [], |row| row.get(0))?;
    Ok(described.max(0) as u64)
}

/// The space one table or index takes in the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Space {
    pub name: String,
    /// `table` or `index`.
    pub kind: String,
    /// The table an index belongs to; a table's own name.
    pub table: String,
    /// Bytes of the file its pages take, and how many of those hold nothing.
    pub bytes: u64,
    pub unused_bytes: u64,
    /// Rows of a table, entries of an index.
    pub rows: u64,
}

/// Where the bytes of the database at `path` are: every table and index,
/// largest first.
///
/// Reads every page (`dbstat`), so unlike [`facts`] it takes as long as the
/// file is large.
pub fn space(path: &Path) -> Result<Vec<Space>, StoreError> {
    let conn = read_only(path)?;
    let mut statement = conn.prepare(
        "SELECT s.name, coalesce(m.type, 'table'), coalesce(m.tbl_name, s.name),
                sum(s.pgsize), sum(s.unused),
                sum(CASE WHEN s.pagetype = 'leaf' THEN s.ncell ELSE 0 END)
         FROM dbstat s LEFT JOIN sqlite_schema m ON m.name = s.name
         GROUP BY s.name ORDER BY 4 DESC, 1",
    )?;
    let number = |value: i64| value.max(0) as u64;
    let rows = statement
        .query_map([], |row| {
            Ok(Space {
                name: row.get(0)?,
                kind: row.get(1)?,
                table: row.get(2)?,
                bytes: number(row.get(3)?),
                unused_bytes: number(row.get(4)?),
                rows: number(row.get(5)?),
            })
        })?
        .collect::<Result<Vec<Space>, _>>()?;
    Ok(rows)
}

/// Connection pragmas, applied before the schema.
fn configure(conn: &Connection) -> Result<(), StoreError> {
    // The timeout first: what follows takes the file's lock, and a process
    // opening the same file at the same moment has to wait rather than fail.
    conn.execute_batch("PRAGMA busy_timeout=5000;")?;
    // Before anything creates a table: on a new file this is what lets the
    // sweep hand freed pages back to the disk a few at a time
    // ([`RefStore::reclaim`]) instead of keeping them until a full vacuum.
    // Only on a file with no tables yet: on one that has them it would
    // change nothing until a vacuum (which converts it) and still write the
    // header, so merely opening a store would modify the file.
    let tables: i64 = conn.query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))?;
    if tables == 0 {
        conn.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")?;
    }
    // `PRAGMA journal_mode` answers with the mode it settled on, so it is a
    // query: an in-memory database keeps "memory" and that is fine.
    //
    // Retried, because the busy timeout does not cover it. Switching a new
    // file to WAL needs it exclusively, and when two processes create the
    // file together each holds a read lock the other waits on; SQLite refuses
    // one at once instead of calling the busy handler. A `serve` and an
    // `mcp serve` starting together on a fresh install lost one of the two
    // to "database is locked". Asking again resolves it, and on a file
    // already in WAL the statement changes nothing.
    let started = std::time::Instant::now();
    loop {
        match conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(())) {
            Ok(()) => break,
            Err(rusqlite::Error::SqliteFailure(error, _))
                if matches!(
                    error.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) && started.elapsed() < std::time::Duration::from_secs(5) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
    conn.execute_batch(&format!(
        "PRAGMA synchronous=NORMAL;
         PRAGMA temp_store=MEMORY;
         PRAGMA journal_size_limit={JOURNAL_SIZE_LIMIT};"
    ))?;
    Ok(())
}

/// The size a write-ahead log is cut back to after a checkpoint
/// (`PRAGMA journal_size_limit`). SQLite reuses the log rather than shrinking
/// it, so without this one burst — a vacuum, a sweep of a full store — leaves
/// a log that size on disk for good.
pub const JOURNAL_SIZE_LIMIT: u64 = 16 * 1024 * 1024;

/// Free pages one [`RefStore::reclaim`] hands back: 8 MiB at the default page
/// size, so it holds the store's lock for milliseconds.
const RECLAIM_PAGES: i64 = 2_048;

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
        // `IMMEDIATE`, so a second process opening the file waits here for
        // the first instead of starting the same step beside it.
        let script = format!("BEGIN IMMEDIATE;\n{sql}\nPRAGMA user_version = {version};\nCOMMIT;");
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

    fn stored(store: &SqliteRefStore) -> u64 {
        store.stored_bytes.load(Ordering::Relaxed)
    }

    /// The bound charges a re-store the difference, not the whole fragment
    /// again: on a long conversation every turn re-stores the same handles,
    /// and a bound that grew with each turn would make the scan it exists to
    /// skip run on every put.
    #[test]
    fn re_storing_a_fragment_does_not_grow_the_byte_bound() {
        let dir = TempDir::new().unwrap();
        let store = SqliteRefStore::open(dir.path().join("refs.db")).unwrap();
        let tenant = Tenant::new("t");
        let body = "x".repeat(100);
        for _ in 0..10 {
            store
                .put_many(&tenant, &[(ref_for(&body), body.clone())], &meta())
                .unwrap();
        }
        assert_eq!(stored(&store), 100);

        store.remove(&tenant, &[ref_for(&body)]).unwrap();
        assert_eq!(stored(&store), 0);
    }

    /// A reopened store seeds its bound from the file, so rows written by an
    /// earlier run still count against the cap without a scan per put.
    #[test]
    fn a_reopened_store_still_enforces_the_cap_on_rows_it_did_not_write() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("refs.db");
        let tenant = Tenant::new("t");
        {
            let store = SqliteRefStore::open_with(
                &path,
                SqliteOptions {
                    clock: Clock::system(),
                    max_bytes: None,
                },
            )
            .unwrap();
            for fill in ["a", "b", "c"] {
                let body = fill.repeat(100);
                store
                    .put_many(&tenant, &[(ref_for(&body), body.clone())], &meta())
                    .unwrap();
            }
        }
        let store = SqliteRefStore::open_with(
            &path,
            SqliteOptions {
                clock: Clock::system(),
                max_bytes: Some(200),
            },
        )
        .unwrap();
        assert_eq!(stored(&store), 300);
        let body = "d".repeat(10);
        store
            .put_many(&tenant, &[(ref_for(&body), body.clone())], &meta())
            .unwrap();
        let live = store.stats().unwrap().live_bytes;
        assert!(live <= 180, "cap 200 evicts to 90%, left {live}");
        assert_eq!(stored(&store), live, "the scan re-seeds the bound");
    }

    /// The purge pass makes the bound exact again, whatever drifted.
    #[test]
    fn a_purge_re_reads_the_true_total() {
        let dir = TempDir::new().unwrap();
        let clock = ManualClock::at(100);
        let store = SqliteRefStore::open_with(
            dir.path().join("refs.db"),
            SqliteOptions {
                clock: clock.clock(),
                max_bytes: None,
            },
        )
        .unwrap();
        let tenant = Tenant::new("t");
        let body = "x".repeat(50);
        store
            .put_many(
                &tenant,
                &[(ref_for(&body), body.clone())],
                &meta().with_ttl(1),
            )
            .unwrap();
        store.stored_bytes.store(10_000, Ordering::Relaxed);
        clock.set(200);
        assert_eq!(store.purge_expired(200).unwrap(), 1);
        assert_eq!(stored(&store), 0);
    }

    /// A file-backed store browses on its own read-only connection and sees
    /// what the writer committed.
    #[test]
    fn a_file_store_browses_committed_rows_on_a_second_connection() {
        let dir = TempDir::new().unwrap();
        let store = SqliteRefStore::open(dir.path().join("refs.db")).unwrap();
        assert!(store.browse_conn.is_some());
        let tenant = Tenant::new("t");
        let body = "needle in a haystack".to_string();
        store
            .put_many(&tenant, &[(ref_for(&body), body.clone())], &meta())
            .unwrap();
        let page = store.browse(&tenant, &BrowseQuery::default()).unwrap();
        assert_eq!(page.total, 1);
        let found = store
            .browse(
                &tenant,
                &BrowseQuery {
                    search: Some("NEEDLE".into()),
                    ..BrowseQuery::default()
                },
            )
            .unwrap();
        assert_eq!(found.total, 1);
        assert!(store.ping().is_ok());
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

    /// Forty fragments of a few kilobytes each, and their handles.
    fn filled(store: &SqliteRefStore, tag: &str) -> Vec<RefId> {
        (0..40)
            .map(|index| {
                let text = format!("{tag} fragment {index} {}", "x".repeat(4_000));
                let id = ref_for(&text);
                store
                    .put_many(&Tenant::anonymous(), &[(id, text)], &meta())
                    .unwrap();
                id
            })
            .collect()
    }

    #[test]
    fn a_database_file_is_described_checked_and_checkpointed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refs.db");
        let store = SqliteRefStore::open(&path).unwrap();
        let ids = filled(&store, "a");
        let described = facts(&path).unwrap();
        assert_eq!(described.schema_version, latest_version());
        assert!(described.wal);
        assert!(described.page_size >= 512 && described.page_count > 0);
        assert_eq!(integrity_check(&path).unwrap(), Vec::<String>::new());

        let log = dir.path().join("refs.db-wal");
        assert!(
            std::fs::metadata(&log).unwrap().len() > 0,
            "writes are in the log"
        );
        let done = checkpoint(&path).unwrap();
        assert!(done.complete);
        assert_eq!(
            std::fs::metadata(&log).unwrap().len(),
            0,
            "and now in the file"
        );
        assert!(
            store.get(&Tenant::anonymous(), &ids[0], false).is_ok(),
            "the store carries on"
        );
        // Again, with nothing in the log, is nothing to do.
        assert!(checkpoint(&path).unwrap().complete);

        // Deleted rows leave free pages: what a vacuum would reclaim.
        RefBrowse::remove(&store, &Tenant::anonymous(), &ids).unwrap();
        checkpoint(&path).unwrap();
        let after = facts(&path).unwrap();
        assert!(after.free_pages > 0, "{after:?}");
        assert_eq!(
            after.reclaimable_bytes(),
            after.free_pages * after.page_size
        );

        // The breakdown names the table and its index, and accounts for
        // every page of the file.
        let breakdown = space(&path).unwrap();
        assert!(
            breakdown
                .iter()
                .any(|s| s.name == "refs" && s.kind == "table")
        );
        assert!(
            breakdown
                .iter()
                .any(|s| s.name == "refs_expiry" && s.kind == "index" && s.table == "refs")
        );

        // A vacuum hands the free pages back, with the store still open.
        let vacuumed = vacuum(&path).unwrap();
        assert!(vacuumed.folded);
        assert!(vacuumed.bytes_after < vacuumed.bytes_before, "{vacuumed:?}");
        assert_eq!(facts(&path).unwrap().free_pages, 0);
        assert_eq!(integrity_check(&path).unwrap(), Vec::<String>::new());
        let ids = filled(&store, "b");
        assert!(
            store.get(&Tenant::anonymous(), &ids[0], false).is_ok(),
            "the store carries on after a vacuum"
        );
        assert!(optimize(&path).unwrap() > 0, "the planner has figures now");
        drop(store);

        // A file that is not a database is an error, not a clean bill.
        let junk = dir.path().join("junk.db");
        std::fs::write(&junk, vec![7u8; 8_192]).unwrap();
        assert!(facts(&junk).is_err());
        assert!(integrity_check(&junk).is_err());
        assert!(facts(&dir.path().join("missing.db")).is_err());
    }

    /// A new store hands freed pages back by itself; one from before is
    /// converted by its next vacuum and does from then on.
    #[test]
    fn freed_pages_go_back_to_the_disk_without_a_full_vacuum() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refs.db");
        let store = SqliteRefStore::open(&path).unwrap();
        assert_eq!(
            facts(&path).unwrap().auto_vacuum,
            2,
            "incremental from birth"
        );
        assert_eq!(store.reclaim().unwrap(), 0, "nothing free yet");
        let ids = filled(&store, "a");
        RefBrowse::remove(&store, &Tenant::anonymous(), &ids).unwrap();
        checkpoint(&path).unwrap();
        let free = facts(&path).unwrap().reclaimable_bytes();
        assert!(free > 0);
        let reclaimed = store.reclaim().unwrap();
        assert_eq!(reclaimed, free, "every free page, under the per-call bound");
        checkpoint(&path).unwrap();
        assert_eq!(facts(&path).unwrap().free_pages, 0);
        assert_eq!(integrity_check(&path).unwrap(), Vec::<String>::new());
        drop(store);

        // A file written before this, with auto-vacuum off.
        let old = dir.path().join("old.db");
        {
            let conn = Connection::open(&old).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE t (x BLOB);
                 INSERT INTO t SELECT randomblob(4000) FROM generate_series(1, 50);
                 DELETE FROM t;",
            )
            .or_else(|_| {
                // generate_series is an extension; a recursive CTE is not.
                conn.execute_batch(
                    "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 50)
                     INSERT INTO t SELECT randomblob(4000) FROM n; DELETE FROM t;",
                )
            })
            .unwrap();
        }
        assert_eq!(facts(&old).unwrap().auto_vacuum, 0);
        assert!(vacuum(&old).is_ok());
        assert_eq!(
            facts(&old).unwrap().auto_vacuum,
            0,
            "a plain vacuum keeps the mode"
        );
        let converted = vacuum_incremental(&old).unwrap();
        assert!(converted.folded);
        assert_eq!(facts(&old).unwrap().auto_vacuum, 2, "converted");
    }

    /// One burst must not leave a log its size on disk for good.
    #[test]
    fn the_write_ahead_log_is_cut_back_after_a_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteRefStore::open(dir.path().join("refs.db")).unwrap();
        let limit: i64 = store
            .conn
            .lock()
            .query_row("PRAGMA journal_size_limit", [], |row| row.get(0))
            .unwrap();
        assert_eq!(limit as u64, JOURNAL_SIZE_LIMIT);
    }

    #[test]
    fn a_database_someone_has_open_is_in_use_and_is_left_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refs.db");
        assert!(!in_use(&path), "nothing there");
        let store = SqliteRefStore::open(&path).unwrap();
        filled(&store, "c");
        assert!(in_use(&path), "held by an idle connection");
        assert!(in_use(&path), "and asking changed nothing");
        assert!(facts(&path).unwrap().wal);
        drop(store);
        assert!(!in_use(&path), "closed");
        assert!(
            facts(&path).unwrap().wal,
            "left in the mode it was found in"
        );
        let reopened = SqliteRefStore::open(&path).unwrap();
        assert_eq!(reopened.stats().unwrap().live_refs, 40);
        drop(reopened);

        let junk = dir.path().join("junk.db");
        std::fs::write(&junk, vec![7u8; 8_192]).unwrap();
        assert!(!in_use(&junk));
    }

    /// A page of the file overwritten is found.
    #[test]
    fn a_damaged_database_fails_its_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refs.db");
        let store = SqliteRefStore::open(&path).unwrap();
        filled(&store, "b");
        drop(store);
        assert!(checkpoint(&path).unwrap().complete);
        let mut bytes = std::fs::read(&path).unwrap();
        let page = facts(&path).unwrap().page_size as usize;
        assert!(bytes.len() > page * 4);
        for byte in &mut bytes[page * 2..page * 3] {
            *byte = 0xAB;
        }
        std::fs::write(&path, bytes).unwrap();
        let found = integrity_check(&path);
        assert!(
            found.as_ref().map_or(true, |problems| !problems.is_empty()),
            "a clean bill for a damaged file: {found:?}"
        );
    }

    /// Several processes creating one store at the same moment all get it.
    #[test]
    fn openers_racing_to_create_one_store_all_succeed() {
        for _ in 0..3 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("refs.db");
            let start = std::sync::Barrier::new(4);
            let opened: Vec<Result<(), String>> = std::thread::scope(|scope| {
                let openers: Vec<_> = (0..4)
                    .map(|_| {
                        scope.spawn(|| {
                            start.wait();
                            SqliteRefStore::open(&path)
                                .map(|_| ())
                                .map_err(|error| error.to_string())
                        })
                    })
                    .collect();
                openers.into_iter().map(|t| t.join().unwrap()).collect()
            });
            assert!(opened.iter().all(Result::is_ok), "{opened:?}");
        }
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
