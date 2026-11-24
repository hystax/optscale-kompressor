//! The record of maintenance work, and deleting part of the statistics:
//! before a day, for a tenant, a run or an experiment, or only the copy of
//! the settings each request carries.

use kompressor_stats::events::{Event, MaintenanceEvent};
use kompressor_stats::purge::{PurgeKind, PurgeScope, purge};
use kompressor_stats::{StatsConfig, StatsReader, StatsWriter, schema};
use rusqlite::{Connection, params};

const DAY: i64 = 86_400_000;
/// 2026-11-01T00:00:00Z.
const T0: i64 = 1_793_491_200_000;

fn count(path: &std::path::Path, table: &str, filter: &str) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE {filter}"),
            [],
            |row| row.get(0),
        )
        .unwrap()
}

/// One proxied request with its compression and its provenance.
fn request(conn: &Connection, time_ms: i64, tenant: &str, id: &str, run: &str, experiment: &str) {
    conn.execute(
        "INSERT INTO optimization_events (time_ms, tenant, request_id, surface, mode, tokens_in,
            tokens_out, chars_in, chars_out, messages_total, messages_compressed,
            messages_skipped, refs_emitted, ref_bytes_emitted, content_types,
            protected_by_reason, engine_ms)
         VALUES (?1, ?2, ?3, 'proxy', 'recoverable', 1000, 400, 4000, 1600, 3, 1, 0, 1, 10,
                 '{}', '{}', 1.0)",
        params![time_ms, tenant, id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO proxy_requests (time_ms, tenant, request_id, api_family, upstream, status,
            stream, prompt_tokens)
         VALUES (?1, ?2, ?3, 'anthropic', 'u', 200, 0, 1200)",
        params![time_ms, tenant, id],
    )
    .unwrap();
    let context = serde_json::json!({
        "build_version": "test",
        "config_fingerprint": "f",
        "run_id": run,
        "experiment_id": experiment,
        "settings": {"padding": "x".repeat(3_000)},
    });
    conn.execute(
        "INSERT INTO request_context (tenant, request_id, time_ms, model, mode, context)
         VALUES (?1, ?2, ?3, 'm', 'recoverable', ?4)",
        params![tenant, id, time_ms, context.to_string()],
    )
    .unwrap();
}

/// Two old days and one recent, two tenants, two runs.
fn seeded() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stats.db");
    let conn = schema::open_read_write(&path).unwrap();
    for i in 0..10 {
        request(&conn, T0 + i, "acme", &format!("a{i}"), "bench-1", "exp-a");
        request(
            &conn,
            T0 + DAY + i,
            "acme",
            &format!("b{i}"),
            "bench-2",
            "exp-a",
        );
        request(
            &conn,
            T0 + 5 * DAY + i,
            "globex",
            &format!("c{i}"),
            "bench-2",
            "exp-b",
        );
    }
    (dir, path)
}

#[test]
fn a_delete_before_a_day_is_retention_brought_forward() {
    let (_dir, path) = seeded();
    let scope = PurgeScope {
        before_ms: Some(T0 + 2 * DAY + 5_000),
        ..PurgeScope::default()
    };

    let preview = purge(&path, scope.clone(), PurgeKind::Rows, true).unwrap();
    assert!(preview.dry_run);
    assert_eq!(
        preview.scope.before_ms,
        Some(T0 + 2 * DAY),
        "rounded to midnight"
    );
    assert_eq!(preview.tables["proxy_requests"], 20);
    assert_eq!(preview.tables["optimization_events"], 20);
    assert_eq!(preview.tables["request_context"], 20);
    assert!(preview.freed_bytes > 0, "{preview:?}");
    assert_eq!(
        count(&path, "proxy_requests", "1"),
        30,
        "a rehearsal changes nothing"
    );
    assert_eq!(count(&path, "daily_rollups", "1"), 0);

    let done = purge(&path, scope, PurgeKind::Rows, false).unwrap();
    assert_eq!(
        done.tables, preview.tables,
        "the rehearsal said what would happen"
    );
    assert!(done.rolled_up);
    assert_eq!(count(&path, "proxy_requests", "1"), 10);
    assert_eq!(count(&path, "request_context", "1"), 10);
    let reader = StatsReader::open(&path).unwrap();
    let kept = reader.rollups("acme", "2026-11-01", "2026-11-02").unwrap();
    assert_eq!(
        kept.iter().map(|day| day.requests).sum::<i64>(),
        20,
        "the days were rolled up before they went: {kept:?}"
    );
}

#[test]
fn a_tenant_run_or_experiment_is_deleted_and_nothing_else() {
    let (_dir, path) = seeded();
    let by = |scope: PurgeScope| purge(&path, scope, PurgeKind::Rows, false).unwrap();

    let run = by(PurgeScope {
        run_id: Some("bench-1".into()),
        ..PurgeScope::default()
    });
    assert_eq!(run.tables["proxy_requests"], 10, "{run:?}");
    assert!(!run.rolled_up, "a run is made not to have happened");
    assert_eq!(count(&path, "request_context", "request_id LIKE 'a%'"), 0);
    assert_eq!(count(&path, "daily_rollups", "1"), 0);

    let tenant_in_experiment = by(PurgeScope {
        tenant: Some("globex".into()),
        experiment_id: Some("exp-a".into()),
        ..PurgeScope::default()
    });
    assert_eq!(tenant_in_experiment.rows, 0, "both filters must match");

    let tenant = by(PurgeScope {
        tenant: Some(" globex ".into()),
        ..PurgeScope::default()
    });
    assert_eq!(tenant.tables["optimization_events"], 10);
    assert_eq!(
        count(&path, "proxy_requests", "1"),
        10,
        "acme's second day is left"
    );
    assert_eq!(count(&path, "proxy_requests", "tenant = 'acme'"), 10);
}

#[test]
fn trimming_keeps_every_request_and_what_the_history_reports() {
    let (_dir, path) = seeded();
    let reader = StatsReader::open(&path).unwrap();
    let filter = kompressor_stats::history::HistoryFilter::default();
    let before = reader.history("acme", T0, T0 + 3 * DAY, &filter).unwrap();

    let trimmed = purge(
        &path,
        PurgeScope {
            before_ms: Some(T0 + 2 * DAY),
            ..PurgeScope::default()
        },
        PurgeKind::Context,
        false,
    )
    .unwrap();
    assert_eq!(trimmed.tables["request_context"], 20, "{trimmed:?}");
    assert!(
        trimmed.content_bytes.unwrap() > 20 * 3_000,
        "what a vacuum would hand back: {trimmed:?}"
    );
    assert_eq!(count(&path, "proxy_requests", "1"), 30, "no request goes");
    assert_eq!(count(&path, "request_context", "context IS NULL"), 20);

    let after = reader.history("acme", T0, T0 + 3 * DAY, &filter).unwrap();
    assert_eq!(
        serde_json::to_value(&after.metrics).unwrap(),
        serde_json::to_value(&before.metrics).unwrap(),
        "the history was archived before the settings went"
    );
    assert_eq!(after.rows.len(), before.rows.len());
}

#[test]
fn an_unscoped_purge_is_refused() {
    let (_dir, path) = seeded();
    let refused = purge(
        &path,
        PurgeScope {
            tenant: Some("   ".into()),
            ..PurgeScope::default()
        },
        PurgeKind::Rows,
        false,
    );
    assert!(matches!(
        refused,
        Err(kompressor_stats::StatsError::Invalid(_))
    ));
    assert_eq!(count(&path, "proxy_requests", "1"), 30);
}

#[test]
fn the_writer_trims_settings_older_than_their_own_window() {
    let (_dir, path) = seeded();
    let config = StatsConfig {
        context_retention_days: 3,
        ..StatsConfig::at(&path)
    };
    assert_eq!(config.context_cutoff_ms(T0 + 7 * DAY), Some(T0 + 4 * DAY));
    let no_shorter = StatsConfig {
        context_retention_days: 90,
        ..StatsConfig::at(&path)
    };
    assert_eq!(
        no_shorter.context_cutoff_ms(T0),
        None,
        "the requests go first anyway"
    );
    assert_eq!(
        StatsConfig::at(&path).context_cutoff_ms(T0),
        None,
        "off by default"
    );

    let mut writer = StatsWriter::open(&config).unwrap();
    assert_eq!(writer.trim_contexts(T0 + 4 * DAY).unwrap(), 20);
    assert_eq!(writer.trim_contexts(T0 + 4 * DAY).unwrap(), 0, "idempotent");
    assert_eq!(count(&path, "request_context", "context IS NOT NULL"), 10);
}

fn work(time_ms: i64, action: &str, target: &str, ok: bool) -> MaintenanceEvent {
    MaintenanceEvent {
        time_ms,
        action: action.into(),
        target: target.into(),
        origin: "http".into(),
        actor: Some("default".into()),
        took_ms: 12,
        ok,
        bytes_before: Some(2_000),
        bytes_after: Some(1_000),
        detail: Some(r#"{"problems":[]}"#.into()),
        error: (!ok).then(|| "disk full".into()),
    }
}

#[test]
fn maintenance_is_recorded_kept_through_an_erase_and_read_back_newest_first() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stats.db");
    let (sink, handle) = kompressor_stats::spawn(&StatsConfig::at(&path)).unwrap();
    sink.send(Event::Maintenance(work(T0, "vacuum", "stats.db", true)));
    sink.send(Event::Maintenance(work(T0 + 1, "check", "stats.db", true)));
    sink.send(Event::Maintenance(work(
        T0 + 2,
        "vacuum",
        "stats.db",
        false,
    )));
    sink.send(Event::Maintenance(work(T0 + 3, "vacuum", "refs.db", true)));
    sink.flush().unwrap();

    let for_job = path.clone();
    sink.exclusive(move || schema::erase(&for_job))
        .unwrap()
        .unwrap();
    assert_eq!(
        count(&path, "maintenance_events", "1"),
        4,
        "an erase keeps the record"
    );

    let reader = StatsReader::open(&path).unwrap();
    let all = reader.maintenance(10).unwrap();
    assert_eq!(all.len(), 4);
    assert_eq!(all[0], work(T0 + 3, "vacuum", "refs.db", true));
    let latest = reader.latest_maintenance().unwrap();
    assert_eq!(latest.len(), 3, "one per target and action: {latest:?}");
    let vacuum = latest
        .iter()
        .find(|e| e.target == "stats.db" && e.action == "vacuum")
        .unwrap();
    assert!(!vacuum.ok, "the newest, failed one");
    assert_eq!(vacuum.error.as_deref(), Some("disk full"));
    handle.shutdown().unwrap();

    // From a process with no writer, under the rule backups follow.
    assert!(
        kompressor_stats::writer::record_maintenance(
            &path,
            &work(T0 + 9, "check", "refs.db", true)
        )
        .unwrap()
    );
    assert!(
        !kompressor_stats::writer::record_maintenance(
            &dir.path().join("absent.db"),
            &work(T0, "check", "refs.db", true)
        )
        .unwrap(),
        "a database that is not there is not created for one row"
    );
    Connection::open(&path)
        .unwrap()
        .execute_batch("DROP TABLE maintenance_events; PRAGMA user_version = 16;")
        .unwrap();
    assert!(
        !kompressor_stats::writer::record_maintenance(&path, &work(T0, "check", "refs.db", true))
            .unwrap(),
        "nor one an older build migrated"
    );
    assert!(
        StatsReader::open(&path)
            .unwrap()
            .maintenance(5)
            .unwrap()
            .is_empty()
    );
}
