//! The prefix report, against a real database.

use rusqlite::{Connection, params};
use tempfile::TempDir;

use crate::context::{
    DRIFT_HISTORY, DRIFT_SYSTEM, DRIFT_TOOL_DEFS, DRIFT_TOOL_SET, PrefixObservation, Volatile,
};
use crate::prefix::{ColdScore, PrefixReport};
use crate::{StatsReader, schema};

const SEC: i64 = 1_000;

fn database() -> (TempDir, Connection) {
    let dir = TempDir::new().unwrap();
    let conn = schema::open_read_write(&dir.path().join("stats.db")).unwrap();
    (dir, conn)
}

fn report(dir: &TempDir) -> PrefixReport {
    StatsReader::open(dir.path().join("stats.db"))
        .unwrap()
        .prefix("", 0, i64::MAX)
        .unwrap()
}

struct Row<'a> {
    time: i64,
    conversation: &'a str,
    family: &'a str,
    read: i64,
    write: i64,
    observation: Option<PrefixObservation>,
}

fn insert(conn: &Connection, row: Row<'_>) {
    conn.execute(
        "INSERT INTO proxy_requests (time_ms, tenant, request_id, api_family, model, upstream,
           status, stream, prompt_tokens, cache_read_tokens, cache_write_tokens, conversation,
           total_ms, prefix_json)
         VALUES (?1, '', ?2, ?3, 'm', 'u', 200, 1, 10, ?4, ?5, ?6, 100.0, ?7)",
        params![
            row.time,
            format!("{}-{}", row.conversation, row.time),
            row.family,
            row.read,
            row.write,
            row.conversation,
            row.observation
                .map(|seen| serde_json::to_string(&seen).unwrap()),
        ],
    )
    .unwrap();
}

fn compared(drift_in: u8, drift_out: u8) -> PrefixObservation {
    PrefixObservation {
        compared: true,
        drift_in,
        drift_out,
        ..PrefixObservation::default()
    }
}

/// Three turns ten seconds apart: the prefix is written, read back, and then
/// not read back — a loss too soon after the last turn to be an expiry.
fn conversation_that_loses_its_prefix(
    conn: &Connection,
    name: &str,
    start: i64,
    on_the_loss: Option<PrefixObservation>,
) {
    let turn = |time, read, write, observation| Row {
        time,
        conversation: name,
        family: "anthropic_messages",
        read,
        write,
        observation,
    };
    let first = Some(PrefixObservation::default());
    insert(conn, turn(start, 0, 10_000, first));
    insert(
        conn,
        turn(start + 10 * SEC, 10_000, 500, Some(compared(0, 0))),
    );
    insert(conn, turn(start + 20 * SEC, 0, 11_000, on_the_loss));
}

#[test]
fn each_loss_is_put_down_to_the_earliest_segment_and_its_side() {
    let (dir, conn) = database();
    // The client changed a tool and, with it, its history: the tool is first.
    let client = compared(DRIFT_TOOL_DEFS | DRIFT_HISTORY, 0);
    conversation_that_loses_its_prefix(&conn, "a", 0, Some(client));
    // The proxy added a tool; the client changed its system prompt, which is
    // rendered after the tools.
    let proxy = compared(DRIFT_SYSTEM, DRIFT_TOOL_SET);
    conversation_that_loses_its_prefix(&conn, "b", 100 * SEC, Some(proxy));
    conversation_that_loses_its_prefix(&conn, "c", 200 * SEC, Some(compared(0, 0)));
    conversation_that_loses_its_prefix(&conn, "d", 300 * SEC, None);
    let branch = PrefixObservation::default();
    conversation_that_loses_its_prefix(&conn, "e", 400 * SEC, Some(branch));
    drop(conn);

    let report = report(&dir);
    assert!(report.available);
    assert_eq!(report.requests, 15);
    assert_eq!(report.observed, 14, "one row carries no observation");
    let losses = &report.losses;
    assert_eq!(losses.total.events, 5);
    assert_eq!(losses.total.lost_tokens, 5 * 10_500);
    assert_eq!(
        (
            losses.client.events,
            losses.proxy.events,
            losses.none_seen.events,
            losses.unobserved.events,
            losses.not_compared.events
        ),
        (1, 1, 1, 1, 1),
        "every loss lands in exactly one bucket"
    );
    assert!(losses.total.cost > 0.0);

    let segment = |name: &str| report.drift.iter().find(|s| s.segment == name).unwrap();
    assert_eq!(segment("tool_defs").client_losses, 1);
    assert_eq!(segment("tool_defs").client_lost_tokens, 10_500);
    assert_eq!(
        segment("history").client_losses,
        0,
        "the later segment is not charged"
    );
    assert_eq!(
        segment("history").client_turns,
        1,
        "though it is counted as having moved"
    );
    assert_eq!(segment("tool_set").proxy_losses, 1);
    assert_eq!(segment("system").client_losses, 0);
    assert_eq!(segment("system").client_turns, 1);

    // Compared: the second turn of each, and the third of a, b and c.
    assert_eq!(report.compared, 5 + 3);
    assert_eq!(report.stable, 5 + 1);
}

#[test]
fn an_expiry_is_not_a_prefix_change() {
    let (dir, conn) = database();
    let turn = |time, read, write| Row {
        time,
        conversation: "a",
        family: "anthropic_messages",
        read,
        write,
        observation: Some(compared(DRIFT_SYSTEM, 0)),
    };
    insert(&conn, turn(0, 0, 10_000));
    insert(&conn, turn(10 * SEC, 10_000, 500));
    // Ten minutes idle: the cache report calls this an expiry.
    insert(&conn, turn(700 * SEC, 0, 11_000));
    drop(conn);
    assert_eq!(report(&dir).losses.total.events, 0);
}

#[test]
fn composition_is_added_up_per_model() {
    let (dir, conn) = database();
    let seen = |tools_count, tools_deferred| PrefixObservation {
        body_bytes: 1_000,
        system_bytes: 200,
        tools_bytes: 500,
        tools_count,
        tools_deferred,
        messages_bytes: 300,
        text_bytes: 120,
        reasoning_bytes: 50,
        ..PrefixObservation::default()
    };
    for (time, observation) in [(0, seen(20, 0)), (SEC, seen(4, 16)), (2 * SEC, seen(4, 0))] {
        insert(
            &conn,
            Row {
                time,
                conversation: "a",
                family: "openai_chat",
                read: 0,
                write: 0,
                observation: Some(observation),
            },
        );
    }
    drop(conn);

    let report = report(&dir);
    assert_eq!(report.composition.len(), 1);
    let model = &report.composition[0];
    assert_eq!((model.requests, model.body_bytes), (3, 3_000));
    assert_eq!(model.tools_share, 0.5);
    assert_eq!(model.system_share, 0.2);
    assert_eq!(model.reasoning_share, 0.05);
    assert_eq!(model.opaque_share, 0.6, "180 of 300 conversation bytes");
    assert_eq!(model.tools_per_request, 28.0 / 3.0);
    assert_eq!(model.deferring_requests, 1);
    assert_eq!(
        (model.undeferred_requests, model.undeferred_tools_bytes),
        (1, 500),
        "twenty tools and none deferred; four tools is not a long list"
    );
}

#[test]
fn volatile_values_are_counted_by_request_and_by_conversation() {
    let (dir, conn) = database();
    let seen = |volatile| PrefixObservation {
        volatile,
        ..PrefixObservation::default()
    };
    let dated = Volatile {
        dates: 1,
        ..Volatile::default()
    };
    let stamped = Volatile {
        timestamps: 2,
        uuids: 1,
        ..Volatile::default()
    };
    for (time, conversation, volatile) in [
        (0, "a", Some(dated)),
        (SEC, "a", Some(stamped)),
        (2 * SEC, "b", None),
    ] {
        insert(
            &conn,
            Row {
                time,
                conversation,
                family: "openai_chat",
                read: 0,
                write: 0,
                observation: Some(seen(volatile)),
            },
        );
    }
    drop(conn);
    let volatile = report(&dir).volatile;
    assert_eq!((volatile.requests, volatile.conversations), (2, 1));
    assert_eq!(
        (
            volatile.with_dates,
            volatile.with_timestamps,
            volatile.with_uuids,
            volatile.with_jwts
        ),
        (1, 1, 1, 0)
    );
}

fn cold<'a>(
    report: &'a PrefixReport,
    family: &str,
    lifetime_s: u32,
    margin_s: u32,
) -> &'a ColdScore {
    report
        .cold
        .iter()
        .find(|score| {
            score.api_family == family
                && score.lifetime_s == lifetime_s
                && score.margin_s == margin_s
        })
        .unwrap()
}

#[test]
fn a_predicted_lapse_is_scored_against_what_the_provider_read() {
    let (dir, conn) = database();
    let waited = |idle_s: u64, was_warm| PrefixObservation {
        compared: true,
        idle_ms: Some(idle_s * 1_000),
        was_warm,
        lifetime_s: Some(300),
        ..PrefixObservation::default()
    };
    // (idle seconds, cache read, was warm)
    let turns = [
        (400, 0, true),     // lapsed, and predicted up to a 60 s margin
        (310, 5_000, true), // still served: a wrong call at no margin
        (10, 5_000, true),  // warm
        (20, 0, true),      // gone with no pause: nothing predicts it
        (900, 0, false),    // never warm: not a lapse, not scored
    ];
    for (index, (idle_s, read, was_warm)) in turns.into_iter().enumerate() {
        insert(
            &conn,
            Row {
                time: index as i64 * SEC,
                conversation: "a",
                family: "anthropic_messages",
                read,
                write: 100,
                observation: Some(waited(idle_s, was_warm)),
            },
        );
    }
    // No lifetime on the wire: scored against each assumed one.
    insert(
        &conn,
        Row {
            time: 10 * SEC,
            conversation: "o",
            family: "openai_responses",
            read: 0,
            write: 0,
            observation: Some(PrefixObservation {
                lifetime_s: None,
                ..waited(700, true)
            }),
        },
    );
    drop(conn);

    let report = report(&dir);
    let at = |margin| cold(&report, "anthropic_messages", 300, margin);
    assert!(at(0).asked);
    assert_eq!(at(0).scored, 4);
    assert_eq!(
        (at(0).predicted, at(0).confirmed, at(0).wrong, at(0).missed),
        (2, 1, 1, 1)
    );
    assert_eq!((at(0).precision, at(0).recall), (0.5, 0.5));
    assert_eq!(
        (at(60).predicted, at(60).confirmed, at(60).wrong),
        (1, 1, 0)
    );
    assert_eq!(at(60).precision, 1.0);
    assert_eq!(
        (at(120).predicted, at(120).missed),
        (0, 2),
        "400 s is inside 300 + 120"
    );
    assert_eq!(
        at(120).precision,
        0.0,
        "no prediction, no precision to claim"
    );

    let openai = |lifetime| cold(&report, "openai_responses", lifetime, 0);
    assert!(!openai(300).asked);
    assert_eq!((openai(300).confirmed, openai(600).confirmed), (1, 1));
    assert_eq!((openai(3_600).predicted, openai(3_600).missed), (0, 1));
}

/// The reader never migrates, so a database only an older build has written
/// has no observations to read. That is an empty report, not a failure.
#[test]
fn a_database_from_before_the_column_reads_as_unavailable() {
    let (dir, conn) = database();
    conn.execute_batch("ALTER TABLE proxy_requests DROP COLUMN prefix_json;")
        .unwrap();
    drop(conn);
    let report = report(&dir);
    assert!(!report.available);
    assert_eq!(report.observed, 0);
    assert_eq!(report.drift.len(), 6, "the segments are still listed");
}

/// A row written by a later build, with a shape this one cannot read, is left
/// out rather than failing the whole report.
#[test]
fn an_unreadable_observation_is_skipped() {
    let (dir, conn) = database();
    insert(
        &conn,
        Row {
            time: 0,
            conversation: "a",
            family: "openai_chat",
            read: 0,
            write: 0,
            observation: Some(PrefixObservation::default()),
        },
    );
    conn.execute(
        "INSERT INTO proxy_requests (time_ms, tenant, request_id, api_family, upstream, stream,
           prefix_json) VALUES (1, '', 'odd', 'openai_chat', 'u', 1, '{\"compared\": \"yes\"}')",
        [],
    )
    .unwrap();
    drop(conn);
    let report = report(&dir);
    assert_eq!((report.requests, report.observed), (2, 1));
}
