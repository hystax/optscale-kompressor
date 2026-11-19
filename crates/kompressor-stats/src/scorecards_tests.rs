//! The models, activity and cohorts reports: the pure rules, and each report
//! against a real, migrated database.

use rusqlite::{Connection, params};
use tempfile::TempDir;

use super::*;
use crate::schema;

const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;
const OPUS: &str = "claude-opus-5-5";
const ANTHROPIC: &str = "anthropic_messages";

fn db() -> (TempDir, Connection, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("stats.db");
    let conn = schema::open_read_write(&path).unwrap();
    (dir, conn, path)
}

struct Req<'a> {
    time: i64,
    id: &'a str,
    family: &'a str,
    model: Option<&'a str>,
    conversation: Option<&'a str>,
    /// `(prompt, read, written)`.
    usage: (i64, i64, i64),
    keep_warm: bool,
    status: Option<i64>,
    error: Option<&'a str>,
    /// `(rounds, tokens put back)`.
    retrieval: (i64, i64),
}

impl Default for Req<'_> {
    fn default() -> Self {
        Req {
            time: 0,
            id: "r",
            family: ANTHROPIC,
            model: Some(OPUS),
            conversation: None,
            usage: (0, 0, 0),
            keep_warm: false,
            status: Some(200),
            error: None,
            retrieval: (0, 0),
        }
    }
}

fn proxy(conn: &Connection, r: Req<'_>) {
    conn.execute(
        "INSERT INTO proxy_requests (time_ms, tenant, request_id, api_family, model, upstream,
           status, stream, prompt_tokens, completion_tokens, cache_read_tokens,
           cache_write_tokens, conversation, keep_warm, error, retrieve_rounds,
           retrieve_tokens_injected)
         VALUES (?1, '', ?2, ?3, ?4, 'u', ?5, 1, ?6, 1, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            r.time,
            r.id,
            r.family,
            r.model,
            r.status,
            r.usage.0,
            r.usage.1,
            r.usage.2,
            r.conversation,
            r.keep_warm as i64,
            r.error,
            r.retrieval.0,
            r.retrieval.1
        ],
    )
    .unwrap();
}

#[allow(clippy::too_many_arguments)]
fn run(
    conn: &Connection,
    time: i64,
    id: &str,
    model: &str,
    tin: i64,
    tout: i64,
    engine_ms: f64,
    outcome: &str,
) {
    let compressed = i64::from(outcome == "compressed");
    conn.execute(
        "INSERT INTO optimization_events (time_ms, tenant, request_id, surface, api_family, model,
           mode, tokens_in, tokens_out, chars_in, chars_out, messages_total, messages_compressed,
           messages_skipped, refs_emitted, ref_bytes_emitted, content_types, protected_by_reason,
           engine_ms, outcome)
         VALUES (?1, '', ?2, 'proxy', ?3, ?4, 'recoverable', ?5, ?6, 0, 0, 4, ?7, 2, 0, 0, '{}',
           '{}', ?8, ?9)",
        params![
            time, id, ANTHROPIC, model, tin, tout, compressed, engine_ms, outcome
        ],
    )
    .unwrap();
}

// ------------------------------------------------------------------ rules

#[test]
fn turn_buckets_are_half_open_and_the_last_is_unbounded() {
    assert_eq!(turn_bucket(0), 0);
    assert_eq!(turn_bucket(1), 0);
    assert_eq!(turn_bucket(2), 1);
    assert_eq!(turn_bucket(4), 1);
    assert_eq!(turn_bucket(5), 2);
    assert_eq!(turn_bucket(19), 2);
    assert_eq!(turn_bucket(20), 3);
    assert_eq!(turn_bucket(99), 3);
    assert_eq!(turn_bucket(100), 4);
    assert_eq!(turn_bucket(499), 4);
    assert_eq!(turn_bucket(500), 5);
    assert_eq!(turn_bucket(u64::MAX), 5);
}

#[test]
fn the_peak_window_wraps_past_midnight_and_an_empty_day_has_none() {
    assert_eq!(peak_window(&[0; 24], 8), None);

    let mut day = [0u64; 24];
    day[22] = 5;
    day[23] = 5;
    day[0] = 5;
    day[1] = 5;
    day[12] = 4;
    // Two hours wide the best window is one of three equal ones (22, 23, 0):
    // the earliest start wins.
    assert_eq!(peak_window(&day, 2), Some((0, 10.0 / 24.0)));
    // Four hours wide only the window that wraps holds all twenty.
    assert_eq!(peak_window(&day, 4), Some((22, 20.0 / 24.0)));
    // A window wider than the day is the day.
    assert_eq!(peak_window(&day, 99), Some((0, 1.0)));
    assert_eq!(peak_window(&day, 0), None);
}

fn conversation(turns: u64, cost: f64, saved_net: i64, span: i64) -> ConversationRow {
    ConversationRow {
        conversation: format!("c{turns}-{cost}"),
        first_ms: 1_000,
        last_ms: 1_000 + span,
        client_turns: turns,
        saved_gross: saved_net,
        saved_net,
        billed: Billed {
            input: 1_000,
            uncached: 1_000,
            cost,
            ..Billed::default()
        },
        ..ConversationRow::default()
    }
}

#[test]
fn cohorts_bucket_by_turns_and_say_how_few_conversations_carry_half_the_cost() {
    let rows = vec![
        conversation(1, 1.0, 0, 10),
        conversation(1, 1.0, 0, 30),
        conversation(1, 1.0, 0, 20),
        conversation(1, 1.0, 0, 40),
        conversation(3, 6.0, 100, 500),
        conversation(700, 90.0, 400, 9_000),
    ];
    let (cohorts, half) = cohorts_of(&rows);
    assert_eq!(cohorts.len(), TURN_BUCKETS.len());
    let by = |label: &str| cohorts.iter().find(|c| c.turns == label).unwrap();

    assert_eq!(by("1").conversations, 4);
    assert_eq!(by("1").client_turns, 4);
    // Lower median of 10, 20, 30, 40.
    assert_eq!(by("1").median_duration_ms, Some(20));
    assert_eq!(by("1").cost_share, Some(0.04));
    assert_eq!(by("2–4").conversations, 1);
    assert_eq!(by("2–4").net_saving_share, Some(0.1));
    assert_eq!(by("500+").conversations, 1);
    assert_eq!(by("500+").cost_share, Some(0.9));
    // A cohort nobody is in is listed, empty, so the page's rows do not move.
    assert_eq!(by("20–99").conversations, 0);
    assert_eq!(by("20–99").median_duration_ms, None);
    assert_eq!(by("20–99").net_saving_share, None);

    // One conversation of six carries 90% of the cost.
    assert_eq!(half, Some(1.0 / 6.0));
}

#[test]
fn cohorts_of_nothing_are_empty_rather_than_a_division_by_zero() {
    let (cohorts, half) = cohorts_of(&[]);
    assert!(
        cohorts
            .iter()
            .all(|c| c.conversations == 0 && c.cost_share.is_none())
    );
    assert_eq!(half, None);

    // Conversations that cost nothing (no usage reported) have no half.
    let (cohorts, half) = cohorts_of(&[conversation(2, 0.0, 0, 0)]);
    assert_eq!(cohorts[1].conversations, 1);
    assert_eq!(half, None);
}

// ---------------------------------------------------------------- reports

#[test]
fn a_model_card_joins_billing_engine_retrieval_and_failures() {
    let (_dir, conn, path) = db();
    // Opus: two client calls, one ping, one 529; a retrieval round put 40 back.
    proxy(
        &conn,
        Req {
            time: 10,
            id: "a",
            usage: (100, 800, 100),
            retrieval: (1, 40),
            ..Req::default()
        },
    );
    proxy(
        &conn,
        Req {
            time: 20,
            id: "b",
            usage: (50, 900, 0),
            ..Req::default()
        },
    );
    proxy(
        &conn,
        Req {
            time: 30,
            id: "p",
            usage: (0, 1_000, 0),
            keep_warm: true,
            ..Req::default()
        },
    );
    proxy(
        &conn,
        Req {
            time: 40,
            id: "e",
            status: Some(529),
            ..Req::default()
        },
    );
    // A client that hung up is not the upstream's failure.
    proxy(
        &conn,
        Req {
            time: 50,
            id: "d",
            error: Some("client disconnected before the stream completed"),
            ..Req::default()
        },
    );
    // Pass-through has no model and must not become a card.
    proxy(
        &conn,
        Req {
            time: 60,
            id: "x",
            family: "passthrough",
            model: None,
            status: Some(304),
            ..Req::default()
        },
    );
    // Another tenant's and another window's rows are not counted.
    conn.execute(
        "INSERT INTO proxy_requests (time_ms, tenant, request_id, api_family, model, upstream, stream)
         VALUES (15, 'other', 'o', ?1, ?2, 'u', 1)",
        params![ANTHROPIC, OPUS],
    )
    .unwrap();
    proxy(
        &conn,
        Req {
            time: 5_000,
            id: "late",
            ..Req::default()
        },
    );

    run(&conn, 10, "a", OPUS, 1_000, 700, 10.0, "compressed");
    run(
        &conn,
        20,
        "b",
        OPUS,
        1_000,
        1_000,
        30.0,
        "no_compressible_content",
    );
    // A discarded rewrite is a run and a latency sample, but earns no credit.
    run(&conn, 25, "c", OPUS, 500, 100, 20.0, "discarded");
    run(
        &conn,
        26,
        "h",
        "claude-haiku-5-5",
        200,
        200,
        2.0,
        "no_compressible_content",
    );
    drop(conn);

    let report = StatsReader::open(&path)
        .unwrap()
        .models("", 0, 1_000)
        .unwrap();
    assert!(!report.truncated);
    assert_eq!(report.models.len(), 2, "{:?}", report.models);
    let opus = &report.models[0];
    assert_eq!(
        (opus.api_family.as_str(), opus.model.as_str()),
        (ANTHROPIC, OPUS)
    );
    assert_eq!((opus.client_requests, opus.pings), (4, 1));
    assert_eq!(opus.upstream_failures, 1);
    assert_eq!(opus.upstream_failure_share, Some(0.2));
    assert_eq!(opus.billed.input, 150 + 2_700 + 100);
    assert_eq!(opus.billed.read, 2_700);
    assert_eq!((opus.engine_runs, opus.compressed_runs), (3, 1));
    assert_eq!(
        (opus.tokens_in, opus.tokens_out, opus.saved_gross),
        (2_000, 1_700, 300)
    );
    assert_eq!(opus.reduction, Some(0.15));
    assert_eq!(opus.engine_ms.samples, 3);
    assert_eq!(opus.engine_ms.p50, Some(20.0));
    assert_eq!(opus.engine_ms.max, Some(30.0));
    // The runs that changed nothing still cost their time.
    assert_eq!(opus.idle_engine_ms, 50.0);
    assert_eq!(
        (opus.retrieve_rounds, opus.retrieved_tokens, opus.saved_net),
        (1, 40, 260)
    );

    // A model the engine saw but the proxy billed nothing for still has a card.
    let haiku = &report.models[1];
    assert_eq!(haiku.model, "claude-haiku-5-5");
    assert_eq!((haiku.client_requests, haiku.engine_runs), (0, 1));
    assert_eq!(haiku.reduction, Some(0.0));
    assert_eq!(haiku.upstream_failure_share, None);
}

#[test]
fn models_of_an_empty_window_is_an_empty_list() {
    let (_dir, conn, path) = db();
    drop(conn);
    let report = StatsReader::open(&path)
        .unwrap()
        .models("", 0, 1_000)
        .unwrap();
    assert!(report.models.is_empty());
}

#[test]
fn activity_folds_the_window_onto_weekday_and_hour_in_utc() {
    let (_dir, conn, path) = db();
    // 1970-01-01 was a Thursday: weekday 4.
    let thu10 = 10 * HOUR;
    let thu10_next_week = thu10 + 7 * DAY;
    let fri23 = DAY + 23 * HOUR;
    proxy(
        &conn,
        Req {
            time: thu10,
            id: "a",
            usage: (10, 80, 10),
            ..Req::default()
        },
    );
    proxy(
        &conn,
        Req {
            time: thu10 + 1,
            id: "b",
            usage: (5, 95, 0),
            ..Req::default()
        },
    );
    proxy(
        &conn,
        Req {
            time: thu10_next_week,
            id: "c",
            usage: (1, 0, 99),
            ..Req::default()
        },
    );
    proxy(
        &conn,
        Req {
            time: thu10 + 2,
            id: "p",
            usage: (0, 100, 0),
            keep_warm: true,
            ..Req::default()
        },
    );
    proxy(
        &conn,
        Req {
            time: thu10 + 3,
            id: "x",
            family: "passthrough",
            model: None,
            ..Req::default()
        },
    );
    // OpenAI's prompt already contains its cached part: 100 in, not 160.
    proxy(
        &conn,
        Req {
            time: fri23,
            id: "o",
            family: "openai_responses",
            model: Some("gpt-6"),
            usage: (100, 60, 0),
            ..Req::default()
        },
    );
    run(&conn, thu10, "a", OPUS, 1_000, 600, 1.0, "compressed");
    run(&conn, thu10, "z", OPUS, 1_000, 0, 1.0, "discarded");
    drop(conn);

    let report = StatsReader::open(&path)
        .unwrap()
        .activity("", 0, 30 * DAY)
        .unwrap();
    assert_eq!(report.cells.len(), 2);
    let thu = &report.cells[0];
    assert_eq!((thu.weekday, thu.hour), (4, 10));
    assert_eq!((thu.requests, thu.pings, thu.passthrough), (3, 1, 1));
    assert_eq!(thu.input_tokens, 100 + 100 + 100 + 100);
    assert_eq!(thu.cache_write_tokens, 109);
    assert_eq!(thu.saved_gross, 400);
    assert_eq!(thu.days, 2);
    let fri = &report.cells[1];
    assert_eq!((fri.weekday, fri.hour, fri.requests), (5, 23, 1));
    assert_eq!(fri.input_tokens, 100);
    assert_eq!(fri.cache_write_tokens, 0);

    assert_eq!(report.requests, 4);
    assert_eq!(
        report.busiest.as_ref().map(|c| (c.weekday, c.hour)),
        Some((4, 10))
    );
    // Hours 10 and 23 are thirteen apart: no eight-hour window holds both.
    assert_eq!(report.peak_window_start_hour, Some(3));
    assert_eq!(report.peak_window_share, Some(0.75));
}

#[test]
fn activity_of_an_empty_window_has_no_peak() {
    let (_dir, conn, path) = db();
    drop(conn);
    let report = StatsReader::open(&path)
        .unwrap()
        .activity("", 0, DAY)
        .unwrap();
    assert!(report.cells.is_empty());
    assert_eq!(report.busiest, None);
    assert_eq!(report.peak_window_share, None);
}

#[test]
fn cohorts_read_conversations_from_the_database() {
    let (_dir, conn, path) = db();
    for turn in 0..6 {
        let id = format!("long-{turn}");
        proxy(
            &conn,
            Req {
                time: 100 + turn * 10,
                id: &id,
                conversation: Some("aaaaaaaaaaaaaaaa"),
                usage: (10, 900, 90),
                ..Req::default()
            },
        );
    }
    proxy(
        &conn,
        Req {
            time: 400,
            id: "short",
            conversation: Some("bbbbbbbbbbbbbbbb"),
            usage: (100, 0, 0),
            ..Req::default()
        },
    );
    // No conversation: not in any cohort.
    proxy(
        &conn,
        Req {
            time: 410,
            id: "none",
            usage: (7, 0, 0),
            ..Req::default()
        },
    );
    run(&conn, 100, "long-0", OPUS, 1_000, 400, 1.0, "compressed");
    drop(conn);

    let report = StatsReader::open(&path)
        .unwrap()
        .cohorts("", 0, 1_000)
        .unwrap();
    assert_eq!(report.conversations, 2);
    assert!(!report.truncated);
    let by = |label: &str| report.cohorts.iter().find(|c| c.turns == label).unwrap();
    assert_eq!((by("1").conversations, by("1").client_turns), (1, 1));
    assert_eq!((by("5–19").conversations, by("5–19").client_turns), (1, 6));
    assert_eq!(by("5–19").billed.input, 6_000);
    assert_eq!(by("5–19").saved_gross, 600);
    assert_eq!(by("5–19").net_saving_share, Some(0.1));
    assert_eq!(by("5–19").median_duration_ms, Some(50));
    assert_eq!(report.half_cost_conversation_share, Some(0.5));
}
