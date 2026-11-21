//! The prefix report, against a real database.

use rusqlite::{Connection, params};
use tempfile::TempDir;

use crate::context::{
    DRIFT_HISTORY, DRIFT_SYSTEM, DRIFT_TOOL_DEFS, DRIFT_TOOL_SET, PrefixObservation, Volatile,
};
use crate::prefix::{ColdScore, PrefixReport};
use crate::{StatsReader, schema};

pub(crate) const SEC: i64 = 1_000;

pub(crate) fn database() -> (TempDir, Connection) {
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

pub(crate) struct Row<'a> {
    pub(crate) time: i64,
    pub(crate) conversation: &'a str,
    pub(crate) family: &'a str,
    pub(crate) read: i64,
    pub(crate) write: i64,
    pub(crate) observation: Option<PrefixObservation>,
}

pub(crate) fn insert(conn: &Connection, row: Row<'_>) {
    conn.execute(
        "INSERT INTO proxy_requests (time_ms, tenant, request_id, api_family, model, upstream,
           status, stream, prompt_tokens, cache_read_tokens, cache_write_tokens, conversation,
           total_ms, prefix_json)
         VALUES (?1, '', ?2, ?3, 'm', 'u', 200, 1, ?8, ?4, ?5, ?6, 100.0, ?7)",
        params![
            row.time,
            format!("{}-{}", row.conversation, row.time),
            row.family,
            row.read,
            row.write,
            row.conversation,
            row.observation
                .map(|seen| serde_json::to_string(&seen).unwrap()),
            // Anthropic reports the uncached remainder; OpenAI the whole
            // prompt, cached part included.
            if row.family.starts_with("openai") {
                30_000
            } else {
                10
            },
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
    let compacted = PrefixObservation {
        tools_compacted_bytes: 40,
        ..seen(4, 0)
    };
    for (time, observation) in [(0, seen(20, 0)), (SEC, seen(4, 16)), (2 * SEC, compacted)] {
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
    assert_eq!(model.tool_search_requests, 0);
    assert_eq!(
        (model.tools_compacted_bytes, model.tools_compacted_requests),
        (40, 1)
    );
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

/// A conversation whose instructions and tools (13,000 tokens) other
/// conversations keep cached, and whose own messages are what can lapse. Each
/// turn says how long it had been idle and what it then read; a turn that
/// reads only the shared opening has lost everything of its own.
#[test]
fn a_predicted_lapse_is_scored_against_what_the_conversation_lost() {
    let (dir, conn) = database();
    let waited = |idle_s: u64, was_warm| PrefixObservation {
        compared: true,
        idle_ms: Some(idle_s * 1_000),
        was_warm,
        continues: Some(true),
        lifetime_s: Some(300),
        // 40,000 bytes of opening: the 13,000 tokens every turn reads.
        system_bytes: 16_000,
        tools_bytes: 24_000,
        ..PrefixObservation::default()
    };
    // (idle seconds, cache read, cache write, was warm)
    let turns = [
        (0, 13_000, 20_000, false),  // the opening turn: nothing to lapse
        (10, 33_000, 1_000, true),   // read back in full
        (400, 13_000, 21_000, true), // lapsed: only the shared opening came back
        (310, 34_000, 1_000, true),  // past the lifetime, and read back in full
        (20, 13_000, 22_000, true),  // lost with no pause: nothing predicts it
    ];
    for (index, (idle_s, read, write, was_warm)) in turns.into_iter().enumerate() {
        insert(
            &conn,
            Row {
                time: index as i64 * SEC,
                conversation: "a",
                family: "anthropic_messages",
                read,
                write,
                observation: Some(waited(idle_s, was_warm)),
            },
        );
    }
    // A recompacted turn is not scored: what it read says what was rewritten.
    insert(
        &conn,
        Row {
            time: 5 * SEC,
            conversation: "a",
            family: "anthropic_messages",
            read: 13_000,
            write: 9_000,
            observation: Some(PrefixObservation {
                recompacted: Some("predicted".into()),
                ..waited(900, true)
            }),
        },
    );
    // No lifetime on the wire: scored against each assumed one.
    for (time, read, idle_s) in [(20 * SEC, 0, 0), (21 * SEC, 30_000, 5), (22 * SEC, 0, 700)] {
        insert(
            &conn,
            Row {
                time,
                conversation: "o",
                family: "openai_responses",
                read,
                write: 0,
                observation: Some(PrefixObservation {
                    lifetime_s: None,
                    ..waited(idle_s, time > 20 * SEC)
                }),
            },
        );
    }
    drop(conn);

    let report = report(&dir);
    let at = |margin| cold(&report, "anthropic_messages", 300, margin);
    assert!(at(0).asked);
    assert_eq!(at(0).scored, 4, "four warm turns that were not recompacted");
    assert_eq!(
        (at(0).predicted, at(0).confirmed, at(0).wrong, at(0).missed),
        (2, 1, 1, 1),
        "a read of the shared opening alone is a lapse, not a live cache"
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

/// A loss is an expiry only if it took the whole of the conversation's own
/// messages and nothing in the prefix was seen to change. Every other loss on
/// a long-idle turn would have happened whatever the idle time, and is
/// counted beside the prediction it does not confirm.
#[test]
fn a_loss_that_idle_time_does_not_explain_does_not_confirm_a_prediction() {
    let (dir, conn) = database();
    let waited = |idle_s: u64| PrefixObservation {
        compared: true,
        idle_ms: Some(idle_s * 1_000),
        was_warm: true,
        continues: Some(true),
        lifetime_s: Some(300),
        system_bytes: 16_000,
        tools_bytes: 24_000,
        ..PrefixObservation::default()
    };
    // (cache read, cache write, observation)
    let turns = [
        (13_000, 20_000, PrefixObservation::default()),
        (33_000, 1_000, waited(10)),
        // Idle 400 s; the proxy rewrote a message near the end. Most of the
        // conversation was read back all the same.
        (
            28_000,
            7_000,
            PrefixObservation {
                drift_out: DRIFT_HISTORY,
                ..waited(400)
            },
        ),
        // Idle 400 s, nothing seen to change, and the first two thirds of
        // the conversation read back: not what an expiry leaves.
        (25_000, 11_000, waited(400)),
        // Idle 400 s and lost whole on a request with nothing before it to
        // compare with.
        (
            13_000,
            24_000,
            PrefixObservation {
                compared: false,
                ..waited(400)
            },
        ),
        // Idle 400 s and lost whole, nothing changed: an expiry.
        (13_000, 25_000, waited(400)),
        // The same, on a request that does not continue the conversation —
        // another one opening with the same bytes. The runtime would not
        // have predicted on it, and it is not scored.
        (
            13_000,
            25_000,
            PrefixObservation {
                continues: Some(false),
                ..waited(400)
            },
        ),
        // Not idle, and part of the conversation lost: nothing was predicted
        // and no expiry was missed.
        (30_000, 9_000, waited(5)),
    ];
    for (index, (read, write, observation)) in turns.into_iter().enumerate() {
        insert(
            &conn,
            Row {
                time: index as i64 * SEC,
                conversation: "a",
                family: "anthropic_messages",
                read,
                write,
                observation: Some(observation),
            },
        );
    }
    drop(conn);
    let report = report(&dir);
    let at = cold(&report, "anthropic_messages", 300, 0);
    assert_eq!(
        at.scored, 6,
        "every warm, continued turn that was not the non-continuation"
    );
    assert_eq!(at.predicted, 4);
    assert_eq!(
        (at.confirmed, at.partial, at.drift, at.unknown, at.wrong),
        (1, 1, 1, 1, 0)
    );
    assert_eq!(
        at.missed, 0,
        "a partial loss on a busy turn is not a missed expiry"
    );
    assert_eq!(at.precision, 0.25, "one expiry of four predictions");
    assert_eq!(at.recall, 1.0);
}

/// Rows from before `continues` was recorded are scored where they were
/// compared, which is what a request carrying no fewer messages than the one
/// before it was.
#[test]
fn an_older_row_is_scored_where_it_was_compared() {
    let old = PrefixObservation {
        compared: true,
        idle_ms: Some(1),
        was_warm: true,
        ..PrefixObservation::default()
    };
    assert!(old.could_predict());
    let branch = PrefixObservation {
        compared: false,
        ..old.clone()
    };
    assert!(!branch.could_predict());
    assert!(
        !PrefixObservation {
            was_warm: false,
            ..old.clone()
        }
        .could_predict()
    );
    // The breaker's classifier and the report's are this one function: a
    // read past the opening on a turn with no retrieve rounds.
    assert!(old.prediction_was_wrong(Some(9_000), 0));
    assert!(
        !old.prediction_was_wrong(Some(9_000), 1),
        "a later round read an earlier one's write"
    );
    assert!(
        !old.prediction_was_wrong(Some(500), 0),
        "the shared opening alone"
    );
    assert!(
        !old.prediction_was_wrong(None, 0),
        "no usage is no evidence"
    );
    assert!(
        !serde_json::to_string(&old).unwrap().contains("continues"),
        "and a row that does not know says nothing"
    );
}

/// The provider's own tool-search tool in the list is a client loading tools
/// on demand, whatever `defer_loading` flags the rest of the list carries.
#[test]
fn a_tool_search_client_is_not_counted_as_carrying_its_tools_whole() {
    let (dir, conn) = database();
    let seen = |tools_count, tool_search| PrefixObservation {
        body_bytes: 1_000,
        tools_bytes: 500,
        tools_count,
        tool_search,
        ..PrefixObservation::default()
    };
    for (time, observation) in [(0, seen(20, true)), (SEC, seen(20, false))] {
        insert(
            &conn,
            Row {
                time,
                conversation: "a",
                family: "anthropic_messages",
                read: 0,
                write: 0,
                observation: Some(observation),
            },
        );
    }
    drop(conn);
    let report = report(&dir);
    let model = &report.composition[0];
    assert_eq!(
        (
            model.deferring_requests,
            model.tool_search_requests,
            model.undeferred_requests,
            model.undeferred_tools_bytes
        ),
        (1, 1, 1, 500)
    );
}

#[test]
fn recompactions_are_counted_by_how_the_lapse_was_known() {
    let (dir, conn) = database();
    let turn = |kind: &str, ledger_reads| PrefixObservation {
        compared: true,
        recompacted: Some(kind.to_string()),
        ledger_reads,
        ..PrefixObservation::default()
    };
    // (conversation, cache read, observation)
    let rows = [
        ("a", 0, turn("observed", 1)),
        ("a", 0, turn("predicted", 3)),
        ("b", 0, turn("predicted", 2)),
        // Predicted, and the read reached past the opening: it was alive.
        ("b", 9_000, turn("predicted", 2)),
        // Predicted, and only the shared opening came back: a real lapse.
        (
            "d",
            2_000,
            PrefixObservation {
                system_bytes: 3_000,
                tools_bytes: 3_000,
                ..turn("predicted", 1)
            },
        ),
        ("c", 9_000, compared(0, 0)),
    ];
    for (index, (conversation, read, observation)) in rows.into_iter().enumerate() {
        insert(
            &conn,
            Row {
                time: index as i64 * SEC,
                conversation,
                family: "anthropic_messages",
                read,
                write: 100,
                observation: Some(observation),
            },
        );
    }
    drop(conn);
    let seen = report(&dir).recompactions;
    assert_eq!(
        (seen.observed, seen.predicted, seen.predicted_wrong),
        (1, 4, 1)
    );
    assert_eq!(seen.conversations, 3);
    assert_eq!(
        seen.stale_reads,
        3 + 2 + 1,
        "each conversation's ledger once, at its largest"
    );
}

/// "Predicted wrongly" is counted as the proxy's breaker counted it: a turn
/// that spent retrieve rounds sums a later round's read of what an earlier
/// one wrote, and is not evidence the cache was alive.
#[test]
fn a_recompacted_turn_with_retrieve_rounds_is_not_a_wrong_prediction() {
    let (dir, conn) = database();
    let predicted = PrefixObservation {
        compared: true,
        recompacted: Some("predicted".to_string()),
        ..PrefixObservation::default()
    };
    for (index, conversation) in ["a", "b"].into_iter().enumerate() {
        insert(
            &conn,
            Row {
                time: index as i64 * SEC,
                conversation,
                family: "anthropic_messages",
                read: 9_000,
                write: 100,
                observation: Some(predicted.clone()),
            },
        );
    }
    conn.execute(
        "UPDATE proxy_requests SET retrieve_rounds = 2 WHERE conversation = 'b'",
        [],
    )
    .unwrap();
    drop(conn);
    let seen = report(&dir).recompactions;
    assert_eq!((seen.predicted, seen.predicted_wrong), (2, 1));
}

/// The numbers of the run that found this: Claude Code's opening is 7,374
/// bytes of instructions and 30,687 of tools, 40,705 once the proxy's retrieve
/// tool is in it, and the provider reads it back as 14,482 tokens. Sized from
/// the client's bytes at a token per three it came to 13,711, so three correct
/// predictions were called wrong and prediction ended for the run.
#[test]
fn a_read_of_a_tool_heavy_opening_is_not_a_read_of_the_conversation() {
    let seen = PrefixObservation {
        compared: true,
        system_bytes: 7_374,
        tools_bytes: 30_687,
        opening_bytes: 40_705,
        ..PrefixObservation::default()
    };
    assert!(!seen.read_reaches_messages(14_482));
    assert!(!seen.prediction_was_wrong(Some(14_482), 0));
    assert!(
        seen.prediction_was_wrong(Some(22_017), 0),
        "a read well into the messages still counts"
    );
    // A row from before the opening was recorded falls back on the client's
    // bytes, and still clears the same read.
    let old = PrefixObservation {
        opening_bytes: 0,
        ..seen.clone()
    };
    assert!(!old.read_reaches_messages(14_482));
    assert!(old.read_reaches_messages(22_017));
    // What went upstream is what counts where the two differ.
    let grown = PrefixObservation {
        system_bytes: 1_000,
        tools_bytes: 1_000,
        opening_bytes: 40_000,
        ..PrefixObservation::default()
    };
    assert!(!grown.read_reaches_messages(14_000));
    let json = serde_json::to_string(&old).unwrap();
    assert!(!json.contains("opening_bytes"), "{json}");
}

/// What a lost prefix is put down to, case by case.
#[test]
fn a_loss_is_classified_by_what_else_was_true_of_the_request() {
    use crate::context::{DRIFT_TOOL_ORDER, LapseOutcome};
    let seen = PrefixObservation {
        compared: true,
        system_bytes: 16_000,
        tools_bytes: 24_000,
        ..PrefixObservation::default()
    };
    assert_eq!(seen.lapse_outcome(false, 0), LapseOutcome::ReadBack);
    assert_eq!(seen.lapse_outcome(true, 0), LapseOutcome::Full);
    assert_eq!(seen.lapse_outcome(true, 13_000), LapseOutcome::Full);
    assert_eq!(seen.lapse_outcome(true, 20_000), LapseOutcome::Partial);
    let client = PrefixObservation {
        drift_in: DRIFT_TOOL_ORDER,
        ..seen.clone()
    };
    assert_eq!(client.lapse_outcome(true, 0), LapseOutcome::Drift);
    // A change the proxy put back never reached the provider, and so cannot
    // be what it failed to read.
    let repaired = PrefixObservation {
        repaired: DRIFT_TOOL_ORDER,
        ..client.clone()
    };
    assert_eq!(repaired.lapse_outcome(true, 0), LapseOutcome::Full);
    let ours = PrefixObservation {
        drift_out: DRIFT_HISTORY,
        ..seen.clone()
    };
    assert_eq!(ours.lapse_outcome(true, 20_000), LapseOutcome::Drift);
    let first = PrefixObservation {
        compared: false,
        ..ours
    };
    assert_eq!(first.lapse_outcome(true, 0), LapseOutcome::Unknown);
    assert_eq!(first.lapse_outcome(false, 0), LapseOutcome::ReadBack);
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
