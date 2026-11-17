//! Cache losses set against compression, on a real, migrated database.

use rusqlite::{Connection, params};
use tempfile::TempDir;

use super::*;
use crate::schema;

const MIN: i64 = 60_000;

fn db() -> (TempDir, Connection, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("stats.db");
    let conn = schema::open_read_write(&path).unwrap();
    (dir, conn, path)
}

/// A client request with Anthropic-style usage `(read, written)` and five
/// fresh tokens, filed under `conversation`.
fn turn(conn: &Connection, time: i64, id: &str, conversation: &str, read: i64, write: i64) {
    conn.execute(
        "INSERT INTO proxy_requests (time_ms, tenant, request_id, api_family, model, upstream,
           status, stream, prompt_tokens, completion_tokens, cache_read_tokens,
           cache_write_tokens, conversation, total_ms)
         VALUES (?1, '', ?2, 'anthropic_messages', 'claude-opus-5-5', 'anthropic', 200, 1, 5, 1,
           ?3, ?4, ?5, 1000)",
        params![time, id, read, write, conversation],
    )
    .unwrap();
}

/// The engine run of request `id`: `compressed` messages rewritten, `frozen`
/// frozen, `removed` tokens taken out.
fn engine(conn: &Connection, time: i64, id: &str, compressed: i64, frozen: i64, removed: i64) {
    conn.execute(
        "INSERT INTO optimization_events (time_ms, tenant, request_id, surface, mode, tokens_in,
           tokens_out, chars_in, chars_out, messages_total, messages_compressed, messages_skipped,
           refs_emitted, ref_bytes_emitted, content_types, protected_by_reason, engine_ms, outcome,
           frozen_messages)
         VALUES (?1, '', ?2, 'proxy', 'recoverable', ?3, ?4, 0, 0, 10, ?5, 0, 0, 0, '{}', '{}', 1,
           'compressed', ?6)",
        params![time - 100, id, 10_000 + removed, 10_000, compressed, frozen],
    )
    .unwrap();
}

#[test]
fn losses_are_split_by_what_the_engine_did_and_ttl_losses_are_left_out() {
    let (_dir, conn, path) = db();
    let c = "00000000000000aa";
    turn(&conn, 0, "t0", c, 0, 30_000); // first
    engine(&conn, 0, "t0", 0, 2, 0);
    turn(&conn, MIN, "t1", c, 30_000, 1_000); // hit, compressed
    engine(&conn, MIN, "t1", 1, 2, 500);
    turn(&conn, 2 * MIN, "t2", c, 0, 31_000); // miss, compressed, freeze lifted
    engine(&conn, 2 * MIN, "t2", 2, 0, 700);
    turn(&conn, 3 * MIN, "t3", c, 31_000, 0); // hit, no engine
    turn(&conn, 4 * MIN, "t4", c, 0, 31_000); // miss, no engine
    turn(&conn, 30 * MIN, "t5", c, 0, 31_000); // TTL loss: left out
    turn(&conn, 31 * MIN, "u", "", 0, 0); // a different conversation's first row
    drop(conn);

    let report = StatsReader::open(&path)
        .unwrap()
        .compression_cache("", 0, 60 * MIN, 10)
        .unwrap();
    let [compressed, unchanged, no_engine] = &report.groups[..] else {
        panic!("three groups");
    };
    assert_eq!((compressed.turns, compressed.losses), (2, 1));
    assert_eq!(compressed.lost_tokens, 31_000);
    assert_eq!(compressed.removed_tokens, 1_200);
    assert_eq!(
        (unchanged.turns, unchanged.losses),
        (0, 0),
        "a first row is compared with nothing"
    );
    assert_eq!((no_engine.turns, no_engine.losses), (2, 1));
    assert_eq!(report.ttl_losses, 1);
    assert_eq!(report.baseline_rate, Some(0.5));
    assert_eq!(report.with_compression.events, 1.0);
    // One loss in two compressed turns is the baseline rate: no excess.
    assert_eq!(report.excess.events, 0.0);
    assert_eq!(report.freeze_lifted_turns, 1);
    assert_eq!(report.likely.events, 1.0);
    assert!(report.likely.cost > 0.0);
    assert_eq!(report.losses.len(), 1);
    let loss = &report.losses[0];
    assert_eq!((loss.request_id.as_str(), loss.freeze_lifted), ("t2", true));
    assert_eq!((loss.frozen, loss.previous_frozen), (Some(0), Some(2)));
    assert_eq!(report.conversations.len(), 1);
    assert_eq!(report.conversations[0].compressed_turns, 2);
    assert!(report.removed_cost_floor > 0.0);
}

#[test]
fn losses_above_the_baseline_are_the_excess() {
    let (_dir, conn, path) = db();
    // A conversation whose compressed turns lose every other prefix…
    let a = "00000000000000aa";
    turn(&conn, 0, "a0", a, 0, 10_000);
    for i in 1..=4_i64 {
        let id = format!("a{i}");
        let lost = i % 2 == 0;
        turn(
            &conn,
            i * MIN,
            &id,
            a,
            if lost { 0 } else { 10_000 },
            if lost { 10_000 } else { 0 },
        );
        engine(&conn, i * MIN, &id, 1, 3, 100);
    }
    // …and one the engine never touched that keeps every prefix.
    let b = "00000000000000bb";
    turn(&conn, 0, "b0", b, 0, 10_000);
    for i in 1..=4_i64 {
        turn(&conn, i * MIN, &format!("b{i}"), b, 10_000, 0);
    }
    drop(conn);

    let report = StatsReader::open(&path)
        .unwrap()
        .compression_cache("", 0, 60 * MIN, 10)
        .unwrap();
    assert_eq!(report.baseline_rate, Some(0.0));
    assert_eq!(report.with_compression.events, 2.0);
    assert_eq!(report.excess.events, 2.0);
    assert_eq!(report.excess.cost, report.with_compression.cost);
    // The freeze held at three messages: nothing was lifted.
    assert_eq!(report.likely.events, 0.0);
    assert_eq!(report.losses.len(), 2);
    assert_eq!(report.losses[0].request_id, "a4", "newest first");
}

#[test]
fn an_empty_window_is_an_empty_report() {
    let (_dir, _conn, path) = db();
    let report = StatsReader::open(&path)
        .unwrap()
        .compression_cache("", 0, MIN, 10)
        .unwrap();
    assert_eq!(report.rows, 0);
    assert_eq!(report.baseline_rate, None);
    assert!(
        report
            .groups
            .iter()
            .all(|g| g.turns == 0 && g.loss_rate.is_none())
    );
}
