//! Traffic classes and outcomes, by rule and against a real database.

use rusqlite::{Connection, params};
use tempfile::TempDir;

use super::*;
use crate::schema;

const MIN: i64 = 60_000;

fn row(
    conn: &Connection,
    time: i64,
    api_family: &str,
    keep_warm: bool,
    status: Option<i64>,
    error: Option<&str>,
) {
    conn.execute(
        "INSERT INTO proxy_requests (time_ms, tenant, request_id, api_family, upstream, status,
           stream, keep_warm, error, prompt_tokens)
         VALUES (?1, '', ?2, ?3, 'https://api.anthropic.com', ?4, 1, ?5, ?6, 10)",
        params![
            time,
            format!("r{time}"),
            api_family,
            status,
            keep_warm as i64,
            error
        ],
    )
    .unwrap();
}

#[test]
fn classes_and_outcomes_follow_the_rules() {
    assert_eq!(class_of("anthropic_messages", false, Some(200)), "model");
    assert_eq!(class_of("anthropic_messages", true, Some(200)), "keep_warm");
    assert_eq!(class_of("openai_responses", false, Some(101)), "websocket");
    assert_eq!(class_of("passthrough", false, Some(304)), "passthrough");

    assert_eq!(outcome_of(Some(200), None), "ok");
    assert_eq!(outcome_of(Some(304), None), "not_modified");
    assert_eq!(outcome_of(Some(101), None), "switching");
    assert_eq!(outcome_of(Some(404), None), "refused");
    assert_eq!(outcome_of(Some(429), None), "rate_limited");
    assert_eq!(outcome_of(Some(503), None), "server_error");
    assert_eq!(outcome_of(None, Some("connect failed")), "no_answer");
    assert_eq!(
        outcome_of(
            Some(200),
            Some("client disconnected before the stream completed")
        ),
        "client_disconnect"
    );
    assert_eq!(outcome_of(Some(200), Some("stream reset")), "broken");

    assert_eq!(kind_of("not_modified"), "expected");
    assert_eq!(kind_of("client_disconnect"), "client");
    assert_eq!(kind_of("broken"), "upstream");
}

#[test]
fn noise_is_kept_apart_from_real_failures() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("stats.db");
    let conn = schema::open_read_write(&path).unwrap();
    for i in 0..6 {
        row(&conn, i * MIN, "anthropic_messages", false, Some(200), None);
    }
    row(&conn, 6 * MIN, "anthropic_messages", false, Some(529), None);
    row(&conn, 7 * MIN, "anthropic_messages", true, Some(200), None);
    for i in 0..5 {
        row(&conn, 8 * MIN + i, "passthrough", false, Some(304), None);
    }
    row(&conn, 9 * MIN, "passthrough", false, Some(404), None);
    row(&conn, 10 * MIN, "openai_responses", false, Some(101), None);
    row(
        &conn,
        11 * MIN,
        "anthropic_messages",
        false,
        Some(200),
        Some("client disconnected before the stream completed"),
    );
    row(
        &conn,
        12 * MIN,
        "passthrough",
        false,
        None,
        Some("error sending request 123"),
    );
    row(
        &conn,
        13 * MIN,
        "passthrough",
        false,
        None,
        Some("error sending request 456"),
    );
    drop(conn);

    let report = StatsReader::open(&path)
        .unwrap()
        .traffic("", 0, 60 * MIN)
        .unwrap();
    assert_eq!(report.requests, 18);
    let class = |name: &str| report.classes.iter().find(|c| c.class == name).unwrap();
    assert_eq!(class("model").requests, 8);
    assert_eq!(class("keep_warm").requests, 1);
    assert_eq!(class("websocket").requests, 1);
    assert_eq!(class("passthrough").requests, 8);
    assert_eq!(class("passthrough").outcomes["not_modified"], 5);
    assert_eq!(class("model").prompt_tokens, 80);

    // 304s and the 101 are noise; a naive count calls them failures too.
    assert_eq!(report.noise, 6);
    assert_eq!(report.naive_failures, 6 + 1 + 1 + 1 + 2);
    // Only the 529 among the nine model calls and pings is the upstream's.
    assert_eq!(report.real_failures, 1);
    assert_eq!(report.real_error_share, Some(1.0 / 9.0));
    assert_eq!(report.client_failures, 2, "the 404 and the disconnect");

    // Digits are folded: two request errors are one message.
    let sending = report
        .errors
        .iter()
        .find(|e| e.message.starts_with("error sending"))
        .unwrap();
    assert_eq!(
        (sending.requests, sending.outcome.as_str()),
        (2, "no_answer")
    );
    assert_eq!(report.endpoints[0].outcome, "ok");
    assert_eq!(report.endpoints[0].requests, 6);
    assert!(!report.timeline.is_empty());
}

#[test]
fn an_empty_window_has_no_shares() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("stats.db");
    drop(schema::open_read_write(&path).unwrap());
    let report = StatsReader::open(&path)
        .unwrap()
        .traffic("", 0, MIN)
        .unwrap();
    assert_eq!(report.requests, 0);
    assert_eq!(report.naive_error_share, None);
    assert_eq!(report.classes.len(), 4);
}
