//! Upgrading a *populated* database, version by version.
//!
//! The unit tests build a current-schema database and simulate an older one by
//! dropping columns. That proves the retention logic, not the upgrade: a
//! simulated v5 has whatever the simulation remembered to undo, and the real
//! thing has whatever v5 actually created. These tests build each old version
//! from the shipped migration SQL for that version — the same bytes that
//! created the databases in the field — fill it, and then open it with the
//! current code.
//!
//! Populated matters. An empty database migrates trivially; the failures worth
//! catching are a backfill that touches the wrong rows, an `ALTER TABLE` that
//! collides with something already there, and an archive pass that loses data
//! it was supposed to preserve first.

use kompressor_stats::{StatsConfig, StatsWriter, schema};
use rusqlite::Connection;

const DAY: i64 = 86_400_000;
/// 2023-11-14T22:13:20Z — a fixed instant so the UTC day names are stable.
const T0: i64 = 1_700_000_000_000;

/// Build a database at exactly `version`, using that version's own SQL.
fn database_at(path: &std::path::Path, version: u32) {
    let conn = Connection::open(path).expect("create");
    for (v, sql) in schema::MIGRATIONS {
        if *v > version {
            break;
        }
        conn.execute_batch(sql)
            .unwrap_or_else(|e| panic!("migration v{v} does not apply: {e}"));
    }
    conn.execute_batch(&format!("PRAGMA user_version = {version}"))
        .expect("stamp");
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

/// Insert rows directly, because the writer we are testing is the *new* one and
/// the point is to fill the database while it is still old.
fn fill_v5(path: &std::path::Path) {
    let conn = Connection::open(path).unwrap();
    for (day, tenant) in [(0, "acme"), (0, "globex"), (1, "acme"), (2, "acme")] {
        let t = T0 + day * DAY;
        conn.execute(
            "INSERT INTO optimization_events
               (time_ms, tenant, request_id, surface, mode, tokens_in, tokens_out,
                chars_in, chars_out, messages_total, messages_compressed,
                messages_skipped, refs_emitted, ref_bytes_emitted, content_types,
                protected_by_reason, engine_ms)
             VALUES (?1, ?2, ?3, 'proxy', 'recoverable', 1000, 400, 4000, 1600,
                     3, 1, 2, 1, 2400, '{}', '{}', 1.5)",
            rusqlite::params![t, tenant, format!("req-{day}-{tenant}")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO proxy_requests
               (time_ms, tenant, request_id, api_family, upstream, stream,
                prompt_tokens, completion_tokens, cache_read_tokens,
                cache_write_tokens, conversation)
             VALUES (?1, ?2, ?3, 'anthropic', 'https://u.invalid', 1,
                     4200, 180, 3000, 1200, ?4)",
            rusqlite::params![
                t,
                tenant,
                format!("req-{day}-{tenant}"),
                format!("conv-{tenant}")
            ],
        )
        .unwrap();
    }
}

#[test]
fn a_populated_v5_database_upgrades_to_the_current_schema_without_losing_a_row() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stats.db");
    database_at(&path, 5);
    fill_v5(&path);

    let before = {
        let conn = Connection::open(&path).unwrap();
        (
            count(&conn, "SELECT COUNT(*) FROM optimization_events"),
            count(&conn, "SELECT COUNT(*) FROM proxy_requests"),
        )
    };
    assert_eq!(before, (4, 4));

    // Opening with the current code is what runs the ladder.
    let config = StatsConfig::at(&path);
    let writer = StatsWriter::open(&config).unwrap();
    drop(writer);

    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        count(&conn, "SELECT * FROM pragma_user_version") as u32,
        schema::latest_version(),
        "the ladder ran to the end"
    );
    assert_eq!(
        (
            count(&conn, "SELECT COUNT(*) FROM optimization_events"),
            count(&conn, "SELECT COUNT(*) FROM proxy_requests"),
        ),
        before,
        "an upgrade is not allowed to drop a row"
    );
    // The objects the new migrations add are really there.
    for object in ["daily_requests", "daily_conversations", "v_daily_requests"] {
        assert_eq!(
            count(
                &conn,
                &format!("SELECT COUNT(*) FROM sqlite_master WHERE name = '{object}'")
            ),
            1,
            "{object} was not created"
        );
    }
    // Nothing was pre-marked as archived: no pass has run yet.
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM optimization_events WHERE rolled_up = 1"
        ),
        0
    );
}

#[test]
fn a_v5_database_with_manual_rollups_neither_double_counts_nor_loses_a_tenant() {
    // The shape that broke the first version of the v6 backfill: one tenant
    // rolled up by hand, another tenant's rows on the same day never archived.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stats.db");
    database_at(&path, 5);
    fill_v5(&path);
    {
        // A hand-made rollup covering only one tenant's day, exactly as the old
        // manual `rollup` + `prune` workflow would have left it.
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO daily_rollups
               (day, tenant, mode, surface, requests, tokens_in, tokens_out,
                retrievals, retrieval_hits, retrieved_tokens)
             VALUES ('2023-11-14', 'acme', 'recoverable', 'proxy', 1, 1000, 400, 0, 0, 0)",
            [],
        )
        .unwrap();
    }

    let config = StatsConfig::at(&path);
    let mut writer = StatsWriter::open(&config).unwrap();
    // Archive everything older than the last day of data.
    writer.maintain(T0 + 3 * DAY).unwrap();

    let conn = Connection::open(&path).unwrap();
    let rows: Vec<(String, String, i64)> = conn
        .prepare("SELECT day, tenant, requests FROM daily_rollups ORDER BY day, tenant")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();

    // Four events: acme on three days, globex on the first. Every one archived
    // exactly once — the pre-existing acme rollup neither doubled nor swallowed
    // globex's row.
    assert_eq!(rows.len(), 4, "one row per (day, tenant): {rows:?}");
    for (day, tenant, requests) in &rows {
        assert_eq!(*requests, 1, "{day}/{tenant} counted more than once");
    }
    assert!(
        rows.iter()
            .any(|(d, t, _)| d == "2023-11-14" && t == "globex"),
        "the never-archived tenant survived: {rows:?}"
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM optimization_events"),
        0,
        "the archived rows were then pruned"
    );
    // And the request economy went with them rather than vanishing.
    assert_eq!(count(&conn, "SELECT SUM(requests) FROM daily_requests"), 4);
    assert_eq!(
        count(&conn, "SELECT SUM(conversations) FROM v_daily_requests"),
        4,
        "two conversations, each active on the days it ran"
    );
}

#[test]
fn a_v8_database_gains_strip_usage_without_disturbing_its_rows() {
    // The column arrived to settle a measured discrepancy between what an agent
    // reports it spent and what the proxy saw go past, so an upgrade has to
    // leave the existing rows readable and answer honestly about them: a row
    // written before the column existed cannot claim its usage was stripped.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stats.db");
    database_at(&path, 8);
    fill_v5(&path);

    let before = {
        let conn = Connection::open(&path).unwrap();
        count(&conn, "SELECT COUNT(*) FROM proxy_requests")
    };
    assert_eq!(before, 4);

    let config = StatsConfig::at(&path);
    drop(StatsWriter::open(&config).unwrap());

    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        count(&conn, "SELECT * FROM pragma_user_version") as u32,
        schema::latest_version()
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM proxy_requests"),
        before,
        "the upgrade is not allowed to drop a row"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM pragma_table_info('proxy_requests') WHERE name = 'strip_usage'"
        ),
        1,
        "the column was not added"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM proxy_requests WHERE strip_usage = 0"
        ),
        before,
        "rows that predate the column must read 0, not NULL: the question it \
         answers is whether usage WAS stripped, and for these it was not"
    );
}

#[test]
fn every_version_in_the_ladder_upgrades_from_where_it_stops() {
    // A database created by any shipped version must reach the current one. The
    // ladder is applied one step at a time, so a migration that only works when
    // run straight after its predecessor is a real possibility.
    for (version, _) in schema::MIGRATIONS {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        database_at(&path, *version);
        if *version >= 5 {
            fill_v5(&path);
        }
        let config = StatsConfig::at(&path);
        StatsWriter::open(&config).unwrap_or_else(|e| panic!("v{version} does not upgrade: {e}"));
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            count(&conn, "SELECT * FROM pragma_user_version") as u32,
            schema::latest_version(),
            "v{version} stopped short"
        );
    }
}

#[test]
fn a_v11_database_gains_keep_warm_and_its_rows_are_client_requests() {
    // Every row written before keep-warm pings existed was a client request,
    // so the upgrade must say so: a NULL here would drop them from any count
    // that filters the pings out with `keep_warm = 0`.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stats.db");
    database_at(&path, 11);
    fill_v5(&path);

    let config = StatsConfig::at(&path);
    drop(StatsWriter::open(&config).unwrap());

    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        count(&conn, "SELECT * FROM pragma_user_version") as u32,
        schema::latest_version()
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM proxy_requests WHERE keep_warm = 0"
        ),
        4,
        "rows that predate the column read 0, not NULL"
    );
}

#[test]
fn a_v13_database_gains_range_status_and_keeps_its_retrievals() {
    // Rows written before the column existed asked for no status at all, so
    // they read NULL — the same as a retrieval that sent no range — and the
    // writer fills the column from then on.
    use kompressor_stats::events::{Event, RetrievalEvent, Surface};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stats.db");
    database_at(&path, 13);
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO retrieval_events
               (time_ms, tenant, request_id, surface, \"ref\", found, expired, ranged,
                fragment_bytes, fragment_chars, fragment_tokens, returned_chars,
                returned_tokens, withheld)
             VALUES (?1, 'acme', 'req-old', 'proxy', 'k_AbC123dEf456', 1, 0, 0,
                     400, 400, 100, 400, 100, 0)",
            [T0],
        )
        .unwrap();
    }

    let mut writer = StatsWriter::open(&StatsConfig::at(&path)).unwrap();
    writer
        .write_batch(&[Event::Retrieval(RetrievalEvent {
            context: None,
            time_ms: T0 + 1,
            tenant: "acme".into(),
            request_id: Some("req-new".into()),
            surface: Surface::Proxy,
            reference: "k_AbC123dEf456".into(),
            found: true,
            expired: false,
            ranged: true,
            fragment_bytes: 120,
            fragment_chars: 120,
            fragment_tokens: 30,
            returned_chars: 120,
            returned_tokens: 30,
            withheld: false,
            range_status: Some("normalized".into()),
        })])
        .unwrap();
    drop(writer);

    let conn = Connection::open(&path).unwrap();
    let statuses: Vec<(String, Option<String>)> = conn
        .prepare("SELECT request_id, range_status FROM retrieval_events ORDER BY time_ms")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        statuses,
        vec![
            ("req-old".to_string(), None),
            ("req-new".to_string(), Some("normalized".to_string())),
        ]
    );
}

/// Several processes open one file, and after an upgrade the first to start
/// all find it behind. Each must come up: the one that gets there first
/// applies a step, the others wait for it and skip what it did.
///
/// Reading the version before taking the write lock had every one of them
/// start the same step, and all but the first fail on "table already exists"
/// — a `serve` and an `mcp serve` started together after an upgrade, and one
/// of them did not start.
#[test]
fn openers_racing_to_upgrade_one_database_all_succeed() {
    // v12 is what the last release before the v13–v15 steps left behind; v0
    // is a first start with nothing on disk.
    for from in [12, 0] {
        for round in 0..3 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("stats.db");
            if from > 0 {
                database_at(&path, from);
                fill_v5(&path);
            }
            let before = (from > 0).then(|| {
                let conn = Connection::open(&path).unwrap();
                count(&conn, "SELECT COUNT(*) FROM optimization_events")
            });
            let start = std::sync::Barrier::new(4);
            let versions: Vec<Result<u32, String>> = std::thread::scope(|scope| {
                let openers: Vec<_> = (0..4)
                    .map(|_| {
                        scope.spawn(|| {
                            start.wait();
                            schema::open_read_write(&path)
                                .map(|conn| {
                                    count(&conn, "SELECT * FROM pragma_user_version") as u32
                                })
                                .map_err(|e| e.to_string())
                        })
                    })
                    .collect();
                openers.into_iter().map(|t| t.join().unwrap()).collect()
            });
            for version in &versions {
                assert_eq!(
                    version.as_ref().ok(),
                    Some(&schema::latest_version()),
                    "from v{from}, round {round}: {versions:?}"
                );
            }
            let conn = Connection::open(&path).unwrap();
            let sound: String = conn
                .query_row("PRAGMA integrity_check", [], |row| row.get(0))
                .unwrap();
            assert_eq!(sound, "ok");
            if let Some(before) = before {
                assert_eq!(
                    count(&conn, "SELECT COUNT(*) FROM optimization_events"),
                    before,
                    "no row lost or doubled"
                );
            }
        }
    }
}

/// Applying the ladder to a database that is already current changes nothing
/// and leaves no transaction open.
#[test]
fn a_database_that_is_already_current_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stats.db");
    let conn = schema::open_read_write(&path).unwrap();
    let schema_before: i64 = count(&conn, "SELECT * FROM pragma_schema_version");
    assert_eq!(schema::apply(&conn).unwrap(), schema::latest_version());
    assert_eq!(schema::apply(&conn).unwrap(), schema::latest_version());
    assert_eq!(
        count(&conn, "SELECT * FROM pragma_schema_version"),
        schema_before,
        "nothing was altered"
    );
    assert!(conn.is_autocommit(), "no transaction left open");
}

/// v16 adds the record of backups. A v15 database — what 2.5.0 left behind —
/// gains the table, keeps every row, and the record can be written and read.
#[test]
fn a_v15_database_gains_the_backup_record_and_keeps_its_rows() {
    use kompressor_stats::events::BackupEvent;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stats.db");
    database_at(&path, 15);
    fill_v5(&path);
    let before = {
        let conn = Connection::open(&path).unwrap();
        count(&conn, "SELECT COUNT(*) FROM optimization_events")
    };
    let event = |time_ms, ok| BackupEvent {
        time_ms,
        origin: "cli".to_string(),
        destination: "/backups/a.tar.gz".to_string(),
        bytes: 1_024,
        databases: 2,
        ok,
        error: (!ok).then(|| "disk full".to_string()),
    };
    // Not migrated yet: a backup does not upgrade the schema to leave a note.
    assert!(!kompressor_stats::writer::record_backup(&path, &event(T0, true)).unwrap());
    assert!(
        !kompressor_stats::writer::record_backup(&dir.path().join("absent.db"), &event(T0, true))
            .unwrap()
    );
    assert!(
        !dir.path().join("absent.db").exists(),
        "and creates nothing"
    );
    let reader = kompressor_stats::StatsReader::open(&path).unwrap();
    assert!(
        reader.backups(10).unwrap().is_empty(),
        "an older file has none on record"
    );
    assert_eq!(reader.last_backup().unwrap(), (None, None));
    drop(reader);

    StatsWriter::open(&StatsConfig::at(&path)).expect("v15 upgrades");
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        count(&conn, "SELECT * FROM pragma_user_version") as u32,
        schema::latest_version()
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM optimization_events"),
        before
    );
    drop(conn);

    for (time_ms, ok) in [(T0, true), (T0 + DAY, true), (T0 + 2 * DAY, false)] {
        assert!(kompressor_stats::writer::record_backup(&path, &event(time_ms, ok)).unwrap());
    }
    let reader = kompressor_stats::StatsReader::open(&path).unwrap();
    let listed = reader.backups(2).unwrap();
    assert_eq!(listed.len(), 2, "newest first, and no more than asked");
    assert_eq!(listed[0], event(T0 + 2 * DAY, false));
    assert_eq!(listed[1].time_ms, T0 + DAY);
    assert_eq!(
        reader.last_backup().unwrap(),
        (Some(T0 + DAY), Some(T0 + 2 * DAY)),
        "the last that worked, and the failure since"
    );
    // Once one works again, the failure is history.
    assert!(kompressor_stats::writer::record_backup(&path, &event(T0 + 3 * DAY, true)).unwrap());
    assert_eq!(reader.last_backup().unwrap(), (Some(T0 + 3 * DAY), None));
}
