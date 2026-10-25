//! The `stats.db` schema and its migration ladder.

use crate::error::StatsError;
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

/// Version 1: every table, index and view the crate queries.
///
/// The statements are `IF NOT EXISTS` throughout, so applying this to a
/// database that already has it is a no-op even if the version bookkeeping
/// below were to lose its place.
const V1: &str = r#"
CREATE TABLE IF NOT EXISTS optimization_events (
  id INTEGER PRIMARY KEY, time_ms INTEGER NOT NULL, tenant TEXT NOT NULL, request_id TEXT NOT NULL,
  surface TEXT NOT NULL CHECK (surface IN ('api','mcp','proxy','cli')),
  api_family TEXT, model TEXT, provider TEXT,
  mode TEXT NOT NULL CHECK (mode IN ('aggressive','lossless','recoverable','off')),
  tokens_in INTEGER NOT NULL, tokens_out INTEGER NOT NULL, chars_in INTEGER NOT NULL, chars_out INTEGER NOT NULL,
  messages_total INTEGER NOT NULL, messages_compressed INTEGER NOT NULL, messages_skipped INTEGER NOT NULL,
  refs_emitted INTEGER NOT NULL, ref_bytes_emitted INTEGER NOT NULL,
  content_types TEXT NOT NULL CHECK (json_valid(content_types)),
  protected_by_reason TEXT NOT NULL CHECK (json_valid(protected_by_reason)),
  engine_ms REAL NOT NULL);
CREATE INDEX IF NOT EXISTS oe_tenant_time ON optimization_events (tenant, time_ms);
CREATE INDEX IF NOT EXISTS oe_request ON optimization_events (request_id);

CREATE TABLE IF NOT EXISTS retrieval_events (
  id INTEGER PRIMARY KEY, time_ms INTEGER NOT NULL, tenant TEXT NOT NULL, request_id TEXT,
  surface TEXT NOT NULL, ref TEXT NOT NULL, found INTEGER NOT NULL, expired INTEGER NOT NULL DEFAULT 0,
  ranged INTEGER NOT NULL DEFAULT 0, fragment_bytes INTEGER NOT NULL, fragment_chars INTEGER NOT NULL,
  fragment_tokens INTEGER NOT NULL, returned_chars INTEGER NOT NULL, returned_tokens INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS re_tenant_time ON retrieval_events (tenant, time_ms);
CREATE INDEX IF NOT EXISTS re_request ON retrieval_events (request_id);
CREATE INDEX IF NOT EXISTS re_ref ON retrieval_events (tenant, ref);

CREATE TABLE IF NOT EXISTS protected_ref_events (
  id INTEGER PRIMARY KEY, time_ms INTEGER NOT NULL, tenant TEXT NOT NULL, request_id TEXT,
  refs TEXT NOT NULL CHECK (json_valid(refs)), refs_total INTEGER NOT NULL,
  reasons TEXT NOT NULL CHECK (json_valid(reasons)), messages INTEGER NOT NULL, touched INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS pe_tenant_time ON protected_ref_events (tenant, time_ms);

CREATE TABLE IF NOT EXISTS proxy_requests (
  id INTEGER PRIMARY KEY, time_ms INTEGER NOT NULL, tenant TEXT NOT NULL, request_id TEXT NOT NULL,
  api_family TEXT NOT NULL, model TEXT, upstream TEXT NOT NULL, status INTEGER, stream INTEGER NOT NULL,
  compression_event_id INTEGER REFERENCES optimization_events(id),
  prompt_tokens INTEGER, completion_tokens INTEGER, cache_read_tokens INTEGER, cache_write_tokens INTEGER,
  shadow_prompt_tokens INTEGER, retrieve_rounds INTEGER NOT NULL DEFAULT 0,
  retrieve_tokens_injected INTEGER NOT NULL DEFAULT 0, upstream_ms REAL, total_ms REAL, error TEXT);
CREATE INDEX IF NOT EXISTS pr_tenant_time ON proxy_requests (tenant, time_ms);
CREATE INDEX IF NOT EXISTS pr_request ON proxy_requests (request_id);

CREATE TABLE IF NOT EXISTS store_samples (time_ms INTEGER PRIMARY KEY, live_refs INTEGER NOT NULL, live_bytes INTEGER NOT NULL);

CREATE TABLE IF NOT EXISTS daily_rollups (
  day TEXT NOT NULL, tenant TEXT NOT NULL, mode TEXT NOT NULL, surface TEXT NOT NULL,
  requests INTEGER, tokens_in INTEGER, tokens_out INTEGER, retrievals INTEGER, retrieval_hits INTEGER,
  retrieved_tokens INTEGER, PRIMARY KEY (day, tenant, mode, surface));

CREATE VIEW IF NOT EXISTS v_daily_savings AS
SELECT date(time_ms/1000,'unixepoch') AS day, tenant, mode, COUNT(*) AS requests,
       SUM(tokens_in - tokens_out) AS saved_gross, SUM(tokens_in) AS tokens_in, SUM(tokens_out) AS tokens_out,
       1.0 - CAST(SUM(tokens_out) AS REAL)/NULLIF(SUM(tokens_in),0) AS avg_reduction
FROM optimization_events GROUP BY day, tenant, mode;

CREATE VIEW IF NOT EXISTS v_retrieval_hit_rate AS
SELECT tenant, date(time_ms/1000,'unixepoch') AS day, COUNT(*) AS retrievals, SUM(found) AS hits,
       SUM(expired) AS expiries, CAST(SUM(found) AS REAL)/COUNT(*) AS hit_rate, SUM(returned_tokens) AS retrieved_tokens
FROM retrieval_events GROUP BY tenant, day;

CREATE VIEW IF NOT EXISTS v_request_economy AS
SELECT o.tenant, o.request_id, o.time_ms, o.mode, o.model, (o.tokens_in - o.tokens_out) AS saved_gross_est,
       COALESCE((SELECT SUM(r.returned_tokens) FROM retrieval_events r
                 WHERE r.tenant=o.tenant AND r.request_id=o.request_id AND r.found=1),0) AS retrieved_tokens,
       (o.tokens_in - o.tokens_out) - COALESCE((SELECT SUM(r.returned_tokens) FROM retrieval_events r
                 WHERE r.tenant=o.tenant AND r.request_id=o.request_id AND r.found=1),0) AS saved_net_est,
       p.prompt_tokens, p.cache_read_tokens, p.retrieve_rounds
FROM optimization_events o LEFT JOIN proxy_requests p ON p.tenant=o.tenant AND p.request_id=o.request_id;

CREATE VIEW IF NOT EXISTS v_type_coverage AS
SELECT tenant, j.key AS content_type, SUM(j.value) AS messages
FROM optimization_events, json_each(optimization_events.content_types) AS j GROUP BY tenant, content_type;
"#;

/// Version 2: name why a run removed nothing.
///
/// A zero in `tokens_in - tokens_out` is a fact with at least three different
/// causes, and an operator cannot act on any of them without being told which.
/// Added after a wrapped Claude Code session removed nothing at all and the
/// stats recorded it as an ordinary healthy request.
const V2: &str = r#"
ALTER TABLE optimization_events ADD COLUMN outcome TEXT;
CREATE INDEX IF NOT EXISTS oe_outcome ON optimization_events (tenant, outcome);

CREATE VIEW IF NOT EXISTS v_outcomes AS
SELECT tenant, date(time_ms/1000,'unixepoch') AS day,
       coalesce(outcome, 'unrecorded') AS outcome,
       COUNT(*) AS requests, SUM(tokens_in) AS tokens_in,
       SUM(tokens_in - tokens_out) AS saved
FROM optimization_events GROUP BY tenant, day, outcome;
"#;

/// Version 3: a rewrite the caller threw away is not a saving.
///
/// The optimization row is written when the engine finishes; the proxy then
/// serializes the rewritten body and, if it came out larger than the original,
/// forwards the original instead. Until now the row still claimed the shrink —
/// 16 of 92 requests on one measured run, 2.6% of its headline. `Event::Discarded`
/// marks those rows, and the two views that answer "what did we save" stop
/// counting them. `v_outcomes` still does, because how often this happens is
/// worth knowing.
const V3: &str = r#"
DROP VIEW IF EXISTS v_daily_savings;
CREATE VIEW v_daily_savings AS
SELECT date(time_ms/1000,'unixepoch') AS day, tenant, mode, COUNT(*) AS requests,
       SUM(tokens_in - tokens_out) AS saved_gross, SUM(tokens_in) AS tokens_in, SUM(tokens_out) AS tokens_out,
       1.0 - CAST(SUM(tokens_out) AS REAL)/NULLIF(SUM(tokens_in),0) AS avg_reduction
FROM optimization_events
WHERE coalesce(outcome, '') <> 'discarded'
GROUP BY day, tenant, mode;

DROP VIEW IF EXISTS v_request_economy;
CREATE VIEW v_request_economy AS
SELECT o.tenant, o.request_id, o.time_ms, o.mode, o.model, (o.tokens_in - o.tokens_out) AS saved_gross_est,
       COALESCE((SELECT SUM(r.returned_tokens) FROM retrieval_events r
                 WHERE r.tenant=o.tenant AND r.request_id=o.request_id AND r.found=1),0) AS retrieved_tokens,
       (o.tokens_in - o.tokens_out) - COALESCE((SELECT SUM(r.returned_tokens) FROM retrieval_events r
                 WHERE r.tenant=o.tenant AND r.request_id=o.request_id AND r.found=1),0) AS saved_net_est,
       p.prompt_tokens, p.cache_read_tokens, p.retrieve_rounds
FROM optimization_events o LEFT JOIN proxy_requests p ON p.tenant=o.tenant AND p.request_id=o.request_id
WHERE coalesce(o.outcome, '') <> 'discarded';
"#;

/// The migration ladder, applied in **numeric** order of the `u32`.
///
/// Ordering by the number rather than by a name is the whole reason this is a
/// slice of pairs and not a directory read. The reference service listed
/// migration *filenames* and sorted them as strings, so `10_…` sorted before
/// `2_…`: the ladder was correct only as long as it stayed under ten steps, and
/// would have applied a tenth migration against a second-migration schema the
/// first time it grew past that. Here the key is an integer and the sort is
/// numeric, so growth cannot reorder anything.
/// Version 4: say why a message was skipped, not just how many were.
///
/// `messages_skipped` has always been a bare count, and on agent traffic it is
/// the majority — one Harbor run skipped 6,368 of 8,928 and nothing recorded a
/// reason for any of them. That is where the remaining headroom is, so the most
/// useful question about a run could not be asked of the data.
///
/// `protected_by_reason` covered one part of it: the messages protection
/// withheld. It never covered the ones that reached the compressors and were
/// declined, which are the interesting half — `content_types` counts only
/// successes, so a git workload where every diff is declined by the §7.6 gates
/// reported `diff: 0`, indistinguishable from "no diffs present".
///
/// `frozen_messages` is added alongside because the engine has always computed
/// it, `CompressStats::outcome` reads it, and it was then dropped on the floor
/// rather than written.
const V4: &str = r#"
ALTER TABLE optimization_events ADD COLUMN skipped_by_reason TEXT;
ALTER TABLE optimization_events ADD COLUMN frozen_messages INTEGER;

-- Skips, unrolled one row per reason, so "where did the rest go" is a query
-- rather than a JSON walk. Mirrors `v_type_mix` over `content_types`.
CREATE VIEW IF NOT EXISTS v_skip_reasons AS
SELECT tenant, j.key AS reason, SUM(j.value) AS messages
FROM optimization_events, json_each(optimization_events.skipped_by_reason) AS j
WHERE optimization_events.skipped_by_reason IS NOT NULL
GROUP BY tenant, reason;
"#;

/// Version 5: rank the skips, and name the conversation.
///
/// V4 attributed every skip but only by message count, which cannot rank
/// anything: measured over 16 Harbor runs, `below_min_chars` was 20.9% of
/// 32,082 skipped messages and tiny by construction while `opaque` was 17.9%
/// and possibly enormous. The two read identically in a count.
///
/// `conversation` is the prefix tracker's fingerprint. `tokens_in`/`tokens_out`
/// are per-request sums, so a message compressed on turn ten is counted again
/// every turn after; grouping by conversation is what turns that windowed
/// over-count into unique bytes. Left null where nothing was frozen.
const V5: &str = r#"
ALTER TABLE optimization_events ADD COLUMN skipped_tokens_by_reason TEXT;
ALTER TABLE proxy_requests ADD COLUMN conversation TEXT;

CREATE INDEX IF NOT EXISTS pr_conversation ON proxy_requests (conversation, time_ms);

-- Skips with both weights side by side. Replaces the V4 view, which counted
-- messages alone; `tokens` is NULL for rows written before this migration and
-- 0 for `opaque`, where the engine has no text to measure.
DROP VIEW IF EXISTS v_skip_reasons;
CREATE VIEW v_skip_reasons AS
SELECT tenant, j.key AS reason,
       SUM(j.value) AS messages,
       SUM(COALESCE(json_extract(optimization_events.skipped_tokens_by_reason,
                                 '$.' || j.key), 0)) AS tokens
FROM optimization_events, json_each(optimization_events.skipped_by_reason) AS j
WHERE optimization_events.skipped_by_reason IS NOT NULL
GROUP BY tenant, reason;

-- One row per conversation: how many turns, and what the prefix actually cost.
-- `cache_write` is the unique prefix size because each token is written once;
-- `cache_read` is the windowed sum and grows with the square of the turns.
CREATE VIEW IF NOT EXISTS v_conversations AS
SELECT tenant, conversation,
       COUNT(*) AS turns,
       MIN(time_ms) AS started_ms,
       MAX(time_ms) AS last_ms,
       SUM(cache_write_tokens) AS cache_write,
       SUM(cache_read_tokens) AS cache_read,
       SUM(completion_tokens) AS completion
FROM proxy_requests
WHERE conversation IS NOT NULL
GROUP BY tenant, conversation;
"#;

/// Older databases may carry manually computed rollups beside their raw rows,
/// and the archive must neither count those rows twice nor lose them.
///
/// Marking rows as already-counted cannot do that correctly. Nothing recorded
/// *which* rows a manual rollup covered: the first attempt matched on date
/// alone while a rollup is keyed `(day, tenant, mode, surface)`, so a tenant
/// whose rows landed on a day another tenant had been rolled up for was marked
/// counted, skipped by the next pass, and deleted — the only copy. Narrowing
/// the match to `(day, tenant)` fixes that case and still misses rows that
/// arrived *after* their own tenant's rollup, which is equally unknowable.
///
/// So the raw rows are treated as authoritative wherever they still exist: any
/// rollup for a `(day, tenant)` that still has raw rows is dropped, every row
/// keeps the default `rolled_up = 0`, and the next maintenance pass recomputes
/// that day exactly. Rollups whose raw rows were already pruned are untouched —
/// they are the only copy. Dropping them all, rather than only the days past
/// the horizon, is deliberate: a stale rollup left in place would be ADDED to
/// when its day eventually aged out, which is the double count this exists to
/// prevent.
const V6: &str = r#"
ALTER TABLE optimization_events ADD COLUMN rolled_up INTEGER NOT NULL DEFAULT 0;
ALTER TABLE retrieval_events ADD COLUMN rolled_up INTEGER NOT NULL DEFAULT 0;
DELETE FROM daily_rollups
 WHERE EXISTS (SELECT 1 FROM optimization_events e
                WHERE e.tenant = daily_rollups.tenant
                  AND date(e.time_ms/1000,'unixepoch') = daily_rollups.day)
    OR EXISTS (SELECT 1 FROM retrieval_events r
                WHERE r.tenant = daily_rollups.tenant
                  AND date(r.time_ms/1000,'unixepoch') = daily_rollups.day);
"#;

/// `proxy_requests` is pruned like everything else, and `daily_rollups`
/// aggregates only the engine's own events — so without this the billed-prompt
/// side of the economy vanished at the retention horizon with nothing left
/// behind. A separate table rather than more columns on `daily_rollups`,
/// because the grain differs: a proxy request has no `mode` or `surface`, and
/// forcing it into that key would either invent values or split every day in
/// two.
///
/// What this preserves is the DAILY economy: how much prompt was billed, how
/// much of it the provider served from cache, how much was written. What it
/// cannot preserve is per-turn ordering, and therefore the incremental-cache
/// identity `read[n] + write[n] == read[n+1]` and the excess-writes figure
/// derived from it. Those need the raw rows; past the horizon they are gone,
/// and that is stated here so it is not rediscovered from an empty query.
///
/// There is deliberately no `conversations` *column* here. A DISTINCT count
/// cannot be summed: a day archived in two passes, because rows arrived late,
/// would add the same conversation twice and report two. The identities are
/// kept in `daily_conversations` instead (V8), where set semantics make the
/// count exact however many passes a day takes.
const V7: &str = r#"
CREATE TABLE IF NOT EXISTS daily_requests (
  day            TEXT    NOT NULL,
  tenant         TEXT    NOT NULL,
  requests       INTEGER NOT NULL DEFAULT 0,
  prompt_tokens  INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
  cache_write_tokens INTEGER NOT NULL DEFAULT 0,
  completion_tokens  INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (day, tenant));
ALTER TABLE proxy_requests ADD COLUMN rolled_up INTEGER NOT NULL DEFAULT 0;
"#;

/// Which conversations were active on a day, as a set rather than a count.
///
/// A count is the obvious thing to store and the wrong one: it has to be
/// summed when a day is archived in more than one pass, and summing a DISTINCT
/// count double-counts every conversation that appears in both. Keeping the
/// identities and counting them on read is exact however the archiving is
/// split, because `INSERT OR IGNORE` against the primary key is a set union.
///
/// The identity is the prefix tracker's fingerprint — a 16-character hex hash
/// of the conversation's opening, not its content — so this stores no more
/// about a conversation than `proxy_requests` already did, and keeps it only
/// for days that have been archived.
///
/// `v_daily_requests` joins the count back on, so a reader still gets one row
/// per day and tenant without knowing the table exists.
const V8: &str = r#"
CREATE TABLE IF NOT EXISTS daily_conversations (
  day          TEXT NOT NULL,
  tenant       TEXT NOT NULL,
  conversation TEXT NOT NULL,
  PRIMARY KEY (day, tenant, conversation));

CREATE VIEW IF NOT EXISTS v_daily_requests AS
SELECT r.day, r.tenant, r.requests, r.prompt_tokens, r.cache_read_tokens,
       r.cache_write_tokens, r.completion_tokens,
       (SELECT COUNT(*) FROM daily_conversations c
         WHERE c.day = r.day AND c.tenant = r.tenant) AS conversations
  FROM daily_requests r;
"#;

/// Whether the proxy removed the `usage` it had asked for on the client's
/// behalf.
///
/// The proxy adds `stream_options.include_usage` when the client did not ask
/// for it, so the retrieve loop can account for rounds the client never sees —
/// and then strips the usage frame back out, because handing a client a field
/// it did not request is a change to the response it contracted for. The
/// consequence is that for those requests the client has no usage to report.
///
/// Measured 2026-10-02: across seven Harbor runs the agent's self-reported
/// prompt tokens matched the proxy's own count to within exactly 13 tokens —
/// the probe request the proxy sees and the agent does not — on six of them,
/// and on the seventh the agent reported 36,746 fewer (2.3%), with no
/// duplicate row, abandoned request, bad status or retrieve round to account
/// for it. Without this column there is no way to test whether stripped usage
/// is where the two accountings part company.
const V9: &str = r#"
ALTER TABLE proxy_requests ADD COLUMN strip_usage INTEGER NOT NULL DEFAULT 0;
"#;

/// A retrieval the budget refused, which until now was logged and never
/// recorded.
///
/// `proxy.retrieve_max_tokens_per_request` is checked before the store is
/// read, deliberately: a fragment the budget cannot afford must not file a
/// retrieval event for content the model never receives, or `returned_tokens`
/// counts bytes nobody was sent. The side effect was that a refusal left no
/// trace in the database at all, and the one number an operator reads to tune
/// that budget could not see it bind.
///
/// Measured on a relayed sweep, 2026-10-09: 7 fragments restored and 11
/// refused on the same connection, reported as `hits 7 (100.0%), 0 missed`.
/// A hit rate computed over the asks that survived the gate, in a report with
/// no other mention of the gate, reads as a perfect retrieval record for a
/// request that was turned away 11 times.
///
/// Recorded as its own flag rather than as a miss. `found = 0` means the store
/// did not have it, which is a fragment that expired or a handle the client
/// invented — both faults of the store or the client, and neither one this.
/// Folding them together would have made the budget tightening look like a
/// store regression. Every existing denominator therefore excludes these rows
/// and the count is reported beside the rate, not inside it.
const V10: &str = r#"
ALTER TABLE retrieval_events ADD COLUMN withheld INTEGER NOT NULL DEFAULT 0;

DROP VIEW IF EXISTS v_retrieval_hit_rate;
CREATE VIEW v_retrieval_hit_rate AS
SELECT tenant, date(time_ms/1000,'unixepoch') AS day,
       SUM(1 - withheld) AS retrievals, SUM(CASE WHEN withheld = 0 THEN found ELSE 0 END) AS hits,
       SUM(CASE WHEN withheld = 0 THEN expired ELSE 0 END) AS expiries,
       SUM(withheld) AS withheld,
       CAST(SUM(CASE WHEN withheld = 0 THEN found ELSE 0 END) AS REAL)/NULLIF(SUM(1 - withheld),0) AS hit_rate,
       SUM(CASE WHEN withheld = 0 THEN returned_tokens ELSE 0 END) AS retrieved_tokens
FROM retrieval_events GROUP BY tenant, day;
"#;

/// v11: the part of `cache_write_tokens` the provider billed as one-hour cache
/// entries. Anthropic prices those writes at 2x input against 1.25x for the
/// five-minute default, and `proxy.cache_ttl` decides which a request asks
/// for, so a cost comparison of TTL settings cannot be priced from the total.
/// NULL where the provider reported no split; the five-minute part is the
/// total less this.
const V11: &str = r#"
ALTER TABLE proxy_requests ADD COLUMN cache_write_1h_tokens INTEGER;
"#;

pub const MIGRATIONS: &[(u32, &str)] = &[
    (1, V1),
    (2, V2),
    (3, V3),
    (4, V4),
    (5, V5),
    (6, V6),
    (7, V7),
    (8, V8),
    (9, V9),
    (10, V10),
    (11, V11),
];

/// The version a freshly migrated database reports in `PRAGMA user_version`.
pub fn latest_version() -> u32 {
    MIGRATIONS.iter().map(|(v, _)| *v).max().unwrap_or(0)
}

/// Bring `conn` up to [`latest_version`], returning the version it now holds.
///
/// Idempotent: a database already at the latest version is untouched, and each
/// step runs inside its own transaction so a failure leaves the file at the
/// last version that fully applied rather than half-way through one.
pub fn apply(conn: &Connection) -> Result<u32, StatsError> {
    let mut current: u32 = {
        let raw: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        // a negative or absurd user_version is not ours; treat it as unversioned
        u32::try_from(raw).unwrap_or(0)
    };

    let mut ladder: Vec<&(u32, &str)> = MIGRATIONS.iter().collect();
    ladder.sort_by_key(|(version, _)| *version);

    for (version, sql) in ladder {
        if *version <= current {
            continue;
        }
        conn.execute_batch("BEGIN")?;
        match conn.execute_batch(sql) {
            Ok(()) => {
                // user_version takes no parameter binding, and the value is a
                // crate constant rather than anything a caller supplies
                conn.execute_batch(&format!("PRAGMA user_version = {version}"))?;
                conn.execute_batch("COMMIT")?;
                current = *version;
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(StatsError::Sqlite(e));
            }
        }
    }

    Ok(current)
}

/// Pragmas every connection to `stats.db` sets, read-only ones included.
///
/// `WAL` is what lets the reader and the CLI query the file while the writer
/// thread is committing into it; `synchronous=NORMAL` trades an fsync per
/// commit for the possibility of losing the last few batches on a power cut,
/// which for analytics is the right side of that trade; `busy_timeout` is what
/// stops a concurrent `prune` from turning into an immediate `SQLITE_BUSY` in
/// the writer.
fn set_pragmas(conn: &Connection, read_only: bool) -> Result<(), StatsError> {
    if !read_only {
        // journal_mode returns a row, and it cannot be changed from a read-only
        // connection: the mode is a property of the file, already set by the
        // writer that created it.
        let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
    }
    conn.execute_batch("PRAGMA busy_timeout=5000;")?;
    Ok(())
}

/// Open `path` for writing, creating and migrating it if needed.
pub fn open_read_write(path: &Path) -> Result<Connection, StatsError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|source| StatsError::Directory {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let conn = Connection::open(path).map_err(|source| StatsError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    set_pragmas(&conn, false)?;
    apply(&conn)?;
    Ok(conn)
}

/// Open `path` read-only.
///
/// Read-only is not decoration: the reader backs an HTTP endpoint and a CLI,
/// and a query that cannot write cannot take a write lock, cannot migrate a
/// schema out from under the running writer, and cannot be the thing that
/// corrupts the file.
pub fn open_read_only(path: &Path) -> Result<Connection, StatsError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|source| StatsError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    set_pragmas(&conn, true)?;
    Ok(conn)
}
