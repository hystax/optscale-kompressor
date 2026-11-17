//! Column coverage against a real, migrated database.

use rusqlite::{Connection, params};
use tempfile::TempDir;

use super::*;
use crate::schema;

const MIN: i64 = 60_000;
const DAY: i64 = 86_400_000;

fn db() -> (TempDir, Connection, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("stats.db");
    let conn = schema::open_read_write(&path).unwrap();
    (dir, conn, path)
}

/// An answered model call, with usage and a conversation when asked.
fn call(conn: &Connection, time: i64, id: &str, usage: bool, conversation: bool) {
    conn.execute(
        "INSERT INTO proxy_requests (time_ms, tenant, request_id, api_family, upstream, status,
           stream, prompt_tokens, conversation, timing_json)
         VALUES (?1, '', ?2, 'anthropic_messages', 'a', 200, 1, ?3, ?4, '{}')",
        params![
            time,
            id,
            usage.then_some(10_i64),
            conversation.then_some("00000000000000aa")
        ],
    )
    .unwrap();
}

fn check<'a>(report: &'a QualityReport, id: &str) -> &'a Check {
    report.checks.iter().find(|c| c.id == id).unwrap()
}

fn runtime(now_ms: i64) -> QualityRuntime {
    QualityRuntime {
        now_ms,
        stats_dropped: 0,
        retention_days: 30,
    }
}

#[test]
fn coverage_is_judged_below_its_limits() {
    let (_dir, conn, path) = db();
    // 30 answered calls: 27 with usage (90 %, warn), 12 with a conversation
    // (40 %, bad).
    for i in 0..30_i64 {
        call(&conn, i * MIN, &format!("r{i}"), i >= 3, i < 12);
    }
    // A pass-through 304 is not an answered model call.
    conn.execute(
        "INSERT INTO proxy_requests (time_ms, tenant, request_id, api_family, upstream, status, stream)
         VALUES (1, '', 'p', 'passthrough', 'a', 304, 0)",
        [],
    )
    .unwrap();
    drop(conn);

    let report = StatsReader::open(&path)
        .unwrap()
        .quality("", 0, DAY, &runtime(DAY))
        .unwrap();
    let usage = check(&report, "usage");
    assert_eq!(usage.value, Some(0.9));
    assert!(usage.below);
    assert_eq!(usage.level, Level::Warn);
    assert_eq!(check(&report, "conversation").level, Level::Bad);
    assert_eq!(report.level, Level::Bad);
    // No run context was filed for any row.
    assert_eq!(check(&report, "context").value, Some(0.0));
    // Never written by this build: reported, not judged.
    let shadow = check(&report, "shadow");
    assert_eq!(shadow.level, Level::Info);
    assert!(shadow.detail.contains("does not measure"));
    // Too few engine runs or retrievals to judge.
    assert_eq!(check(&report, "outcome").level, Level::Info);
    let proxy = report
        .tables
        .iter()
        .find(|t| t.table == "proxy_requests")
        .unwrap();
    assert_eq!(proxy.rows, 31);
    assert!(!proxy.over_limit);
    assert_eq!(report.raw_from_ms, Some(0));
    assert_eq!(report.archived_through, None);
}

#[test]
fn duplicate_runs_retention_and_drops_are_flagged() {
    let (_dir, conn, path) = db();
    for i in 0..25_i64 {
        // Every fifth request id is filed twice.
        let id = format!("r{}", if i % 5 == 0 { i + 1 } else { i });
        conn.execute(
            "INSERT INTO optimization_events (time_ms, tenant, request_id, surface, mode, tokens_in,
               tokens_out, chars_in, chars_out, messages_total, messages_compressed,
               messages_skipped, refs_emitted, ref_bytes_emitted, content_types,
               protected_by_reason, engine_ms, outcome, skipped_tokens_by_reason)
             VALUES (?1, '', ?2, 'proxy', 'recoverable', 1, 1, 0, 0, 1, 0, 1, 0, 0, '{}', '{}', 1,
               'compressed', '{}')",
            params![40 * DAY + i * MIN, id],
        )
        .unwrap();
    }
    drop(conn);

    let rt = QualityRuntime {
        stats_dropped: 7,
        ..runtime(41 * DAY)
    };
    let report = StatsReader::open(&path)
        .unwrap()
        .quality("", 0, 41 * DAY, &rt)
        .unwrap();
    assert_eq!(check(&report, "outcome").level, Level::Ok);
    assert_eq!(check(&report, "duplicate_runs").value, Some(5.0 / 25.0));
    assert_eq!(check(&report, "duplicate_runs").level, Level::Bad);
    assert_eq!(check(&report, "stats_dropped").level, Level::Warn);
    // The window reaches 41 days back; raw rows are kept 30.
    assert_eq!(check(&report, "retention").level, Level::Warn);
}

#[test]
fn an_empty_database_is_ok_with_nothing_judged() {
    let (_dir, _conn, path) = db();
    let report = StatsReader::open(&path)
        .unwrap()
        .quality("", 0, DAY, &runtime(DAY))
        .unwrap();
    assert_eq!(report.level, Level::Ok);
    assert_eq!(report.raw_from_ms, None);
    assert!(report.tables.iter().all(|t| t.rows == 0));
}
