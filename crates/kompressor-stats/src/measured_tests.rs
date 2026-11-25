//! The three gate measurements, against a real database filled through the
//! writer.

use rusqlite::params;
use tempfile::TempDir;

use crate::events::{
    Elided, ElisionsEvent, Event, ProcessStartEvent, ProxyRequestEvent, RetrievalEvent,
    RetrieveRoundsEvent, RoundUsage, Surface,
};
use crate::measured::{FETCH_PROBABILITY_SQL, RESTARTS_SQL, RETRIEVE_ROUNDS_SQL};
use crate::{StatsConfig, StatsReader, StatsWriter};

const MIN: i64 = 60_000;
/// A start well clear of zero, so the hour before it is a real window.
const T: i64 = 1_800_000_000_000;

struct Db {
    dir: TempDir,
    writer: StatsWriter,
}

impl Db {
    fn new() -> Db {
        let dir = TempDir::new().unwrap();
        let writer = StatsWriter::open(&StatsConfig::at(dir.path().join("stats.db"))).unwrap();
        Db { dir, writer }
    }

    fn write(&mut self, events: Vec<Event>) {
        self.writer.write_batch(&events).unwrap();
    }

    fn reader(&self) -> StatsReader {
        StatsReader::open(self.dir.path().join("stats.db")).unwrap()
    }
}

struct Turn<'a> {
    time: i64,
    request_id: &'a str,
    conversation: &'a str,
    family: &'a str,
    prompt: u32,
    read: u32,
    write: u32,
    rounds: u32,
    keep_warm: bool,
}

impl Default for Turn<'_> {
    fn default() -> Self {
        Turn {
            time: T,
            request_id: "",
            conversation: "c",
            family: "anthropic_messages",
            prompt: 10,
            read: 0,
            write: 0,
            rounds: 0,
            keep_warm: false,
        }
    }
}

fn turn(turn: Turn<'_>) -> Event {
    let request_id = if turn.request_id.is_empty() {
        format!("{}-{}-{}", turn.conversation, turn.time, turn.keep_warm)
    } else {
        turn.request_id.to_string()
    };
    Event::ProxyRequest(ProxyRequestEvent {
        context: None,
        timing: None,
        prefix: None,
        time_ms: turn.time,
        tenant: String::new(),
        request_id,
        api_family: turn.family.into(),
        model: Some("claude-opus-5-5".into()),
        upstream: "u".into(),
        status: Some(200),
        stream: false,
        strip_usage: false,
        prompt_tokens: Some(turn.prompt),
        completion_tokens: Some(20),
        cache_read_tokens: Some(turn.read),
        cache_write_tokens: Some(turn.write),
        cache_write_1h_tokens: None,
        shadow_prompt_tokens: None,
        conversation: Some(turn.conversation.into()),
        retrieve_rounds: turn.rounds,
        retrieve_tokens_injected: 0,
        upstream_ms: None,
        total_ms: None,
        error: None,
        keep_warm: turn.keep_warm,
        output_shaping: None,
    })
}

fn start(time_ms: i64) -> Event {
    Event::ProcessStart(ProcessStartEvent {
        time_ms,
        version: "2.6.0".into(),
        pid: Some(42),
        surfaces: "api,proxy".into(),
    })
}

// ----------------------------------------------------------------- restarts

#[test]
fn with_no_restart_on_record_there_is_nothing_to_set_against() {
    let mut db = Db::new();
    db.write(vec![
        turn(Turn {
            time: T - MIN,
            write: 40_000,
            ..Turn::default()
        }),
        turn(Turn {
            time: T + MIN,
            read: 40_000,
            ..Turn::default()
        }),
    ]);
    let report = db.reader().restarts("", 0, i64::MAX, None).unwrap();
    assert!(report.scope.available);
    assert!(report.starts.is_empty());
    assert_eq!(report.totals.conversations_across, 0);
    assert_eq!(report.totals.read_share, None);
    assert!(report.scope.query.contains("process_starts"));
}

#[test]
fn a_conversation_that_ended_before_the_start_is_counted_before_and_not_across() {
    let mut db = Db::new();
    db.write(vec![
        start(T),
        turn(Turn {
            time: T - 10 * MIN,
            write: 40_000,
            ..Turn::default()
        }),
    ]);
    let report = db.reader().restarts("", 0, i64::MAX, None).unwrap();
    let row = &report.starts[0];
    assert_eq!(
        (
            row.conversations_before,
            row.conversations_after,
            row.conversations_across
        ),
        (1, 0, 0)
    );
    assert_eq!(row.read_share, None, "nothing was open across it");
    assert_eq!(
        (row.version.as_str(), row.surfaces.as_str()),
        ("2.6.0", "api,proxy")
    );
}

#[test]
fn a_restart_is_set_against_the_last_turn_before_and_the_first_after() {
    let mut db = Db::new();
    db.write(vec![
        start(T),
        // a: only before
        turn(Turn {
            time: T - 10 * MIN,
            conversation: "a",
            write: 5_000,
            ..Turn::default()
        }),
        // b: open across, three minutes apart, and found cold
        turn(Turn {
            time: T - 30 * MIN,
            conversation: "b",
            write: 10_000,
            ..Turn::default()
        }),
        turn(Turn {
            time: T - 2 * MIN,
            conversation: "b",
            read: 10_000,
            write: 40_000,
            ..Turn::default()
        }),
        // a ping is not a turn
        turn(Turn {
            time: T - MIN,
            conversation: "b",
            read: 99_999,
            keep_warm: true,
            ..Turn::default()
        }),
        turn(Turn {
            time: T + MIN,
            conversation: "b",
            write: 50_000,
            ..Turn::default()
        }),
        turn(Turn {
            time: T + 2 * MIN,
            conversation: "b",
            read: 50_000,
            ..Turn::default()
        }),
        // c: open across, fifty minutes apart, and found warm
        turn(Turn {
            time: T - 20 * MIN,
            conversation: "c",
            read: 30_000,
            ..Turn::default()
        }),
        turn(Turn {
            time: T + 30 * MIN,
            conversation: "c",
            read: 30_000,
            ..Turn::default()
        }),
        // d: only after; e: more than an hour before, so not open at all
        turn(Turn {
            time: T + 5 * MIN,
            conversation: "d",
            ..Turn::default()
        }),
        turn(Turn {
            time: T - 2 * 60 * MIN,
            conversation: "e",
            ..Turn::default()
        }),
    ]);
    let report = db.reader().restarts("", 0, i64::MAX, None).unwrap();
    assert_eq!(report.starts.len(), 1);
    let row = &report.starts[0];
    assert_eq!(
        (
            row.conversations_before,
            row.conversations_after,
            row.conversations_across
        ),
        (3, 3, 2)
    );
    assert_eq!(row.cached_before, 80_000, "b's 50,000 and c's 30,000");
    assert_eq!(row.read_after, 30_000, "b's first turn after read nothing");
    assert_eq!(row.lost_tokens, 50_000);
    assert_eq!(row.cold_after, 1);
    assert_eq!(row.read_share, Some(30_000.0 / 80_000.0));
    assert_eq!(
        row.across_within_ttl, 1,
        "only b's turns were five minutes apart"
    );
    assert_eq!(
        (row.cached_before_within_ttl, row.read_after_within_ttl),
        (50_000, 0)
    );
    assert_eq!(row.read_share_within_ttl, Some(0.0));
    assert_eq!(report.totals.conversations_across, 2);
    assert_eq!(report.totals.read_share, row.read_share);

    // A start outside the window is not reported; the turns around one
    // inside it are read whatever the window says.
    let narrow = db.reader().restarts("", T, T + 1, None).unwrap();
    assert_eq!(narrow.starts.len(), 1);
    assert_eq!(narrow.starts[0].conversations_across, 2);
    let outside = db.reader().restarts("", T + 1, T + MIN, None).unwrap();
    assert!(outside.starts.is_empty());
    // Another tenant's conversations are not this one's.
    let other = db.reader().restarts("other", 0, i64::MAX, None).unwrap();
    assert_eq!(other.starts[0].conversations_before, 0);
}

#[test]
fn the_experiment_filter_keeps_only_turns_recorded_under_it() {
    let mut db = Db::new();
    db.write(vec![
        start(T),
        turn(Turn {
            time: T - MIN,
            write: 10_000,
            ..Turn::default()
        }),
        turn(Turn {
            time: T + MIN,
            read: 10_000,
            ..Turn::default()
        }),
    ]);
    db.writer
        .connection()
        .execute(
            "INSERT OR REPLACE INTO request_context (tenant, request_id, time_ms, context)
             SELECT tenant, request_id, time_ms,
                    '{\"build_version\":\"b\",\"config_fingerprint\":\"f\",\"experiment_id\":\"t0\",\"run_id\":\"r\",\"settings\":{}}'
               FROM proxy_requests",
            [],
        )
        .unwrap();
    let reader = db.reader();
    let tagged = reader.restarts("", 0, i64::MAX, Some("t0")).unwrap();
    assert_eq!(tagged.starts[0].conversations_across, 1);
    assert_eq!(tagged.scope.experiment_id.as_deref(), Some("t0"));
    let other = reader.restarts("", 0, i64::MAX, Some("t1")).unwrap();
    assert_eq!(other.starts[0].conversations_across, 0);
}

#[test]
fn a_database_without_the_v18_tables_says_so_rather_than_failing() {
    let db = Db::new();
    db.writer
        .connection()
        .execute_batch(
            "DROP TABLE process_starts; DROP TABLE retrieve_round_usage; DROP TABLE elisions;",
        )
        .unwrap();
    let reader = db.reader();
    assert!(
        !reader
            .restarts("", 0, i64::MAX, None)
            .unwrap()
            .scope
            .available
    );
    let rounds = reader.retrieve_round_cost("", 0, i64::MAX, None).unwrap();
    assert!(!rounds.scope.available);
    assert!(
        !reader
            .fetch_probability("", 0, i64::MAX, None)
            .unwrap()
            .scope
            .available
    );
    // and a prune over it does not trip on them either
    crate::retention::prune_on(db.writer.connection(), T).unwrap();
}

// ---------------------------------------------------------- retrieve rounds

fn rounds(request_id: &str, rounds: &[(u32, u32, u32, u32)]) -> Event {
    Event::RetrieveRounds(RetrieveRoundsEvent {
        time_ms: T,
        tenant: String::new(),
        request_id: request_id.into(),
        rounds: rounds
            .iter()
            .enumerate()
            .map(|(round, &(input, read, write, output))| RoundUsage {
                round: round as u32,
                input_tokens: Some(input),
                cache_read_tokens: Some(read),
                cache_write_tokens: Some(write),
                output_tokens: Some(output),
            })
            .collect(),
    })
}

#[test]
fn extra_rounds_are_priced_from_what_each_round_was_billed() {
    let mut db = Db::new();
    db.write(vec![
        // no round
        turn(Turn {
            request_id: "zero",
            read: 9_000,
            ..Turn::default()
        }),
        // one round
        turn(Turn {
            request_id: "one",
            read: 21_000,
            rounds: 1,
            ..Turn::default()
        }),
        rounds("one", &[(5, 10_000, 1_000, 20), (5, 11_000, 200, 30)]),
        // three rounds
        turn(Turn {
            request_id: "three",
            read: 50_000,
            rounds: 3,
            ..Turn::default()
        }),
        rounds(
            "three",
            &[
                (5, 11_000, 100, 10),
                (5, 12_000, 100, 10),
                (5, 13_000, 100, 10),
                (5, 14_000, 100, 10),
            ],
        ),
        // two rounds from before per-round usage was recorded
        turn(Turn {
            request_id: "unmeasured",
            read: 30_000,
            rounds: 2,
            ..Turn::default()
        }),
        // a ping is not a client turn
        turn(Turn {
            request_id: "ping",
            read: 50_000,
            keep_warm: true,
            ..Turn::default()
        }),
    ]);
    let report = db
        .reader()
        .retrieve_round_cost("", 0, i64::MAX, None)
        .unwrap();
    let t = &report.totals;
    assert_eq!(t.client_turns, 4);
    assert_eq!(t.requests_with_rounds, 3);
    assert_eq!(t.extra_rounds_recorded, 6);
    assert_eq!((t.requests_measured, t.extra_rounds_measured), (2, 4));
    assert_eq!(t.coverage, Some(2.0 / 3.0));
    assert_eq!(t.extra_input, 20);
    assert_eq!(t.extra_uncached, 20, "Anthropic's input is the fresh part");
    assert_eq!(t.extra_cache_read, 50_000);
    assert_eq!(t.extra_cache_write, 500);
    assert_eq!(t.extra_output, 60);
    assert_eq!(t.first_round_cache_read, 21_000);
    assert_eq!(t.cache_read_per_extra_round, Some(12_500.0));
    // fresh at 1, Opus 5.5 reads at 0.05, five-minute writes at 1.25
    let cost = 20.0 + 50_000.0 * 0.05 + 500.0 * 1.25;
    assert!((t.extra_round_cost - cost).abs() < 1e-9);
    assert_eq!(t.cost_per_extra_round, Some(cost / 4.0));
    assert_eq!(t.cost_per_1000_turns, Some(cost * 1000.0 / 4.0));
    assert_eq!(t.extra_rounds_per_1000_turns, Some(1000.0));
    // the old estimate: each request's summed read shared across its rounds
    let assumed = 21_000.0 / 2.0 + 50_000.0 * 3.0 / 4.0 + 30_000.0 * 2.0 / 3.0;
    assert!((t.assumed_extra_cache_read - assumed).abs() < 1e-6);
    assert_eq!(report.groups.len(), 1);
    assert_eq!(report.groups[0].model, "claude-opus-5-5");
}

#[test]
fn with_no_round_taken_there_is_no_round_to_price() {
    let mut db = Db::new();
    db.write(vec![turn(Turn::default())]);
    let report = db
        .reader()
        .retrieve_round_cost("", 0, i64::MAX, None)
        .unwrap();
    assert_eq!(report.totals.client_turns, 1);
    assert_eq!(report.totals.extra_rounds_measured, 0);
    assert_eq!(report.totals.cost_per_extra_round, None);
    assert_eq!(report.totals.coverage, None);
    assert_eq!(report.totals.cost_per_1000_turns, Some(0.0));
}

#[test]
fn openai_rounds_are_billed_fresh_only_on_the_part_not_read() {
    let mut db = Db::new();
    db.write(vec![
        turn(Turn {
            request_id: "chat",
            family: "openai_chat",
            prompt: 22_000,
            read: 10_000,
            rounds: 1,
            ..Turn::default()
        }),
        rounds("chat", &[(10_000, 0, 0, 5), (12_000, 10_000, 0, 5)]),
    ]);
    let report = db
        .reader()
        .retrieve_round_cost("", 0, i64::MAX, None)
        .unwrap();
    assert_eq!(report.totals.extra_uncached, 2_000);
    assert!((report.totals.extra_round_cost - (2_000.0 + 10_000.0 * 0.1)).abs() < 1e-9);
}

// -------------------------------------------------------- fetch probability

fn shipped(time_ms: i64, request_id: &str, refs: &[(&str, &str, u64)]) -> Event {
    Event::Elisions(ElisionsEvent {
        time_ms,
        tenant: String::new(),
        request_id: request_id.into(),
        refs: refs
            .iter()
            .map(|&(reference, kind, bytes)| Elided {
                reference: reference.into(),
                kind: kind.into(),
                bytes,
            })
            .collect(),
    })
}

fn asked(reference: &str, withheld: bool) -> Event {
    Event::Retrieval(RetrievalEvent {
        context: None,
        time_ms: T + MIN,
        tenant: String::new(),
        request_id: Some("later".into()),
        surface: Surface::Proxy,
        reference: reference.into(),
        found: !withheld,
        expired: false,
        ranged: false,
        fragment_bytes: 0,
        fragment_chars: 0,
        fragment_tokens: 0,
        returned_chars: 0,
        returned_tokens: 0,
        withheld,
        range_status: None,
    })
}

#[test]
fn fetch_probability_is_split_by_kind_fragment_size_and_prompt_size() {
    let mut db = Db::new();
    db.write(vec![
        turn(Turn {
            request_id: "p1",
            read: 40_000,
            write: 5_000,
            ..Turn::default()
        }),
        // its row sums two rounds; the prompt that carried the handle is
        // round 0's
        turn(Turn {
            request_id: "p2",
            read: 300_000,
            rounds: 1,
            ..Turn::default()
        }),
        rounds("p2", &[(10, 120_000, 0, 5), (10, 180_000, 0, 5)]),
        shipped(
            T,
            "p1",
            &[
                ("k_aaaaaaaaaaaa", "log", 500),
                ("k_bbbbbbbbbbbb", "log", 2_000),
            ],
        ),
        shipped(T, "p2", &[("k_cccccccccccc", "json", 70_000)]),
        // shipped through the API: no proxied request carried it
        shipped(T, "api-1", &[("k_dddddddddddd", "json", 100)]),
        // shipped again later by another request: still p1's
        shipped(T + MIN, "p3", &[("k_aaaaaaaaaaaa", "log", 500)]),
        // outside the window
        shipped(T - 10 * MIN, "p0", &[("k_eeeeeeeeeeee", "log", 500)]),
        asked("k_aaaaaaaaaaaa", false),
        // asked by its bare body
        asked("bbbbbbbbbbbb", false),
        // refused by the budget: not a fetch
        asked("k_cccccccccccc", true),
        asked("k_dddddddddddd", false),
    ]);
    let report = db
        .reader()
        .fetch_probability("", T, T + 2 * MIN, None)
        .unwrap();
    assert_eq!((report.elisions, report.fetched), (4, 3));
    assert_eq!(report.probability, Some(0.75));
    let margin = |cells: &[crate::measured::FetchCell],
                  pick: &dyn Fn(&crate::measured::FetchCell) -> bool| {
        let cell = cells.iter().find(|c| pick(c)).expect("a margin row");
        (cell.elisions, cell.fetched, cell.probability)
    };
    assert_eq!(
        margin(&report.by_kind, &|c| c.kind == "log"),
        (2, 2, Some(1.0))
    );
    assert_eq!(
        margin(&report.by_kind, &|c| c.kind == "json"),
        (2, 1, Some(0.5))
    );
    assert_eq!(
        margin(&report.by_size, &|c| c.size_bucket == "<1KiB"),
        (2, 2, Some(1.0))
    );
    assert_eq!(
        margin(&report.by_size, &|c| c.size_bucket == "1-4KiB"),
        (1, 1, Some(1.0))
    );
    assert_eq!(
        margin(&report.by_size, &|c| c.size_bucket == ">=64KiB"),
        (1, 0, Some(0.0))
    );
    assert_eq!(
        margin(&report.by_size, &|c| c.size_bucket == "4-16KiB"),
        (0, 0, None)
    );
    assert_eq!(
        margin(&report.by_prefix, &|c| c.prefix_bucket == "10-50k"),
        (2, 2, Some(1.0))
    );
    assert_eq!(
        margin(&report.by_prefix, &|c| c.prefix_bucket == "100-200k"),
        (1, 0, Some(0.0)),
        "round 0's 120,010, not the row's summed 300,010"
    );
    assert_eq!(
        margin(&report.by_prefix, &|c| c.prefix_bucket == "unknown"),
        (1, 1, Some(1.0))
    );
    assert_eq!(report.cells.len(), 4);

    let first: String = db
        .writer
        .connection()
        .query_row(
            "SELECT request_id FROM elisions WHERE ref = 'k_aaaaaaaaaaaa'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(first, "p1", "the first request to ship a handle keeps it");
}

#[test]
fn a_discarded_rewrite_takes_its_elisions_with_it() {
    let mut db = Db::new();
    db.write(vec![
        shipped(T, "gone", &[("k_aaaaaaaaaaaa", "log", 500)]),
        shipped(T, "kept", &[("k_bbbbbbbbbbbb", "log", 500)]),
        Event::Discarded {
            tenant: String::new(),
            request_id: "gone".into(),
        },
    ]);
    let report = db
        .reader()
        .fetch_probability("", 0, i64::MAX, None)
        .unwrap();
    assert_eq!(report.elisions, 1);
    let held: i64 = db
        .writer
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM elisions WHERE request_id = ?1",
            params!["gone"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(held, 0);
}

#[test]
fn the_measurement_tables_are_pruned_with_everything_else() {
    let mut db = Db::new();
    db.write(vec![
        start(T - 10 * MIN),
        rounds("old", &[(1, 1, 1, 1), (1, 1, 1, 1)]),
        shipped(T - 10 * MIN, "old", &[("k_aaaaaaaaaaaa", "log", 500)]),
        start(T + MIN),
    ]);
    crate::retention::prune_on(db.writer.connection(), T).unwrap();
    let count = |table: &str| -> i64 {
        db.writer
            .connection()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    };
    assert_eq!(count("process_starts"), 1);
    assert_eq!(
        count("retrieve_round_usage"),
        2,
        "time_ms is T, not before it"
    );
    assert_eq!(count("elisions"), 0);
}

// ----------------------------------------------------------- the queries

/// The queries an operator runs by hand are the queries the reports run.
#[test]
fn scripts_db_queries_sql_carries_each_query_verbatim() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/db/queries.sql");
    let script = std::fs::read_to_string(&path).expect("scripts/db/queries.sql");
    let squash = |sql: &str| sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let script = squash(&script);
    for (name, sql) in [
        ("RESTARTS_SQL", RESTARTS_SQL),
        ("RETRIEVE_ROUNDS_SQL", RETRIEVE_ROUNDS_SQL),
        ("FETCH_PROBABILITY_SQL", FETCH_PROBABILITY_SQL),
    ] {
        assert!(
            script.contains(&format!("{};", squash(sql))),
            "{name} has drifted from scripts/db/queries.sql"
        );
    }
}
