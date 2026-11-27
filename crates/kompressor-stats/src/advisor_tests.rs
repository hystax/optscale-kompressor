//! The advisor's verdicts, against a real database.

use rusqlite::Connection;
use tempfile::TempDir;

use crate::StatsReader;
use crate::advisor::{Advice, AdvisorReport, Verdict};
use crate::context::{DRIFT_TOOL_ORDER, PrefixObservation};
use crate::prefix_tests::{Row, SEC, database, insert};

fn report(dir: &TempDir) -> AdvisorReport {
    StatsReader::open(dir.path().join("stats.db"))
        .unwrap()
        .advisor("", 0, i64::MAX)
        .unwrap()
}

fn advice<'a>(report: &'a AdvisorReport, setting: &str) -> &'a Advice {
    report
        .advice
        .iter()
        .find(|advice| advice.setting == setting)
        .unwrap_or_else(|| panic!("no advice for {setting}"))
}

fn fact(advice: &Advice, label: &str) -> f64 {
    advice
        .facts
        .iter()
        .find(|fact| fact.label == label)
        .unwrap_or_else(|| panic!("no fact {label:?} in {:?}", advice.facts))
        .value
}

fn compared() -> PrefixObservation {
    PrefixObservation {
        compared: true,
        ..PrefixObservation::default()
    }
}

/// An Anthropic conversation of `turns` requests ten seconds apart, each
/// reading back what the one before left. `seen` shapes each observation.
fn warm_conversation(
    conn: &Connection,
    name: &str,
    start: i64,
    turns: usize,
    seen: impl Fn(usize) -> PrefixObservation,
) -> i64 {
    insert(
        conn,
        Row {
            time: start,
            conversation: name,
            family: "anthropic_messages",
            read: 0,
            write: 10_000,
            observation: Some(PrefixObservation::default()),
        },
    );
    for turn in 1..turns {
        insert(
            conn,
            Row {
                time: start + turn as i64 * 10 * SEC,
                conversation: name,
                family: "anthropic_messages",
                read: 10_000 + 100 * (turn as i64 - 1),
                write: 100,
                observation: Some(seen(turn)),
            },
        );
    }
    start + turns as i64 * 10 * SEC
}

#[test]
fn with_too_little_traffic_nothing_is_recommended_either_way() {
    let (dir, conn) = database();
    warm_conversation(&conn, "a", 0, 5, |_| compared());
    drop(conn);
    let report = report(&dir);
    assert!(report.available);
    assert_eq!(report.advice.len(), 8, "one row per setting, always");
    for advice in &report.advice {
        assert_eq!(advice.verdict, Verdict::NotEnoughData, "{}", advice.setting);
        assert!(!advice.why.is_empty(), "{} says why", advice.setting);
        assert_eq!(advice.value, None);
    }
    assert!(advice(&report, "tools_defer").built);
}

#[test]
fn a_database_with_no_observations_gives_no_advice() {
    let (dir, conn) = database();
    conn.execute_batch("ALTER TABLE proxy_requests DROP COLUMN prefix_json;")
        .unwrap();
    drop(conn);
    let report = report(&dir);
    assert!(!report.available);
    assert!(report.advice.is_empty());
}

#[test]
fn a_drift_that_costs_the_cache_earns_its_setting_and_one_that_never_happens_does_not() {
    let (dir, conn) = database();
    let end = warm_conversation(&conn, "a", 0, 130, |_| compared());
    // The client reorders its tools and the prefix is not read back.
    insert(
        &conn,
        Row {
            time: end,
            conversation: "a",
            family: "anthropic_messages",
            read: 0,
            write: 24_000,
            observation: Some(PrefixObservation {
                drift_in: DRIFT_TOOL_ORDER,
                ..compared()
            }),
        },
    );
    drop(conn);

    let report = report(&dir);
    assert!(report.input_cost > 0.0);
    let pinned = advice(&report, "tools_order");
    assert_eq!(pinned.verdict, Verdict::TurnOn, "{}", pinned.why);
    assert_eq!(pinned.value.as_deref(), Some("pinned"));
    assert!(pinned.measure.unwrap() >= pinned.threshold);
    assert_eq!(fact(pinned, "cache losses put down to it"), 1.0);
    assert!(pinned.saving.unwrap() > 20_000.0, "the loss, priced");
    assert_eq!(
        pinned.enabled, None,
        "the bare report does not know the configuration"
    );

    let beta = advice(&report, "sticky_beta");
    assert_eq!(beta.verdict, Verdict::NotWorthIt);
    assert!(beta.why.contains("No client changed it"), "{}", beta.why);
}

#[test]
fn compaction_is_judged_on_what_it_would_remove() {
    let verdict = |compactable| {
        let (dir, conn) = database();
        warm_conversation(&conn, "a", 0, 120, |_| PrefixObservation {
            tools_count: 12,
            tools_bytes: 1_000,
            tools_compactable_bytes: compactable,
            ..compared()
        });
        drop(conn);
        let report = report(&dir);
        let out = advice(&report, "tools_compact").clone();
        (out.verdict, out.measure, out.value)
    };
    assert_eq!(
        verdict(50),
        (Verdict::TurnOn, Some(0.05), Some("true".to_string()))
    );
    assert_eq!(verdict(20), (Verdict::NotWorthIt, Some(0.02), None));
}

#[test]
fn a_cache_key_is_advised_when_unkeyed_requests_read_less() {
    let (dir, conn) = database();
    // The test fixture gives every OpenAI row a 30,000-token prompt.
    for (index, (keyed, read)) in (0..120)
        .map(|n| (n % 2 == 0, if n % 2 == 0 { 27_000 } else { 15_000 }))
        .enumerate()
    {
        insert(
            &conn,
            Row {
                time: index as i64 * SEC,
                conversation: if keyed { "k" } else { "u" },
                family: "openai_responses",
                read,
                write: 0,
                observation: Some(PrefixObservation {
                    client_cache_key: Some(keyed),
                    ..compared()
                }),
            },
        );
    }
    drop(conn);
    let report = report(&dir);
    let key = advice(&report, "prompt_cache_key");
    assert_eq!(key.verdict, Verdict::TurnOn, "{}", key.why);
    assert_eq!(key.value.as_deref(), Some("conversation"));
    assert!((key.measure.unwrap() - 0.4).abs() < 1e-9, "90% against 50%");
    assert_eq!(fact(key, "requests that named none"), 60.0);
    assert!(key.saving.unwrap() > 0.0);
}

/// Twenty-five cycles of a warm turn and, fifteen minutes later, one that
/// reads back only the shared opening: lapses that idle time predicts.
fn lapsing(conn: &Connection, with_reads: bool) {
    let mut time = 0;
    insert(
        conn,
        Row {
            time,
            conversation: "a",
            family: "anthropic_messages",
            read: 0,
            write: 40_000,
            observation: Some(PrefixObservation::default()),
        },
    );
    for _ in 0..25 {
        time += 10 * SEC;
        insert(
            conn,
            Row {
                time,
                conversation: "a",
                family: "anthropic_messages",
                read: 40_000,
                write: 0,
                observation: Some(PrefixObservation {
                    idle_ms: Some(10_000),
                    was_warm: true,
                    lifetime_s: Some(300),
                    ..compared()
                }),
            },
        );
        time += 900 * SEC;
        insert(
            conn,
            Row {
                time,
                conversation: "a",
                family: "anthropic_messages",
                read: 13_000,
                write: 27_000,
                observation: Some(PrefixObservation {
                    idle_ms: Some(900_000),
                    was_warm: true,
                    lifetime_s: Some(300),
                    // The 13,000 tokens it read are the instructions and
                    // tools other conversations keep cached: none of its own.
                    system_bytes: 40_000,
                    text_bytes: 100_000,
                    stale_read_bytes: if with_reads { 5_000 } else { 0 },
                    superseded_read_bytes: if with_reads { 12_000 } else { 0 },
                    quiet_read_bytes: if with_reads { 18_000 } else { 0 },
                    ..compared()
                }),
            },
        );
    }
    // Padding, so the report has enough compared requests to speak at all.
    warm_conversation(conn, "b", time + 10 * SEC, 60, |_| compared());
}

#[test]
fn lapses_that_idle_time_predicts_earn_predicted_recompaction_and_name_the_margin() {
    let (dir, conn) = database();
    lapsing(&conn, true);
    drop(conn);
    let report = report(&dir);

    let cold = advice(&report, "cold_recompact");
    assert_eq!(cold.verdict, Verdict::TurnOn, "{}", cold.why);
    assert_eq!(
        cold.value.as_deref(),
        Some("predicted, cold_recompact_margin = \"0s\""),
        "the smallest margin that held"
    );
    assert_eq!(fact(cold, "turns that lost their prefix to expiry"), 25.0);
    assert_eq!(fact(cold, "their precision"), 1.0);
    assert_eq!(cold.saving, None, "what is at stake is not what is saved");
    assert!(cold.why.contains("first_seen"));

    let reads = advice(&report, "superseded_reads, matured_reads");
    assert_eq!(reads.verdict, Verdict::TurnOn, "{}", reads.why);
    assert_eq!(
        reads.value.as_deref(),
        Some("superseded_reads = true, matured_reads = true")
    );
    assert!(
        (reads.measure.unwrap() - 0.30).abs() < 1e-9,
        "12% and 18%; stale is not theirs"
    );
    assert_eq!(fact(reads, "reads a later edit invalidated"), 0.05);
}

#[test]
fn lapses_with_nothing_to_rewrite_leave_the_read_decisions_off() {
    let (dir, conn) = database();
    lapsing(&conn, false);
    drop(conn);
    let report = report(&dir);
    assert_eq!(advice(&report, "cold_recompact").verdict, Verdict::TurnOn);
    let reads = advice(&report, "superseded_reads, matured_reads");
    assert_eq!(reads.verdict, Verdict::NotWorthIt);
    assert_eq!(reads.measure, Some(0.0));
}

#[test]
fn a_conversation_that_never_lapses_is_not_worth_recompacting() {
    let (dir, conn) = database();
    warm_conversation(&conn, "a", 0, 150, |_| compared());
    drop(conn);
    let report = report(&dir);
    let cold = advice(&report, "cold_recompact");
    assert_eq!(cold.verdict, Verdict::NotWorthIt);
    assert_eq!(cold.measure, Some(0.0));
    assert_eq!(
        advice(&report, "superseded_reads, matured_reads").verdict,
        Verdict::NotEnoughData,
        "no lapsed turn to look at"
    );
}

#[test]
fn a_model_that_re_sends_reasoning_is_named_and_one_that_does_not_is_not() {
    let (dir, conn) = database();
    for index in 0..120 {
        let reasoner = index % 2 == 0;
        conn.execute(
            "INSERT INTO proxy_requests (time_ms, tenant, request_id, api_family, model, upstream,
               status, stream, prompt_tokens, conversation, total_ms, prefix_json)
             VALUES (?1, '', ?2, 'openai_chat', ?3, 'u', 200, 1, 30000, ?3, 100.0, ?4)",
            rusqlite::params![
                index as i64 * SEC,
                format!("r{index}"),
                if reasoner {
                    "deepseek-reasoner"
                } else {
                    "gpt-5.5"
                },
                serde_json::to_string(&PrefixObservation {
                    body_bytes: 10_000,
                    reasoning_bytes: if reasoner { 2_000 } else { 100 },
                    ..compared()
                })
                .unwrap(),
            ],
        )
        .unwrap();
    }
    drop(conn);
    let report = report(&dir);
    let reasoning = advice(&report, "reasoning_elide_models");
    assert_eq!(reasoning.verdict, Verdict::TurnOn, "{}", reasoning.why);
    assert_eq!(reasoning.value.as_deref(), Some("[\"deepseek-reasoner\"]"));
    assert_eq!(reasoning.measure, Some(0.2));
    assert!(
        reasoning.why.contains("live request"),
        "it says to check the provider first"
    );
}

#[test]
fn long_undeferred_tool_lists_make_the_case_for_turning_deferral_on() {
    let (dir, conn) = database();
    warm_conversation(&conn, "a", 0, 120, |turn| PrefixObservation {
        tools_count: if turn % 4 == 0 { 30 } else { 6 },
        tools_bytes: 20_000,
        ..compared()
    });
    drop(conn);
    let report = report(&dir);
    let deferral = advice(&report, "tools_defer");
    assert!(deferral.built);
    assert_eq!(deferral.verdict, Verdict::TurnOn, "{}", deferral.why);
    assert!(deferral.why.contains("tools_defer = true"));
    assert!(deferral.measure.unwrap() > 0.2);
}

#[test]
fn what_is_on_is_marked_by_whoever_knows_the_configuration() {
    let (dir, conn) = database();
    warm_conversation(&conn, "a", 0, 5, |_| compared());
    drop(conn);
    let mut report = report(&dir);
    report.mark_enabled("tools_order", true);
    report.mark_enabled("no such setting", true);
    assert_eq!(advice(&report, "tools_order").enabled, Some(true));
    assert_eq!(advice(&report, "sticky_beta").enabled, None);
}
