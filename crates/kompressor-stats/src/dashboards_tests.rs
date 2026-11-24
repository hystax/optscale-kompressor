//! The dashboard reports against a real, migrated database.

use rusqlite::{Connection, params};
use tempfile::TempDir;

use super::*;
use crate::schema;

const MIN: i64 = 60_000;
const OPUS: &str = "claude-opus-5-5";
const ANTHROPIC: &str = "anthropic_messages";

fn db() -> (TempDir, Connection, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("stats.db");
    let conn = schema::open_read_write(&path).unwrap();
    (dir, conn, path)
}

/// One relayed request or ping with Anthropic-style usage:
/// `(fresh, read, written)`.
fn proxy(
    conn: &Connection,
    time: i64,
    conversation: &str,
    usage: (i64, i64, i64),
    keep_warm: bool,
) {
    let (fresh, read, write) = usage;
    conn.execute(
        "INSERT INTO proxy_requests (time_ms, tenant, request_id, api_family, model, upstream,
           status, stream, prompt_tokens, completion_tokens, cache_read_tokens,
           cache_write_tokens, conversation, keep_warm)
         VALUES (?1, '', ?2, ?3, ?4, 'anthropic', 200, 1, ?5, 1, ?6, ?7, ?8, ?9)",
        params![
            time,
            format!("{conversation}-{time}"),
            ANTHROPIC,
            OPUS,
            fresh,
            read,
            write,
            conversation,
            keep_warm as i64
        ],
    )
    .unwrap();
}

/// A failed or successful upstream request, for the upstream report.
fn answered(
    conn: &Connection,
    time: i64,
    upstream: &str,
    status: Option<i64>,
    ms: f64,
    timing: &str,
) {
    conn.execute(
        "INSERT INTO proxy_requests (time_ms, tenant, request_id, api_family, model, upstream,
           status, stream, upstream_ms, total_ms, timing_json)
         VALUES (?1, '', ?2, 'anthropic_messages', ?3, ?4, ?5, 1, ?6, ?6, ?7)",
        params![time, format!("u{time}"), OPUS, upstream, status, ms, timing],
    )
    .unwrap();
}

#[allow(clippy::too_many_arguments)]
fn run(
    conn: &Connection,
    time: i64,
    id: &str,
    tin: i64,
    tout: i64,
    types: &str,
    engine_ms: f64,
    refs: (i64, i64),
) {
    conn.execute(
        "INSERT INTO optimization_events (time_ms, tenant, request_id, surface, mode, tokens_in,
           tokens_out, chars_in, chars_out, messages_total, messages_compressed, messages_skipped,
           refs_emitted, ref_bytes_emitted, content_types, protected_by_reason, engine_ms, outcome)
         VALUES (?1, '', ?2, 'proxy', 'recoverable', ?3, ?4, 0, 0, 4, 2, 2, ?5, ?6, ?7, '{}', ?8,
           'compressed')",
        params![time, id, tin, tout, refs.0, refs.1, types, engine_ms],
    )
    .unwrap();
}

fn ask(conn: &Connection, time: i64, handle: &str, found: bool, expired: bool, tokens: i64) {
    conn.execute(
        "INSERT INTO retrieval_events (time_ms, tenant, request_id, surface, ref, found, expired,
           fragment_bytes, fragment_chars, fragment_tokens, returned_chars, returned_tokens)
         VALUES (?1, '', 'r', 'proxy', ?2, ?3, ?4, 0, 0, 0, 0, ?5)",
        params![time, handle, found as i64, expired as i64, tokens],
    )
    .unwrap();
}

fn stored(id: &str, kind: &str, bytes: u64, created_at: u64, last_accessed: u64) -> StoredRef {
    StoredRef {
        id: id.into(),
        kind: kind.into(),
        bytes,
        created_at,
        last_accessed,
        expires_at: last_accessed + 3_600,
        expired: false,
    }
}

#[test]
fn keep_warm_prices_bridged_pauses_against_their_pings() {
    let (_dir, conn, path) = db();
    // Bridged: two pings carry a nine-minute pause past the five-minute TTL.
    proxy(&conn, 0, "c1", (100, 0, 10_000), false);
    proxy(&conn, 4 * MIN, "c1", (0, 10_000, 0), true);
    proxy(&conn, 8 * MIN, "c1", (0, 10_000, 0), true);
    proxy(&conn, 9 * MIN, "c1", (100, 10_000, 100), false);
    // Unpinged: a twenty-minute pause loses the prefix to the TTL.
    proxy(&conn, 0, "c2", (100, 0, 10_000), false);
    proxy(&conn, 20 * MIN, "c2", (100, 0, 10_000), false);
    // Trailing: a ping after the last turn bridges nothing anyone used.
    proxy(&conn, 0, "c3", (100, 0, 10_000), false);
    proxy(&conn, 4 * MIN, "c3", (0, 10_000, 0), true);
    drop(conn);

    let report = StatsReader::open(&path)
        .unwrap()
        .keep_warm("", 0, 60 * MIN, 10)
        .unwrap();
    // A warm ping reads 10,000 at 0.05; a rewrite costs 1.25 - 0.05 over a read.
    assert_eq!(
        (report.pings, report.warm, report.cold, report.failed),
        (3, 3, 0, 0)
    );
    assert!((report.ping_cost - 1_500.0).abs() < 1e-6);
    assert_eq!(
        (report.bridged, report.not_bridged, report.unneeded),
        (1, 0, 0)
    );
    assert!((report.avoided_cost - 12_000.0).abs() < 1e-6);
    assert_eq!(report.unpinged_expired, 1);
    assert!((report.missed_cost - 12_000.0).abs() < 1e-6);
    assert_eq!(report.trailing_pings, 1);
    assert!((report.trailing_cost - 500.0).abs() < 1e-6);
    assert!((report.net - 10_500.0).abs() < 1e-6);
    assert!((report.return_ratio.unwrap() - 8.0).abs() < 1e-9);

    let five_to_ten = report.gaps.iter().find(|g| g.label == "5-10m").unwrap();
    assert_eq!(
        (five_to_ten.pauses, five_to_ten.pinged, five_to_ten.bridged),
        (1, 1, 1)
    );
    assert!((five_to_ten.ping_cost - 1_000.0).abs() < 1e-6);
    let ten_to_thirty = report.gaps.iter().find(|g| g.label == "10-30m").unwrap();
    assert_eq!(
        (ten_to_thirty.pauses, ten_to_thirty.unpinged_expired),
        (1, 1)
    );

    let names: Vec<&str> = report
        .conversations
        .iter()
        .map(|c| c.conversation.as_str())
        .collect();
    assert_eq!(names, vec!["c1", "c3", "c2"], "most ping cost first");
    assert_eq!(report.conversations_total, 3);
    let c3 = &report.conversations[1];
    assert_eq!((c3.pings, c3.trailing_pings), (1, 1));
}

#[test]
fn trends_fold_raw_rows_into_whole_periods_and_mark_new_builds() {
    let (_dir, conn, path) = db();
    // Monday 2026-11-02 and Monday 2026-11-09, UTC.
    let week1 = days_from_civil(2026, 11, 2) * DAY_MS;
    let week2 = days_from_civil(2026, 11, 9) * DAY_MS;
    for (time, build) in [(week1 + 3_600_000, "2.0.10"), (week2 + 7_200_000, "2.0.11")] {
        proxy(&conn, time, "c", (1_000, 0, 0), false);
        conn.execute(
            "INSERT INTO request_context (tenant, request_id, time_ms, context, model, mode)
             VALUES ('', ?1, ?2, ?3, ?4, 'recoverable')",
            params![
                format!("c-{time}"),
                time,
                format!(
                    r#"{{"build_version":"{build}","config_fingerprint":"f","experiment_id":null,"run_id":"r","settings":{{}}}}"#
                ),
                OPUS
            ],
        )
        .unwrap();
    }
    drop(conn);

    let report = StatsReader::open(&path)
        .unwrap()
        .trends("", week1 + DAY_MS, week2 + 3 * DAY_MS, TrendBucket::Week)
        .unwrap();
    assert!(!report.truncated);
    assert_eq!(
        report.from_ms, week1,
        "the first period is widened to its Monday"
    );
    let labels: Vec<&str> = report.points.iter().map(|p| p.label.as_str()).collect();
    assert_eq!(labels, vec!["2026-11-02", "2026-11-09"]);
    assert_eq!(report.points[0].metrics.client_requests, 1);
    assert_eq!(report.points[1].metrics.client_requests, 1);
    assert_eq!(
        report.points[0].cache_saving,
        Some(0.0),
        "nothing was cached"
    );
    assert_eq!(report.totals.metrics.client_requests, 2);
    assert_eq!(report.totals.conversations, 1);
    let markers: Vec<(&str, &str)> = report
        .markers
        .iter()
        .map(|m| (m.build_version.as_str(), m.label.as_str()))
        .collect();
    assert_eq!(
        markers,
        vec![("2.0.10", "2026-11-02"), ("2.0.11", "2026-11-09")]
    );
}

#[test]
fn the_engine_report_groups_by_type_and_size_and_lists_the_slowest() {
    let (_dir, conn, path) = db();
    run(
        &conn,
        1,
        "a",
        1_000,
        400,
        r#"{"json":3,"text":1}"#,
        2.0,
        (1, 10),
    );
    run(&conn, 2, "b", 20_000, 5_000, r#"{"log":2}"#, 40.0, (2, 20));
    run(&conn, 3, "c", 2_000, 1_000, r#"{"json":1}"#, 4.0, (0, 0));
    drop(conn);

    let report = StatsReader::open(&path).unwrap().engine("", 0, 10).unwrap();
    assert_eq!(report.runs, 3);
    assert!((report.total_ms - 46.0).abs() < 1e-9);
    let names: Vec<&str> = report.by_type.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(names, vec!["log", "json"], "most engine time first");
    let json = &report.by_type[1];
    assert_eq!((json.runs, json.tokens_in, json.saved), (2, 3_000, 1_600));
    assert!((json.ms_per_1k_tokens.unwrap() - 2.0).abs() < 1e-9);
    let sizes: Vec<(&str, u64)> = report
        .by_size
        .iter()
        .map(|g| (g.name.as_str(), g.runs))
        .collect();
    assert_eq!(sizes, vec![("<4K", 2), ("16K-64K", 1)], "in size order");
    assert_eq!(report.slowest[0].request_id, "b");
    assert_eq!(report.slowest[0].content_type, "log");
    assert_eq!(report.latency.max, Some(40.0));
}

#[test]
fn the_upstream_report_splits_status_classes_and_finds_bursts() {
    let (_dir, conn, path) = db();
    let one = r#"{"attempts_total":1}"#;
    answered(&conn, 0, "anthropic", Some(200), 100.0, one);
    answered(
        &conn,
        1_000,
        "anthropic",
        Some(429),
        10.0,
        r#"{"attempts_total":3}"#,
    );
    answered(&conn, 2_000, "anthropic", Some(529), 10.0, one);
    answered(&conn, 3_000, "openai", None, 10.0, "not json");
    answered(&conn, 4_000, "anthropic", Some(404), 10.0, one);
    answered(&conn, 10 * MIN, "openai", Some(200), 300.0, one);
    drop(conn);

    let report = StatsReader::open(&path)
        .unwrap()
        .upstream_health("", 0, 60 * MIN)
        .unwrap();
    let t = &report.totals;
    assert_eq!(
        (
            t.requests,
            t.ok,
            t.rate_limited,
            t.server_error,
            t.no_status,
            t.failed,
            t.retried
        ),
        (6, 2, 1, 1, 1, 4, 1)
    );
    assert_eq!(t.client_error, 1);
    assert_eq!(report.error_share, Some(4.0 / 6.0));
    assert_eq!(report.bursts.len(), 1);
    assert_eq!(
        (
            report.bursts[0].failures,
            report.bursts[0].from_ms,
            report.bursts[0].to_ms
        ),
        (3, 1_000, 3_000),
        "the 404 is a failure but not part of the incident"
    );
    assert_eq!(report.failures.len(), 4);
    assert_eq!(report.failures[0].time_ms, 4_000, "newest failure first");
    assert_eq!(report.failures[3].attempts, 3);
    let groups: Vec<(&str, u64)> = report
        .by_upstream
        .iter()
        .map(|g| (g.upstream.as_str(), g.mix.requests))
        .collect();
    assert_eq!(groups, vec![("anthropic", 4), ("openai", 2)]);
}

#[test]
fn yield_splits_savings_by_message_share_and_joins_asks_to_the_store() {
    let (_dir, conn, path) = db();
    // 600 saved across 3 json + 1 text; 300 saved on log alone.
    run(
        &conn,
        1_000,
        "a",
        1_000,
        400,
        r#"{"json":3,"text":1}"#,
        1.0,
        (2, 0),
    );
    run(&conn, 2_000, "b", 500, 200, r#"{"log":2}"#, 1.0, (1, 0));
    // Two asks for one log handle (one without the marker), one for a handle
    // the store no longer has.
    ask(&conn, 61_000, "k_log1", true, false, 70);
    ask(&conn, 62_000, "log1", true, false, 30);
    ask(&conn, 63_000, "k_gone", false, true, 0);
    drop(conn);

    let store = vec![
        stored("log1", "log", 400, 1, 1),
        stored("json1", "json", 900, 1, 1),
        stored("old", "json", 50, 0, 0),
    ];
    let report = StatsReader::open(&path)
        .unwrap()
        .compression_yield("", 1_000, 100_000, &store, false)
        .unwrap();
    assert_eq!(
        (
            report.compressing_runs,
            report.saved_gross,
            report.retrieved_tokens
        ),
        (2, 900, 100)
    );
    let json = report
        .types
        .iter()
        .find(|t| t.content_type == "json")
        .unwrap();
    assert!((json.saved_estimate - 450.0).abs() < 1e-9);
    assert_eq!((json.messages, json.single_type_requests), (3, 0));
    let log = report
        .types
        .iter()
        .find(|t| t.content_type == "log")
        .unwrap();
    assert_eq!((log.single_type_requests, log.single_type_saved), (1, 300));

    let log = report.kinds.iter().find(|k| k.kind == "log").unwrap();
    assert_eq!(
        (
            log.refs_stored,
            log.refs_asked,
            log.asks,
            log.returned_tokens
        ),
        (1, 1, 2, 100)
    );
    assert_eq!(log.asked_share, Some(1.0));
    assert_eq!(log.median_first_ask_s, Some(60));
    let json = report.kinds.iter().find(|k| k.kind == "json").unwrap();
    assert_eq!(
        (json.refs_stored, json.refs_asked),
        (1, 0),
        "stored before the window: not counted"
    );
    assert_eq!(
        (report.unknown_kind.refs_asked, report.unknown_kind.asks),
        (1, 1)
    );
}

#[test]
fn store_health_reads_the_census_and_the_retrieval_record() {
    let (_dir, conn, path) = db();
    run(
        &conn,
        1_000,
        "a",
        1_000,
        400,
        r#"{"json":1}"#,
        1.0,
        (4, 4_000),
    );
    ask(&conn, 2_000, "k_a", true, false, 10);
    ask(&conn, 3_000, "k_a", true, false, 10);
    ask(&conn, 4_000, "k_b", false, true, 0);
    ask(&conn, 5_000, "k_c", false, false, 0);
    conn.execute(
        "INSERT INTO retrieval_events (time_ms, tenant, request_id, surface, ref, found, expired,
           fragment_bytes, fragment_chars, fragment_tokens, returned_chars, returned_tokens, withheld)
         VALUES (6000, '', 'r', 'proxy', 'k_d', 0, 0, 0, 0, 0, 0, 0, 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO store_samples (time_ms, live_refs, live_bytes) VALUES (1500, 3, 300), (99999999, 9, 9)",
        [],
    )
    .unwrap();
    drop(conn);

    let now = 10_000u64;
    let mut expired = stored("x", "log", 70, 100, 100);
    expired.expired = true;
    let store = vec![
        stored("a", "json", 100, now - 60, now - 60),
        stored("b", "json", 200, now - 4_000, now - 10),
        expired,
    ];
    let report = StatsReader::open(&path)
        .unwrap()
        .store_health("", 0, 10_000, &store, false, now)
        .unwrap();
    assert_eq!((report.live_refs, report.live_bytes), (2, 300));
    assert_eq!((report.expired_refs, report.expired_bytes), (1, 70));
    assert_eq!(report.never_read_share, Some(0.5));
    assert_eq!(report.by_kind[0].kind, "json");
    let under_5m = &report.age[0];
    assert_eq!((under_5m.label.as_str(), under_5m.refs), ("<5m", 1));
    assert_eq!(
        report
            .age
            .iter()
            .find(|b| b.label == "30m-2h")
            .unwrap()
            .refs,
        1
    );
    assert_eq!(
        report.samples.len(),
        1,
        "samples outside the window are left out"
    );
    assert_eq!((report.minted_refs, report.minted_bytes), (4, 4_000));
    assert_eq!(
        (
            report.asks,
            report.hits,
            report.expired_asks,
            report.missing_asks,
            report.withheld_asks
        ),
        (5, 2, 1, 1, 1),
        "a withheld ask is not a missing entry"
    );
    assert_eq!(report.asked_refs, 4);
    assert_eq!(report.asked_share, Some(1.0));
}
