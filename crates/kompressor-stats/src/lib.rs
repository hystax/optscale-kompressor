//! Append-only analytics for the compressor, and an honest savings report.
//!
//! Every surface records what it did — what it compressed, what it retrieved,
//! what it was not allowed to touch, what the provider billed — into one
//! SQLite file, off the request path. Reports are then plain SQL over those
//! rows.
//!
//! # Two things this crate is careful about
//!
//! **Recording is never allowed to cost a request.** [`StatsSink::send`] cannot
//! block and cannot fail: it pushes onto a bounded queue and, if that queue is
//! full, drops the event and increments a counter. One thread owns the
//! connection and commits a batch every [`DEFAULT_FLUSH_INTERVAL`] or every
//! [`DEFAULT_BATCH_MAX`] events. A lost row makes a report slightly wrong; a
//! blocked compression makes the product slightly broken, and the first is
//! always the better failure.
//!
//! **Savings are reported net, not gross.** Gross savings —
//! `Σ(tokens_in - tokens_out)` — ignore that in
//! [`Mode::Recoverable`](kompressor_engine::types::Mode::Recoverable) every
//! retrieval re-injects a fragment the model pays for again. [`StatsReader`]
//! reports both, and net can come out negative. The reasoning, and the
//! over-counting that net still does not fix, are written out in the [`reader`]
//! module documentation; anything that renders these numbers should read it
//! before deciding which one to put in large type.
//!
//! # Example
//!
//! ```no_run
//! use kompressor_engine::types::{Kind, Mode};
//! use kompressor_stats::{spawn, Event, OptimizationEvent, StatsConfig, StatsReader, Surface};
//!
//! let config = StatsConfig::at("/var/lib/kompressor/stats.db");
//! let (sink, handle) = spawn(&config)?;
//!
//! let mut event = OptimizationEvent::new(1_700_000_000_000, "acme", "req-1", Surface::Api, Mode::Recoverable);
//! event.tokens_in = 9_000;
//! event.tokens_out = 3_000;
//! event.record_type(Kind::Json, 2);
//! sink.send(Event::Optimization(event));
//!
//! sink.flush()?;
//! let reader = StatsReader::open(&config.path)?;
//! let summary = reader.summary("acme", 0, i64::MAX)?;
//! println!("net {} (gross {})", summary.saved_net, summary.saved_gross);
//! handle.shutdown()?;
//! # Ok::<(), kompressor_stats::StatsError>(())
//! ```

#![forbid(unsafe_code)]

pub mod config;
pub mod error;
pub mod events;
pub mod reader;
pub mod retention;
pub mod schema;
pub mod writer;

pub use config::{
    DEFAULT_BATCH_MAX, DEFAULT_CHANNEL_CAPACITY, DEFAULT_FLUSH_INTERVAL, DEFAULT_RETENTION_DAYS,
    StatsConfig,
};
pub use error::StatsError;
pub use events::{
    Event, OptimizationEvent, ProtectedRefEvent, ProxyRequestEvent, RetrievalEvent, StoreSample,
    Surface,
};
pub use reader::{
    Bucket, BucketRow, DailyRollup, ModeTotals, ProviderCoverage, ProxyRow, RefCount,
    RequestEconomy, RetrievalSummary, SessionOutcome, StatsReader, Summary, SurfaceTotals,
    TypeCoverage,
};
pub use retention::PruneCounts;
pub use writer::{StatsHandle, StatsSink, StatsWriter, null_sink, spawn};

#[cfg(test)]
mod tests {
    use super::*;
    use kompressor_engine::types::{Kind, Mode};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;
    use tempfile::TempDir;

    /// 2023-11-14T22:13:20Z, so day arithmetic in the tests is checkable by eye.
    const T0: i64 = 1_700_000_000_000;
    const DAY0: &str = "2023-11-14";
    const HOUR: i64 = 3_600_000;
    const DAY: i64 = 86_400_000;

    struct Harness {
        dir: TempDir,
        sink: StatsSink,
        handle: Option<StatsHandle>,
    }

    impl Harness {
        fn new() -> Harness {
            Harness::with_config(|_| {})
        }

        fn with_config(tweak: impl FnOnce(&mut StatsConfig)) -> Harness {
            let dir = tempfile::tempdir().expect("a temp dir");
            let mut config = StatsConfig::at(dir.path().join("nested").join("stats.db"));
            tweak(&mut config);
            let (sink, handle) = spawn(&config).expect("the writer starts");
            Harness {
                dir,
                sink,
                handle: Some(handle),
            }
        }

        fn path(&self) -> PathBuf {
            self.dir.path().join("nested").join("stats.db")
        }

        fn write(&self, events: Vec<Event>) {
            for event in events {
                self.sink.send(event);
            }
            self.sink.flush().expect("the batch commits");
        }

        fn reader(&self) -> StatsReader {
            StatsReader::open(self.path()).expect("the database opens read-only")
        }

        fn shutdown(&mut self) {
            if let Some(handle) = self.handle.take() {
                handle.shutdown().expect("the writer stops cleanly");
            }
        }
    }

    fn optimization(
        time_ms: i64,
        tenant: &str,
        request_id: &str,
        tokens_in: u32,
        tokens_out: u32,
    ) -> Event {
        let mut event = OptimizationEvent::new(
            time_ms,
            tenant,
            request_id,
            Surface::Proxy,
            Mode::Recoverable,
        );
        event.tokens_in = tokens_in;
        event.tokens_out = tokens_out;
        event.chars_in = u64::from(tokens_in) * 4;
        event.chars_out = u64::from(tokens_out) * 4;
        event.messages_total = 3;
        event.messages_compressed = 1;
        event.messages_skipped = 2;
        event.record_type(Kind::Json, 1);
        Event::Optimization(event)
    }

    fn retrieval(
        time_ms: i64,
        tenant: &str,
        request_id: Option<&str>,
        reference: &str,
        found: bool,
        returned_tokens: u32,
    ) -> Event {
        Event::Retrieval(RetrievalEvent {
            time_ms,
            tenant: tenant.to_string(),
            request_id: request_id.map(str::to_string),
            surface: Surface::Proxy,
            reference: reference.to_string(),
            found,
            expired: false,
            ranged: false,
            fragment_bytes: u64::from(returned_tokens) * 4,
            fragment_chars: u64::from(returned_tokens) * 4,
            fragment_tokens: returned_tokens,
            returned_chars: u64::from(returned_tokens) * 4,
            returned_tokens,
            withheld: false,
        })
    }

    /// An ask the budget refused: no store read, so no sizes.
    fn withheld(time_ms: i64, tenant: &str, request_id: Option<&str>, reference: &str) -> Event {
        match retrieval(time_ms, tenant, request_id, reference, false, 0) {
            Event::Retrieval(mut event) => {
                event.withheld = true;
                Event::Retrieval(event)
            }
            other => other,
        }
    }

    fn proxy_in(time_ms: i64, tenant: &str, request_id: &str, conversation: &str) -> Event {
        match proxy(time_ms, tenant, request_id) {
            Event::ProxyRequest(mut event) => {
                event.conversation = Some(conversation.to_string());
                Event::ProxyRequest(event)
            }
            other => other,
        }
    }

    fn proxy(time_ms: i64, tenant: &str, request_id: &str) -> Event {
        Event::ProxyRequest(ProxyRequestEvent {
            time_ms,
            tenant: tenant.to_string(),
            request_id: request_id.to_string(),
            api_family: "anthropic".to_string(),
            model: Some("claude-x".to_string()),
            upstream: "https://upstream.invalid".to_string(),
            status: Some(200),
            stream: true,
            strip_usage: false,
            prompt_tokens: Some(4_200),
            completion_tokens: Some(180),
            cache_read_tokens: Some(3_000),
            cache_write_tokens: None,
            cache_write_1h_tokens: None,
            shadow_prompt_tokens: Some(9_000),
            conversation: None,
            retrieve_rounds: 2,
            retrieve_tokens_injected: 1_500,
            upstream_ms: Some(812.5),
            total_ms: Some(840.0),
            error: None,
            keep_warm: false,
        })
    }

    #[test]
    fn an_event_sent_through_the_sink_is_readable_after_flush() {
        let harness = Harness::new();
        harness.write(vec![optimization(T0, "acme", "req-1", 1_000, 400)]);

        let summary = harness.reader().summary("acme", 0, i64::MAX).unwrap();
        assert_eq!(summary.requests, 1);
        assert_eq!(summary.tokens_in, 1_000);
        assert_eq!(summary.tokens_out, 400);
        assert_eq!(summary.saved_gross, 600);
        assert_eq!(summary.saved_net, 600);
    }

    #[test]
    fn the_schema_applies_from_empty_and_applying_it_again_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");

        let conn = schema::open_read_write(&path).unwrap();
        assert_eq!(schema::apply(&conn).unwrap(), schema::latest_version());

        let objects = |conn: &rusqlite::Connection| -> Vec<String> {
            let mut stmt = conn
                .prepare("SELECT type || ':' || name FROM sqlite_master ORDER BY 1")
                .unwrap();
            let rows = stmt.query_map([], |row| row.get::<_, String>(0)).unwrap();
            rows.map(Result::unwrap).collect()
        };
        let before = objects(&conn);

        // a second migration run, and a second open, must both be no-ops
        assert_eq!(schema::apply(&conn).unwrap(), schema::latest_version());
        let reopened = schema::open_read_write(&path).unwrap();
        assert_eq!(objects(&reopened), before);
        assert!(before.iter().any(|o| o == "table:optimization_events"));
        assert!(before.iter().any(|o| o == "view:v_request_economy"));
    }

    /// A database already carrying V1 data has to reach the latest version
    /// *and keep its rows*. The ladder is the only thing between an operator's
    /// recorded history and a fresh empty file.
    #[test]
    fn an_existing_v1_database_upgrades_in_place_without_losing_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");

        // build a V1 file the way a older build would have left one
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(schema::MIGRATIONS[0].1).unwrap();
            conn.execute_batch("PRAGMA user_version = 1").unwrap();
            conn.execute(
                "INSERT INTO optimization_events (time_ms, tenant, request_id, surface, mode,
                    tokens_in, tokens_out, chars_in, chars_out, messages_total,
                    messages_compressed, messages_skipped, refs_emitted, ref_bytes_emitted,
                    content_types, protected_by_reason, engine_ms)
                 VALUES (1, '', 'r1', 'proxy', 'recoverable', 10, 5, 40, 20, 1, 1, 0, 0, 0,
                    '{}', '{}', 1.0)",
                [],
            )
            .unwrap();
        }

        let conn = schema::open_read_write(&path).unwrap();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
                .unwrap(),
            schema::latest_version()
        );
        let rows: u32 = conn
            .query_row("SELECT count(*) FROM optimization_events", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 1, "the recorded event survived the upgrade");
        // the new column exists and reads as unrecorded for the old row
        let unrecorded: u32 = conn
            .query_row(
                "SELECT count(*) FROM v_outcomes WHERE outcome = 'unrecorded'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            unrecorded, 1,
            "a row written before the column existed is named, not folded into a real outcome"
        );
    }

    /// A rewrite the proxy threw away must stop counting as a saving — in the
    /// views that answer "what did we save", while staying visible in the one
    /// that answers "how often does this happen".
    #[test]
    fn a_discarded_rewrite_leaves_the_savings_views_but_not_the_outcome_view() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        let conn = schema::open_read_write(&path).unwrap();
        for (request, tokens_out) in [("kept", 50), ("thrown", 90)] {
            conn.execute(
                "INSERT INTO optimization_events (time_ms, tenant, request_id, surface, mode,
                    tokens_in, tokens_out, chars_in, chars_out, messages_total,
                    messages_compressed, messages_skipped, refs_emitted, ref_bytes_emitted,
                    content_types, protected_by_reason, engine_ms, outcome)
                 VALUES (1000, '', ?1, 'proxy', 'recoverable', 100, ?2, 400, 200, 1, 1, 0, 0, 0,
                    '{}', '{}', 1.0, 'compressed')",
                rusqlite::params![request, tokens_out],
            )
            .unwrap();
        }
        conn.execute(
            "UPDATE optimization_events SET outcome = 'discarded' WHERE request_id = 'thrown'",
            [],
        )
        .unwrap();

        let saved: i64 = conn
            .query_row("SELECT sum(saved_gross) FROM v_daily_savings", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(saved, 50, "only the rewrite that was actually sent counts");
        let economy: i64 = conn
            .query_row("SELECT count(*) FROM v_request_economy", [], |r| r.get(0))
            .unwrap();
        assert_eq!(economy, 1, "the discarded request has no economy to report");
        let outcomes: i64 = conn
            .query_row(
                "SELECT requests FROM v_outcomes WHERE outcome = 'discarded'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(outcomes, 1, "but how often it happens stays visible");
    }

    #[test]
    fn a_v4_database_gains_the_v5_columns_without_losing_its_rows() {
        // The ladder is applied to databases that already hold a run's worth of
        // data, so the interesting question is not "does V5 parse" but "does an
        // existing v4 file come out the other side intact and queryable". Both
        // halves of V5 are additive for exactly that reason: the columns are
        // nullable and the rewritten `v_skip_reasons` COALESCEs the token side,
        // so rows written before this migration still answer the count question
        // they always did.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let ladder: Vec<&(u32, &str)> = schema::MIGRATIONS
            .iter()
            .filter(|(version, _)| *version <= 4)
            .collect();
        for (version, sql) in &ladder {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", *version).unwrap();
        }
        conn.execute(
            "INSERT INTO optimization_events (time_ms, tenant, request_id, surface, mode,
                 tokens_in, tokens_out, chars_in, chars_out, messages_total,
                 messages_compressed, messages_skipped, refs_emitted, ref_bytes_emitted,
                 content_types, protected_by_reason, engine_ms, skipped_by_reason)
             VALUES (1,'acme','req-old','proxy','recoverable',100,40,400,160,3,1,2,0,0,
                     '{}','{}',1.0,'{\"opaque\":1,\"below_min_chars\":1}')",
            [],
        )
        .unwrap();

        let reached = schema::apply(&conn).unwrap();
        assert_eq!(reached, schema::latest_version());

        let kept: i64 = conn
            .query_row("SELECT COUNT(*) FROM optimization_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, 1, "the pre-existing row survives the migration");

        // The old row has no token side; it must still answer the count
        // question rather than vanish from the view or error.
        let (messages, tokens): (i64, i64) = conn
            .query_row(
                "SELECT messages, tokens FROM v_skip_reasons WHERE reason = 'opaque'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(messages, 1);
        assert_eq!(
            tokens, 0,
            "unweighted history reads as zero, not as an error"
        );

        // And the new columns exist to be written.
        conn.execute(
            "UPDATE optimization_events SET skipped_tokens_by_reason = '{\"opaque\":0}'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO proxy_requests (time_ms, tenant, request_id, api_family,
                 upstream, stream, retrieve_rounds, retrieve_tokens_injected, conversation)
             VALUES (1,'acme','req-old','anthropic_messages','x',0,0,0,'00000000deadbeef')",
            [],
        )
        .unwrap();
        let turns: i64 = conn
            .query_row(
                "SELECT turns FROM v_conversations WHERE conversation = '00000000deadbeef'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(turns, 1);
    }

    #[test]
    fn the_migration_ladder_is_ordered_numerically_and_has_no_duplicates() {
        let mut seen = 0u32;
        for (version, _) in schema::MIGRATIONS {
            assert!(
                *version > seen,
                "migration {version} does not follow {seen}: the ladder must be strictly increasing"
            );
            seen = *version;
        }
        assert_eq!(schema::latest_version(), seen);
    }

    #[test]
    fn a_full_channel_drops_the_event_instead_of_blocking_and_counts_the_drop() {
        // the receiver is held and never drained, so the queue fills and stays full
        let (sink, _receiver) = writer::detached_sink(2);
        for i in 0..5 {
            sink.send(optimization(T0, "acme", &format!("req-{i}"), 10, 1));
        }
        assert_eq!(
            sink.dropped(),
            3,
            "two fit in the queue, three were dropped"
        );
    }

    #[test]
    fn summary_reports_gross_retrieved_and_net() {
        let harness = Harness::new();
        harness.write(vec![
            optimization(T0, "acme", "req-1", 10_000, 2_000),
            retrieval(
                T0 + 10,
                "acme",
                Some("req-1"),
                "k_aaaaaaaaaaaa",
                true,
                1_500,
            ),
            retrieval(T0 + 20, "acme", Some("req-1"), "k_bbbbbbbbbbbb", true, 500),
        ]);

        let summary = harness.reader().summary("acme", 0, i64::MAX).unwrap();
        assert_eq!(summary.saved_gross, 8_000);
        assert_eq!(summary.retrieved_tokens, 2_000);
        assert_eq!(summary.saved_net, 6_000);
        assert_eq!(summary.retrievals, 2);
        assert_eq!(summary.retrieval_hits, 2);
        assert!((summary.hit_rate - 1.0).abs() < f64::EPSILON);
        assert_eq!(summary.by_mode.len(), 1);
        assert_eq!(summary.by_mode[0].mode, "recoverable");
        assert_eq!(summary.by_mode[0].saved_gross, 8_000);
        assert_eq!(summary.by_surface[0].surface, "proxy");
    }

    #[test]
    fn net_goes_negative_when_retrieval_gives_back_more_than_compression_saved() {
        let harness = Harness::new();
        // the agent loop: one small saving, then the same fragment re-injected
        // round after round
        let mut events = vec![optimization(T0, "acme", "req-1", 8_000, 1_000)];
        for round in 0..10 {
            events.push(retrieval(
                T0 + round,
                "acme",
                Some("req-1"),
                "k_cccccccccccc",
                true,
                6_700,
            ));
        }
        harness.write(events);

        let summary = harness.reader().summary("acme", 0, i64::MAX).unwrap();
        assert_eq!(summary.saved_gross, 7_000);
        assert_eq!(summary.retrieved_tokens, 67_000);
        assert_eq!(summary.saved_net, -60_000);
        assert!(
            summary.saved_net < 0,
            "the reader must report a negative net"
        );
    }

    #[test]
    fn only_a_retrieval_that_found_its_fragment_is_a_debit() {
        let harness = Harness::new();
        harness.write(vec![
            optimization(T0, "acme", "req-1", 5_000, 1_000),
            retrieval(T0 + 1, "acme", Some("req-1"), "k_dddddddddddd", true, 300),
            // a miss returns nothing, so it costs nothing
            retrieval(T0 + 2, "acme", Some("req-1"), "k_eeeeeeeeeeee", false, 900),
        ]);

        let summary = harness.reader().summary("acme", 0, i64::MAX).unwrap();
        assert_eq!(summary.retrievals, 2);
        assert_eq!(summary.retrieval_hits, 1);
        assert_eq!(summary.retrieved_tokens, 300);
        assert_eq!(summary.saved_net, 4_000 - 300);

        let request = harness.reader().request("acme", "req-1").unwrap().unwrap();
        assert_eq!(request.retrieved_tokens, 300);
    }

    #[test]
    fn the_hourly_histogram_buckets_by_hour_and_sums_to_the_summary() {
        let harness = Harness::new();
        harness.write(vec![
            optimization(T0 - 2 * HOUR, "acme", "req-1", 4_000, 1_000),
            optimization(T0, "acme", "req-2", 6_000, 2_000),
            retrieval(T0, "acme", Some("req-2"), "k_ffffffffffff", true, 900),
        ]);
        let reader = harness.reader();

        let rows = reader.histogram("acme", 0, i64::MAX, Bucket::Hour).unwrap();
        assert_eq!(rows.len(), 2, "two distinct hours");
        assert_eq!(rows[1].bucket_ms - rows[0].bucket_ms, 2 * HOUR);
        assert_eq!(rows[0].bucket_ms % HOUR, 0, "buckets start on the hour");
        assert!(
            rows[0].label.ends_with(":00:00Z"),
            "label: {}",
            rows[0].label
        );
        assert_eq!(rows[1].retrieved_tokens, 900);
        assert_eq!(rows[1].saved_net, 4_000 - 900);

        let summary = reader.summary("acme", 0, i64::MAX).unwrap();
        assert_eq!(
            rows.iter().map(|r| r.requests).sum::<u64>(),
            summary.requests
        );
        assert_eq!(
            rows.iter().map(|r| r.saved_gross).sum::<i64>(),
            summary.saved_gross
        );
        assert_eq!(
            rows.iter().map(|r| r.retrieved_tokens).sum::<i64>(),
            summary.retrieved_tokens
        );
        assert_eq!(
            rows.iter().map(|r| r.saved_net).sum::<i64>(),
            summary.saved_net
        );
    }

    #[test]
    fn the_daily_histogram_buckets_by_day_and_sums_to_the_summary() {
        let harness = Harness::new();
        harness.write(vec![
            optimization(T0, "acme", "req-1", 4_000, 1_000),
            optimization(T0 + DAY, "acme", "req-2", 9_000, 3_000),
            retrieval(
                T0 + DAY,
                "acme",
                Some("req-2"),
                "k_gggggggggggg",
                true,
                2_000,
            ),
        ]);
        let reader = harness.reader();

        let rows = reader.histogram("acme", 0, i64::MAX, Bucket::Day).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, DAY0);
        assert_eq!(rows[1].bucket_ms - rows[0].bucket_ms, DAY);
        assert_eq!(rows[0].bucket_ms % DAY, 0, "buckets start at UTC midnight");

        let summary = reader.summary("acme", 0, i64::MAX).unwrap();
        assert_eq!(
            rows.iter().map(|r| r.saved_gross).sum::<i64>(),
            summary.saved_gross
        );
        assert_eq!(
            rows.iter().map(|r| r.saved_net).sum::<i64>(),
            summary.saved_net
        );
    }

    #[test]
    fn the_histogram_window_excludes_its_upper_bound() {
        let harness = Harness::new();
        harness.write(vec![
            optimization(T0, "acme", "req-1", 100, 10),
            optimization(T0 + DAY, "acme", "req-2", 100, 10),
        ]);
        let reader = harness.reader();

        let rows = reader.histogram("acme", T0, T0 + DAY, Bucket::Day).unwrap();
        assert_eq!(rows.len(), 1, "the row exactly at `to_ms` is outside");
        assert_eq!(reader.summary("acme", T0, T0 + DAY).unwrap().requests, 1);
    }

    #[test]
    fn a_bucket_before_the_epoch_floors_downwards_rather_than_towards_zero() {
        let harness = Harness::new();
        // a container that started before NTP ran stamps events in 1970
        harness.write(vec![optimization(-1, "acme", "req-1", 100, 10)]);

        let rows = harness
            .reader()
            .histogram("acme", i64::MIN / 2, i64::MAX, Bucket::Hour)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].bucket_ms, -HOUR,
            "truncation towards zero would have filed this in the bucket after it"
        );
    }

    #[test]
    fn by_type_unfolds_the_json_content_type_map() {
        let harness = Harness::new();
        let mut first = OptimizationEvent::new(T0, "acme", "req-1", Surface::Api, Mode::Aggressive);
        first.record_type(Kind::Json, 2);
        first.record_type(Kind::Log, 1);
        let mut second =
            OptimizationEvent::new(T0 + 1, "acme", "req-2", Surface::Api, Mode::Aggressive);
        second.record_type(Kind::Json, 3);
        second.record_type(Kind::SourceCode, 1);
        harness.write(vec![first.into(), second.into()]);

        let types = harness.reader().by_type("acme", 0, i64::MAX).unwrap();
        let by_name: BTreeMap<&str, i64> = types
            .iter()
            .map(|t| (t.content_type.as_str(), t.messages))
            .collect();
        assert_eq!(by_name.get("json"), Some(&5));
        assert_eq!(by_name.get("log"), Some(&1));
        assert_eq!(by_name.get("source_code"), Some(&1));
        assert_eq!(types[0].content_type, "json", "busiest type first");
        assert!((types.iter().map(|t| t.share).sum::<f64>() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn retrievals_separates_misses_from_expiries_and_counts_ranged_reads() {
        let harness = Harness::new();
        let mut expired = match retrieval(T0, "acme", None, "k_hhhhhhhhhhhh", false, 0) {
            Event::Retrieval(e) => e,
            _ => unreachable!(),
        };
        expired.expired = true;
        let mut ranged = match retrieval(T0 + 1, "acme", None, "k_iiiiiiiiiiii", true, 120) {
            Event::Retrieval(e) => e,
            _ => unreachable!(),
        };
        ranged.ranged = true;
        harness.write(vec![
            Event::Retrieval(expired),
            Event::Retrieval(ranged),
            retrieval(T0 + 2, "acme", None, "k_iiiiiiiiiiii", true, 80),
            retrieval(T0 + 3, "acme", None, "k_jjjjjjjjjjjj", false, 0),
        ]);

        let report = harness.reader().retrievals("acme", 0, i64::MAX).unwrap();
        assert_eq!(report.retrievals, 4);
        assert_eq!(report.hits, 2);
        assert_eq!(report.misses, 2);
        assert_eq!(report.expiries, 1, "one miss was a fragment we let go");
        assert_eq!(report.ranged, 1);
        assert!((report.ranged_share - 0.25).abs() < 1e-9);
        assert!((report.hit_rate - 0.5).abs() < 1e-9);
        assert_eq!(report.retrieved_tokens, 200);
        assert_eq!(report.top_refs[0].reference, "k_iiiiiiiiiiii");
        assert_eq!(report.top_refs[0].retrievals, 2);
        assert_eq!(report.top_refs[0].returned_tokens, 200);
        assert_eq!(report.withheld, 0, "nothing was refused here");
    }

    /// The report that sent me looking for this printed `hits 7 (100.0%), 0
    /// missed` for a connection the budget had turned away eleven times. Every
    /// rate is computed over the asks that reached the store, so a refusal must
    /// not enter one -- and must still be counted, or the budget is the one
    /// setting with no evidence of it binding.
    #[test]
    fn a_withheld_ask_is_counted_without_entering_any_rate() {
        let harness = Harness::new();
        harness.write(vec![
            retrieval(T0, "acme", Some("req-1"), "k_mmmmmmmmmmmm", true, 100),
            withheld(T0 + 1, "acme", Some("req-1"), "k_nnnnnnnnnnnn"),
            withheld(T0 + 2, "acme", Some("req-1"), "k_nnnnnnnnnnnn"),
            withheld(T0 + 3, "acme", Some("req-1"), "k_oooooooooooo"),
        ]);

        let report = harness.reader().retrievals("acme", 0, i64::MAX).unwrap();
        assert_eq!(report.retrievals, 1, "three asks never reached the store");
        assert_eq!(report.hits, 1);
        assert_eq!(
            report.misses, 0,
            "a refusal is not the store failing to answer"
        );
        assert_eq!(report.expiries, 0);
        assert!(
            (report.hit_rate - 1.0).abs() < 1e-9,
            "the one ask that was served did hit, and the rate says so"
        );
        assert_eq!(report.withheld, 3, "and the refusals are still counted");

        // Per handle, so an operator can see which fragments the budget keeps
        // turning away. `k_nnnn` was never served and would otherwise not
        // appear at all.
        let refused: Vec<_> = report
            .top_refs
            .iter()
            .filter(|r| r.withheld > 0)
            .map(|r| (r.reference.as_str(), r.retrievals, r.withheld))
            .collect();
        assert_eq!(
            refused,
            vec![("k_nnnnnnnnnnnn", 0, 2), ("k_oooooooooooo", 0, 1)],
            "a handle only ever refused is still listed, with no asks against it"
        );
    }

    /// Retrieval feeds the net saving, and a refusal returned nothing. If it
    /// ever reached that arithmetic it would debit the saving by content the
    /// model never received.
    #[test]
    fn a_withheld_ask_debits_nothing() {
        let harness = Harness::new();
        harness.write(vec![
            optimization(T0, "acme", "req-1", 10_000, 2_000),
            withheld(T0 + 1, "acme", Some("req-1"), "k_pppppppppppp"),
        ]);
        let summary = harness.reader().summary("acme", 0, i64::MAX).unwrap();
        assert_eq!(summary.saved_gross, 8_000);
        assert_eq!(summary.retrieved_tokens, 0);
        assert_eq!(
            summary.saved_net, 8_000,
            "nothing came back, so nothing is owed"
        );
    }

    /// The reader opens read-only and never migrates, so it meets databases
    /// the writer has not upgraded yet — which is every installation between
    /// an upgrade and the next server restart. Selecting `withheld`
    /// unconditionally made every retrieval query fail with `no such column`
    /// on a v9 database. Measured on a real one before this test existed.
    #[test]
    fn retrievals_still_read_on_a_database_that_predates_the_withheld_column() {
        let dir = tempfile::tempdir().unwrap();
        let config = StatsConfig::at(dir.path().join("stats.db"));
        {
            let mut writer = StatsWriter::open(&config).unwrap();
            writer
                .write_batch(&[
                    retrieval(T0, "acme", Some("req-1"), "k_qqqqqqqqqqqq", true, 100),
                    retrieval(T0 + 1, "acme", Some("req-1"), "k_rrrrrrrrrrrr", false, 0),
                ])
                .unwrap();
            // Back to the shape a v9 database has. The view goes first: it
            // reads the column.
            writer
                .connection()
                .execute_batch(
                    "DROP VIEW IF EXISTS v_retrieval_hit_rate;
                     ALTER TABLE retrieval_events DROP COLUMN withheld;
                     PRAGMA user_version = 9;",
                )
                .unwrap();
        }

        let reader = StatsReader::open(&config.path).unwrap();
        let report = reader.retrievals("acme", 0, i64::MAX).unwrap();
        assert_eq!(report.retrievals, 2, "the old rows still count");
        assert_eq!(report.hits, 1);
        assert_eq!(report.misses, 1);
        assert!((report.hit_rate - 0.5).abs() < 1e-9);
        assert_eq!(
            report.withheld, 0,
            "a database that could not record a refusal reports none, which is true"
        );
        assert_eq!(report.top_refs.len(), 2);
    }

    #[test]
    fn every_query_isolates_tenants() {
        let harness = Harness::new();
        harness.write(vec![
            optimization(T0, "acme", "req-1", 10_000, 2_000),
            retrieval(T0, "acme", Some("req-1"), "k_kkkkkkkkkkkk", true, 100),
            proxy(T0, "acme", "req-1"),
            optimization(T0, "globex", "req-1", 500, 400),
            retrieval(T0, "globex", Some("req-1"), "k_llllllllllll", true, 7_000),
            proxy(T0, "globex", "req-1"),
        ]);
        let reader = harness.reader();

        let acme = reader.summary("acme", 0, i64::MAX).unwrap();
        assert_eq!(acme.requests, 1);
        assert_eq!(acme.saved_gross, 8_000);
        assert_eq!(acme.retrieved_tokens, 100);

        let globex = reader.summary("globex", 0, i64::MAX).unwrap();
        assert_eq!(globex.saved_gross, 100);
        assert_eq!(globex.saved_net, 100 - 7_000);

        assert_eq!(
            reader
                .histogram("acme", 0, i64::MAX, Bucket::Day)
                .unwrap()
                .iter()
                .map(|r| r.saved_gross)
                .sum::<i64>(),
            8_000
        );
        assert_eq!(
            reader.by_type("globex", 0, i64::MAX).unwrap()[0].messages,
            1
        );
        assert_eq!(reader.retrievals("acme", 0, i64::MAX).unwrap().hits, 1);
        assert_eq!(
            reader.retrievals("acme", 0, i64::MAX).unwrap().top_refs[0].reference,
            "k_kkkkkkkkkkkk"
        );

        // the same request id under two tenants must not blend
        let acme_request = reader.request("acme", "req-1").unwrap().unwrap();
        assert_eq!(acme_request.saved_gross, 8_000);
        assert_eq!(acme_request.retrieved_tokens, 100);
        assert!(reader.request("initech", "req-1").unwrap().is_none());
    }

    #[test]
    fn a_keep_warm_ping_is_written_as_one() {
        let harness = Harness::new();
        let ping = match proxy(T0 + 1, "acme", "req-1.keepwarm1") {
            Event::ProxyRequest(mut row) => {
                row.keep_warm = true;
                Event::ProxyRequest(row)
            }
            other => other,
        };
        harness.write(vec![proxy(T0, "acme", "req-1"), ping]);
        let conn = rusqlite::Connection::open(harness.path()).unwrap();
        let flags: Vec<(String, i64)> = conn
            .prepare("SELECT request_id, keep_warm FROM proxy_requests ORDER BY time_ms")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            flags,
            vec![("req-1".to_string(), 0), ("req-1.keepwarm1".to_string(), 1)]
        );
    }

    #[test]
    fn request_joins_optimization_retrieval_and_proxy_rows_on_request_id() {
        let harness = Harness::new();
        harness.write(vec![
            optimization(T0, "acme", "req-1", 12_000, 4_000),
            // the agent loop compresses the same request again next round
            optimization(T0 + 50, "acme", "req-1", 13_000, 5_000),
            retrieval(
                T0 + 60,
                "acme",
                Some("req-1"),
                "k_mmmmmmmmmmmm",
                true,
                2_500,
            ),
            retrieval(T0 + 61, "acme", Some("req-1"), "k_nnnnnnnnnnnn", false, 0),
            retrieval(
                T0 + 62,
                "acme",
                Some("req-2"),
                "k_oooooooooooo",
                true,
                9_999,
            ),
            proxy(T0, "acme", "req-1"),
        ]);

        let request = harness.reader().request("acme", "req-1").unwrap().unwrap();
        assert_eq!(request.compressions, 2);
        assert_eq!(request.saved_gross, 8_000 + 8_000);
        assert_eq!(
            request.retrieved_tokens, 2_500,
            "req-2's retrieval is not ours"
        );
        assert_eq!(request.saved_net, 13_500);
        assert_eq!(request.retrievals, 2);
        assert_eq!(request.retrieval_hits, 1);
        assert_eq!(request.mode.as_deref(), Some("recoverable"));
        assert_eq!(request.surface.as_deref(), Some("proxy"));
        assert_eq!(request.time_ms, T0);

        let proxy_row = request.proxy.expect("the proxy row joined");
        assert_eq!(proxy_row.api_family, "anthropic");
        assert_eq!(proxy_row.prompt_tokens, Some(4_200));
        assert_eq!(proxy_row.cache_read_tokens, Some(3_000));
        assert_eq!(proxy_row.retrieve_rounds, 2);
        assert!(proxy_row.stream);
        assert_eq!(proxy_row.status, Some(200));
    }

    #[test]
    fn request_reports_a_proxied_request_that_was_never_compressed() {
        let harness = Harness::new();
        harness.write(vec![proxy(T0, "acme", "req-9")]);

        let request = harness.reader().request("acme", "req-9").unwrap().unwrap();
        assert_eq!(request.compressions, 0);
        assert_eq!(request.saved_gross, 0);
        assert!(request.mode.is_none());
        assert!(request.proxy.is_some());
        assert!(
            harness
                .reader()
                .request("acme", "nothing")
                .unwrap()
                .is_none(),
            "an id no table has ever seen is None, not a zero row"
        );
    }

    #[test]
    fn protected_ref_events_record_the_handles_and_their_count() {
        let harness = Harness::new();
        harness.write(vec![Event::ProtectedRefs(ProtectedRefEvent {
            time_ms: T0,
            tenant: "acme".to_string(),
            request_id: Some("req-1".to_string()),
            refs: vec!["k_pppppppppppp".to_string(), "k_qqqqqqqqqqqq".to_string()],
            reasons: vec!["frozen".to_string(), "role:assistant".to_string()],
            messages: 2,
            touched: 2,
        })]);

        let conn = schema::open_read_only(&harness.path()).unwrap();
        let (refs, total, reasons): (String, i64, String) = conn
            .query_row(
                "SELECT refs, refs_total, reasons FROM protected_ref_events",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            total, 2,
            "refs_total is derived from the array, not trusted"
        );
        assert!(refs.contains("k_pppppppppppp"));
        assert!(reasons.contains("role:assistant"));
    }

    #[test]
    fn store_returns_the_latest_sample() {
        let harness = Harness::new();
        harness.write(vec![
            Event::StoreSample {
                time_ms: T0,
                live_refs: 10,
                live_bytes: 1_000,
            },
            Event::StoreSample {
                time_ms: T0 + 60_000,
                live_refs: 12,
                live_bytes: 2_048,
            },
        ]);

        let sample = harness.reader().store("acme").unwrap().expect("a sample");
        assert_eq!(sample.time_ms, T0 + 60_000);
        assert_eq!(sample.live_refs, 12);
        assert_eq!(sample.live_bytes, 2_048);
    }

    #[test]
    fn scheduled_retention_does_not_count_manually_rolled_rows_twice() {
        let mut harness = Harness::new();
        harness.write(vec![
            optimization(T0, "acme", "old", 100, 20),
            retrieval(T0 + 1, "acme", Some("old"), "k_rrrrrrrrrrrr", true, 5),
        ]);
        harness.shutdown();
        let mut writer = StatsWriter::open(&StatsConfig::at(harness.path())).unwrap();
        writer.rollup(DAY0).unwrap();
        writer
            .write_batch(&[optimization(T0 + 2, "acme", "late", 100, 20)])
            .unwrap();
        writer.maintain(T0 + DAY).unwrap();
        let rows = harness.reader().rollups("acme", DAY0, DAY0).unwrap();
        assert_eq!(rows[0].requests, 2);
        assert_eq!(rows[0].retrievals, 1);
        assert_eq!(rows[0].saved_gross(), 160);
    }

    #[test]
    fn upgrading_does_not_mark_rows_the_old_rollup_never_counted() {
        // The v6 backfill decided "already archived" from the DATE alone, while
        // a rollup is keyed (day, tenant, mode, surface). A tenant whose rows
        // landed on a day some *other* tenant had been rolled up for was marked
        // as already counted, skipped by the next archive pass, and then
        // deleted — losing the only copy.
        let dir = tempfile::tempdir().unwrap();
        let config = StatsConfig::at(dir.path().join("stats.db"));
        {
            let mut writer = StatsWriter::open(&config).unwrap();
            writer
                .write_batch(&[optimization(T0, "acme", "rolled", 100, 20)])
                .unwrap();
            writer.rollup(DAY0).unwrap();
            // Arrives after the rollup, and for a different tenant: nothing has
            // ever archived it.
            writer
                .write_batch(&[optimization(T0 + 10, "globex", "never-rolled", 900, 90)])
                .unwrap();
            writer
                .connection()
                .execute_batch(
                    "ALTER TABLE optimization_events DROP COLUMN rolled_up;
                     ALTER TABLE retrieval_events DROP COLUMN rolled_up;
                     DROP VIEW IF EXISTS v_retrieval_hit_rate;
                     ALTER TABLE retrieval_events DROP COLUMN withheld;
                     ALTER TABLE proxy_requests DROP COLUMN rolled_up;
                     ALTER TABLE proxy_requests DROP COLUMN strip_usage;
                     ALTER TABLE proxy_requests DROP COLUMN cache_write_1h_tokens;
                     ALTER TABLE proxy_requests DROP COLUMN keep_warm;
                     PRAGMA user_version = 5;",
                )
                .unwrap();
        }
        let mut writer = StatsWriter::open(&config).unwrap();
        writer.maintain(T0 + DAY).unwrap();

        let reader = StatsReader::open(&config.path).unwrap();
        assert_eq!(
            reader.rollups("acme", DAY0, DAY0).unwrap()[0].requests,
            1,
            "the already-archived tenant must not be counted twice"
        );
        let globex = reader.rollups("globex", DAY0, DAY0).unwrap();
        assert_eq!(
            globex.len(),
            1,
            "the tenant that was never archived must survive the upgrade"
        );
        assert_eq!(globex[0].requests, 1);
        assert_eq!(globex[0].tokens_in, 900);
    }

    #[test]
    fn upgrading_existing_manual_rollups_prevents_retention_double_counting() {
        let dir = tempfile::tempdir().unwrap();
        let config = StatsConfig::at(dir.path().join("stats.db"));
        {
            let mut writer = StatsWriter::open(&config).unwrap();
            writer
                .write_batch(&[optimization(T0, "acme", "old", 100, 20)])
                .unwrap();
            writer.rollup(DAY0).unwrap();
            writer
                .connection()
                .execute_batch(
                    "ALTER TABLE optimization_events DROP COLUMN rolled_up;
                ALTER TABLE retrieval_events DROP COLUMN rolled_up;
                DROP VIEW IF EXISTS v_retrieval_hit_rate;
                ALTER TABLE retrieval_events DROP COLUMN withheld;
                ALTER TABLE proxy_requests DROP COLUMN rolled_up;
                ALTER TABLE proxy_requests DROP COLUMN strip_usage;
                ALTER TABLE proxy_requests DROP COLUMN cache_write_1h_tokens;
                ALTER TABLE proxy_requests DROP COLUMN keep_warm;
                PRAGMA user_version = 5;",
                )
                .unwrap();
        }
        let mut writer = StatsWriter::open(&config).unwrap();
        writer.maintain(T0 + DAY).unwrap();
        let reader = StatsReader::open(&config.path).unwrap();
        assert_eq!(reader.rollups("acme", DAY0, DAY0).unwrap()[0].requests, 1);
    }

    #[test]
    fn zero_retention_days_disables_pruning_rather_than_pruning_everything() {
        // The trap this guards: `retention_days = 0` reads as "off", and taken
        // literally is a cutoff of *now* — every row before this midnight
        // deleted, hourly, for ever. The most destructive setting a knob has
        // must not be spelled the same way as turning it off.
        let config = StatsConfig {
            retention_days: 0,
            ..StatsConfig::at("unused.db")
        };
        assert_eq!(config.retention_cutoff_ms(T0), None);
        // And a real window still produces one.
        let config = StatsConfig {
            retention_days: 2,
            ..StatsConfig::at("unused.db")
        };
        assert_eq!(config.retention_cutoff_ms(T0), Some(T0 - 2 * DAY));
    }

    #[test]
    fn the_request_economy_survives_retention_as_a_daily_total() {
        // `daily_rollups` aggregates the engine's own events only, so without a
        // second archive the billed-prompt side — what the provider charged and
        // how much of it was a cache read — vanished at the horizon with
        // nothing left behind.
        let mut harness = Harness::new();
        harness.write(vec![
            proxy(T0, "acme", "old-1"),
            proxy(T0 + 1_000, "acme", "old-2"),
            proxy(T0 + DAY, "acme", "recent"),
        ]);
        harness.shutdown();
        let mut writer = StatsWriter::open(&StatsConfig::at(harness.path())).unwrap();
        assert_eq!(writer.maintain(T0 + DAY).unwrap().proxy_requests, 2);

        let conn = rusqlite::Connection::open(harness.path()).unwrap();
        let (requests, prompt, cache_read, completion): (i64, i64, i64, i64) = conn
            .query_row(
                "SELECT requests, prompt_tokens, cache_read_tokens, completion_tokens
                   FROM daily_requests WHERE day = ?1 AND tenant = 'acme'",
                rusqlite::params![DAY0],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("the pruned day was archived");
        assert_eq!(requests, 2, "both old requests counted");
        assert_eq!(prompt, 2 * 4_200);
        assert_eq!(cache_read, 2 * 3_000);
        assert_eq!(completion, 2 * 180);

        // Repeating a pass must not double the archive.
        writer.maintain(T0 + DAY).unwrap();
        let again: i64 = conn
            .query_row(
                "SELECT requests FROM daily_requests WHERE day = ?1",
                rusqlite::params![DAY0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(again, 2, "a second pass archived nothing twice");
    }

    #[test]
    fn a_conversation_archived_in_two_passes_is_counted_once() {
        // The reason this is a set and not a column. A DISTINCT count has to be
        // summed when a day is archived more than once — late rows force that —
        // and summing counts double-counts every conversation in both passes.
        // `INSERT OR IGNORE` on the identity is a union, so the answer is the
        // same however the archiving is split.
        let mut harness = Harness::new();
        harness.write(vec![proxy_in(T0, "acme", "first", "conv-a")]);
        harness.shutdown();
        let mut writer = StatsWriter::open(&StatsConfig::at(harness.path())).unwrap();
        writer.maintain(T0 + DAY).unwrap();

        // The same conversation, same day, arriving after that day was archived.
        writer
            .write_batch(&[
                proxy_in(T0 + 10, "acme", "late-same", "conv-a"),
                proxy_in(T0 + 20, "acme", "late-new", "conv-b"),
            ])
            .unwrap();
        writer.maintain(T0 + DAY).unwrap();

        let conn = rusqlite::Connection::open(harness.path()).unwrap();
        let (requests, conversations): (i64, i64) = conn
            .query_row(
                "SELECT requests, conversations FROM v_daily_requests
                  WHERE day = ?1 AND tenant = 'acme'",
                rusqlite::params![DAY0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(requests, 3, "every request is counted");
        assert_eq!(conversations, 2, "conv-a once, conv-b once");
    }

    #[test]
    fn a_request_with_no_conversation_is_not_counted_as_one() {
        // A passthrough request belongs to no conversation; counting it would
        // inflate the one number this table exists to get right.
        //
        // This guards the BEHAVIOUR, not the `IS NOT NULL` filter: the column is
        // NOT NULL and `INSERT OR IGNORE` drops the violation anyway, so the
        // test passes with the filter removed. Two mechanisms, one outcome —
        // worth knowing before concluding the filter is load-bearing.
        let mut harness = Harness::new();
        harness.write(vec![proxy(T0, "acme", "no-conversation")]);
        harness.shutdown();
        let mut writer = StatsWriter::open(&StatsConfig::at(harness.path())).unwrap();
        writer.maintain(T0 + DAY).unwrap();

        let conn = rusqlite::Connection::open(harness.path()).unwrap();
        let (requests, conversations): (i64, i64) = conn
            .query_row(
                "SELECT requests, conversations FROM v_daily_requests WHERE day = ?1",
                rusqlite::params![DAY0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(requests, 1);
        assert_eq!(conversations, 0);
    }

    #[test]
    fn a_zero_day_cap_is_a_no_op_rather_than_a_delete() {
        // A zero cap archives nothing, and the prune horizon fell through to
        // the full cutoff — every eligible row deleted with no copy taken.
        let mut harness = Harness::new();
        harness.write(vec![optimization(T0, "acme", "old", 100, 20)]);
        harness.shutdown();
        let mut writer = StatsWriter::open(&StatsConfig::at(harness.path())).unwrap();
        assert_eq!(writer.maintain_bounded(T0 + DAY, 0).unwrap().total(), 0);
        assert_eq!(
            harness
                .reader()
                .summary("acme", 0, i64::MAX)
                .unwrap()
                .requests,
            1,
            "the row is still here"
        );
    }

    #[test]
    fn recomputing_a_day_whose_rows_are_gone_keeps_the_archive() {
        // `rollup` deletes the day's archive and rebuilds it from raw rows.
        // Once retention has pruned those rows there is nothing to rebuild
        // from, so the rebuild produced nothing and the only copy was lost.
        let mut harness = Harness::new();
        harness.write(vec![optimization(T0, "acme", "old", 100, 20)]);
        harness.shutdown();
        let mut writer = StatsWriter::open(&StatsConfig::at(harness.path())).unwrap();
        writer.maintain(T0 + DAY).unwrap();
        assert_eq!(
            harness.reader().rollups("acme", DAY0, DAY0).unwrap()[0].requests,
            1
        );

        assert_eq!(writer.rollup(DAY0).unwrap(), 0, "nothing to recompute from");
        assert_eq!(
            harness.reader().rollups("acme", DAY0, DAY0).unwrap()[0].requests,
            1,
            "the archive survived the attempted recompute"
        );
    }

    #[test]
    fn a_capped_pass_never_prunes_a_day_it_has_not_archived() {
        // The cap keeps one transaction small, which is only safe if the delete
        // stops where the archiving stopped. Pruning to the full cutoff while
        // rolling up a prefix of the backlog would drop days with no rollup —
        // silent, and unrecoverable.
        let mut harness = Harness::new();
        let days: Vec<_> = (0..4)
            .map(|d| optimization(T0 + d * DAY, "acme", &format!("day-{d}"), 1_000, 100))
            .collect();
        harness.write(days);
        harness.shutdown();
        let mut writer = StatsWriter::open(&StatsConfig::at(harness.path())).unwrap();

        // Four days are old enough; allow two per pass.
        let counts = writer.maintain_bounded(T0 + 4 * DAY, 2).unwrap();
        assert_eq!(counts.optimization_events, 2, "only the archived days went");

        let conn = rusqlite::Connection::open(harness.path()).unwrap();
        let archived: i64 = conn
            .query_row("SELECT COUNT(DISTINCT day) FROM daily_rollups", [], |r| {
                r.get(0)
            })
            .unwrap();
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM optimization_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(archived, 2);
        assert_eq!(left, 2, "the unarchived days are still there");

        // The backlog clears on later passes, losing nothing.
        writer.maintain_bounded(T0 + 4 * DAY, 2).unwrap();
        let archived: i64 = conn
            .query_row("SELECT COUNT(DISTINCT day) FROM daily_rollups", [], |r| {
                r.get(0)
            })
            .unwrap();
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM optimization_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(archived, 4);
        assert_eq!(left, 0);
    }

    #[test]
    fn scheduled_retention_archives_complete_days_and_is_safe_to_repeat() {
        let mut harness = Harness::new();
        harness.write(vec![
            optimization(T0, "acme", "old", 10_000, 2_000),
            optimization(T0 + DAY, "acme", "recent", 100, 20),
        ]);
        harness.shutdown();
        let mut writer = StatsWriter::open(&StatsConfig::at(harness.path())).unwrap();
        assert_eq!(writer.maintain(T0 + DAY).unwrap().optimization_events, 1);
        assert_eq!(writer.maintain(T0 + DAY).unwrap().total(), 0);
        assert_eq!(
            harness.reader().rollups("acme", DAY0, DAY0).unwrap()[0].requests,
            1
        );
        assert_eq!(
            harness
                .reader()
                .summary("acme", 0, i64::MAX)
                .unwrap()
                .requests,
            1
        );
        // Late-arriving old events add to the archive instead of erasing it.
        writer
            .write_batch(&[optimization(T0 + 1, "acme", "late", 100, 20)])
            .unwrap();
        writer.maintain(T0 + DAY).unwrap();
        let rollups = harness.reader().rollups("acme", DAY0, DAY0).unwrap();
        assert_eq!(rollups[0].requests, 2);
        assert_eq!(rollups[0].saved_gross(), 8_080);
    }

    #[test]
    fn retention_rollup_and_prune_roll_back_together_on_failure() {
        let mut harness = Harness::new();
        harness.write(vec![optimization(T0, "acme", "old", 100, 20)]);
        harness.shutdown();
        let mut writer = StatsWriter::open(&StatsConfig::at(harness.path())).unwrap();
        writer.connection().execute_batch("CREATE TRIGGER reject_prune BEFORE DELETE ON optimization_events BEGIN SELECT RAISE(ABORT, 'test failure'); END;").unwrap();
        assert!(writer.maintain(T0 + DAY).is_err());
        assert!(
            harness
                .reader()
                .rollups("acme", DAY0, DAY0)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            harness
                .reader()
                .summary("acme", 0, i64::MAX)
                .unwrap()
                .requests,
            1
        );
        writer
            .connection()
            .execute_batch("DROP TRIGGER reject_prune;")
            .unwrap();
        writer.maintain(T0 + DAY).unwrap();
        assert_eq!(
            harness.reader().rollups("acme", DAY0, DAY0).unwrap()[0].requests,
            1
        );
    }

    #[test]
    fn starting_the_writer_runs_retention_on_existing_data() {
        let mut harness = Harness::new();
        harness.write(vec![optimization(T0, "acme", "old", 100, 20)]);
        harness.shutdown();
        let (sink, handle) = spawn(&StatsConfig::at(harness.path())).unwrap();
        sink.flush().unwrap();
        handle.shutdown().unwrap();
        assert_eq!(
            harness
                .reader()
                .summary("acme", 0, i64::MAX)
                .unwrap()
                .requests,
            0
        );
        assert_eq!(
            harness.reader().rollups("acme", DAY0, DAY0).unwrap()[0].requests,
            1
        );
    }

    #[test]
    fn prune_removes_old_rows_while_rollup_preserves_their_aggregate() {
        let mut harness = Harness::new();
        harness.write(vec![
            optimization(T0, "acme", "req-1", 10_000, 2_000),
            retrieval(T0 + 5, "acme", Some("req-1"), "k_rrrrrrrrrrrr", true, 1_000),
            retrieval(T0 + 6, "acme", Some("req-1"), "k_ssssssssssss", false, 0),
            proxy(T0, "acme", "req-1"),
            Event::StoreSample {
                time_ms: T0,
                live_refs: 3,
                live_bytes: 30,
            },
        ]);
        harness.shutdown();

        let config = StatsConfig::at(harness.path());
        let mut writer = StatsWriter::open(&config).unwrap();
        assert_eq!(writer.rollup(DAY0).unwrap(), 1);

        let counts = writer.prune(T0 + 86_400_000).unwrap();
        assert_eq!(counts.optimization_events, 1);
        assert_eq!(counts.retrieval_events, 2);
        assert_eq!(counts.proxy_requests, 1);
        assert_eq!(counts.store_samples, 1);
        assert_eq!(counts.total(), 5);
        drop(writer);

        let reader = harness.reader();
        assert_eq!(reader.summary("acme", 0, i64::MAX).unwrap().requests, 0);

        let rollups = reader.rollups("acme", DAY0, DAY0).unwrap();
        assert_eq!(rollups.len(), 1, "the aggregate outlived the rows");
        assert_eq!(rollups[0].requests, 1);
        assert_eq!(rollups[0].saved_gross(), 8_000);
        assert_eq!(rollups[0].retrievals, 2);
        assert_eq!(rollups[0].retrieval_hits, 1);
        assert_eq!(rollups[0].retrieved_tokens, 1_000);
        assert_eq!(rollups[0].saved_net(), 7_000);
        assert!(reader.rollups("globex", DAY0, DAY0).unwrap().is_empty());
    }

    #[test]
    fn rollup_is_a_recompute_so_running_it_twice_does_not_double_a_day() {
        let mut harness = Harness::new();
        harness.write(vec![optimization(T0, "acme", "req-1", 1_000, 100)]);
        harness.shutdown();

        let config = StatsConfig::at(harness.path());
        let mut writer = StatsWriter::open(&config).unwrap();
        writer.rollup(DAY0).unwrap();
        writer.rollup(DAY0).unwrap();
        drop(writer);

        let rollups = harness.reader().rollups("acme", DAY0, DAY0).unwrap();
        assert_eq!(rollups.len(), 1);
        assert_eq!(rollups[0].tokens_in, 1_000);
    }

    #[test]
    fn rollup_attributes_an_orphan_retrieval_rather_than_dropping_its_debit() {
        let mut harness = Harness::new();
        // the plugin path carried no join key, so retrievals can arrive with no
        // request to attach to; the debit still has to be counted
        harness.write(vec![retrieval(
            T0,
            "acme",
            None,
            "k_tttttttttttt",
            true,
            4_000,
        )]);
        harness.shutdown();

        let mut writer = StatsWriter::open(&StatsConfig::at(harness.path())).unwrap();
        writer.rollup(DAY0).unwrap();
        drop(writer);

        let rollups = harness.reader().rollups("acme", DAY0, DAY0).unwrap();
        assert_eq!(rollups.len(), 1);
        assert_eq!(rollups[0].mode, "unknown");
        assert_eq!(rollups[0].retrieved_tokens, 4_000);
        assert_eq!(rollups[0].saved_net(), -4_000);
    }

    #[test]
    fn a_json_column_rejects_text_that_is_not_json() {
        let dir = tempfile::tempdir().unwrap();
        let conn = schema::open_read_write(&dir.path().join("stats.db")).unwrap();

        let insert = |content_types: &str| {
            conn.execute(
                "INSERT INTO optimization_events (
                    time_ms, tenant, request_id, surface, mode, tokens_in, tokens_out,
                    chars_in, chars_out, messages_total, messages_compressed, messages_skipped,
                    refs_emitted, ref_bytes_emitted, content_types, protected_by_reason, engine_ms)
                 VALUES (1,'acme','req','api','recoverable',1,1,1,1,1,1,0,0,0,?1,'{}',0.0)",
                [content_types],
            )
        };
        assert!(insert(r#"{"json":2}"#).is_ok());
        let err = insert("{oops").expect_err("the CHECK constraint fires");
        assert!(
            err.to_string().to_lowercase().contains("constraint"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn the_surface_column_rejects_a_surface_outside_the_four() {
        let dir = tempfile::tempdir().unwrap();
        let conn = schema::open_read_write(&dir.path().join("stats.db")).unwrap();
        let err = conn
            .execute(
                "INSERT INTO optimization_events (
                    time_ms, tenant, request_id, surface, mode, tokens_in, tokens_out,
                    chars_in, chars_out, messages_total, messages_compressed, messages_skipped,
                    refs_emitted, ref_bytes_emitted, content_types, protected_by_reason, engine_ms)
                 VALUES (1,'acme','req','webhook','recoverable',1,1,1,1,1,1,0,0,0,'{}','{}',0.0)",
                [],
            )
            .expect_err("the CHECK constraint fires");
        assert!(err.to_string().to_lowercase().contains("constraint"));
    }

    #[test]
    fn the_writer_thread_flushes_what_it_had_and_shuts_down_cleanly() {
        // long enough that nothing commits on the timer: only shutdown can
        let mut harness = Harness::with_config(|config| {
            config.flush_interval = Duration::from_secs(600);
            config.batch_max = 10_000;
        });
        for i in 0..7 {
            harness
                .sink
                .send(optimization(T0, "acme", &format!("req-{i}"), 100, 10));
        }
        harness.shutdown();

        let summary = harness.reader().summary("acme", 0, i64::MAX).unwrap();
        assert_eq!(summary.requests, 7, "the final commit kept the whole batch");
        assert_eq!(summary.saved_gross, 7 * 90);
        assert_eq!(harness.sink.dropped(), 0);

        // sending into a stopped writer is still not an error for the caller
        harness
            .sink
            .send(optimization(T0, "acme", "req-late", 1, 1));
        assert!(matches!(
            harness.sink.flush(),
            Err(StatsError::WriterGone) | Ok(())
        ));
    }

    #[test]
    fn the_batch_cap_commits_before_the_interval_expires() {
        let harness = Harness::with_config(|config| {
            config.flush_interval = Duration::from_secs(600);
            config.batch_max = 4;
        });
        for i in 0..4 {
            harness
                .sink
                .send(optimization(T0, "acme", &format!("req-{i}"), 100, 10));
        }

        // no flush() call: the cap alone has to have committed the batch
        let reader = harness.reader();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let requests = reader.summary("acme", 0, i64::MAX).unwrap().requests;
            if requests == 4 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the batch cap never committed; saw {requests} rows"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_disabled_config_starts_no_thread_and_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        let mut config = StatsConfig::at(&path);
        config.enabled = false;

        let (sink, handle) = spawn(&config).unwrap();
        assert!(!sink.is_enabled());
        sink.send(optimization(T0, "acme", "req-1", 100, 10));
        sink.flush().unwrap();
        handle.shutdown().unwrap();

        assert_eq!(sink.dropped(), 0);
        assert!(!path.exists(), "a disabled sink must not create the file");
    }

    #[test]
    fn the_null_sink_swallows_events_and_never_fails() {
        let sink = null_sink();
        assert!(!sink.is_enabled());
        sink.send(optimization(T0, "acme", "req-1", 100, 10));
        assert!(sink.flush().is_ok());
        assert_eq!(sink.dropped(), 0);
    }

    #[test]
    fn surfaces_round_trip_through_the_spelling_the_check_constraint_allows() {
        for surface in [Surface::Api, Surface::Mcp, Surface::Proxy, Surface::Cli] {
            assert_eq!(Surface::parse(surface.as_str()), Some(surface));
            assert_eq!(
                serde_json::to_string(&surface).unwrap(),
                format!("\"{surface}\"")
            );
        }
        assert_eq!(Surface::parse("webhook"), None);
    }

    #[test]
    fn record_type_accumulates_under_the_engines_own_type_names() {
        let mut event = OptimizationEvent::new(T0, "acme", "req-1", Surface::Cli, Mode::Lossless);
        event.record_type(Kind::SourceCode, 1);
        event.record_type(Kind::SourceCode, 2);
        event.record_protected("role:system", 1);

        assert_eq!(event.content_types.get("source_code"), Some(&3));
        assert_eq!(event.protected_by_reason.get("role:system"), Some(&1));
    }

    #[test]
    fn an_event_round_trips_through_its_tagged_json_form() {
        let event = optimization(T0, "acme", "req-1", 100, 10);
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.starts_with("{\"optimization\":"), "{json}");

        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(back.time_ms(), T0);
        assert_eq!(back.tenant(), Some("acme"));

        let sample: Event = StoreSample {
            time_ms: T0,
            live_refs: 1,
            live_bytes: 2,
        }
        .into();
        assert_eq!(sample.tenant(), None, "the store sweep is process-wide");
    }

    /// The three states a wrapped session can end in. Only one of them is a
    /// fault, and it is the one that otherwise looks exactly like success.
    #[test]
    fn a_session_tells_no_traffic_apart_from_nothing_compressible() {
        let harness = Harness::new();

        // 1. Nothing routed through the proxy at all.
        let nothing = harness.reader().session_since(0).unwrap();
        assert_eq!(nothing.proxy_requests, 0, "the agent never reached us");
        assert_eq!(nothing.messages_compressed, 0);

        // 2. Requests arrived and every message was withheld.
        let mut frozen =
            OptimizationEvent::new(T0, "acme", "req-1", Surface::Proxy, Mode::Recoverable);
        frozen.tokens_in = 500;
        frozen.tokens_out = 500;
        frozen.messages_total = 4;
        frozen.messages_compressed = 0;
        frozen.messages_skipped = 4;
        harness.write(vec![
            proxy(T0, "acme", "req-1"),
            Event::Optimization(frozen),
        ]);

        let pinned = harness.reader().session_since(0).unwrap();
        assert_eq!(pinned.proxy_requests, 1, "traffic did arrive");
        assert_eq!(pinned.messages_compressed, 0, "and none of it was eligible");
        assert_eq!(pinned.saved_gross, 0);

        // 3. A session that actually worked, with retrieval charged against it.
        harness.write(vec![
            proxy(T0 + 1, "acme", "req-2"),
            optimization(T0 + 1, "acme", "req-2", 1_000, 400),
            retrieval(T0 + 1, "acme", Some("req-2"), "k_AbC123dEf456", true, 50),
        ]);
        let working = harness.reader().session_since(0).unwrap();
        assert_eq!(working.proxy_requests, 2);
        assert_eq!(working.messages_compressed, 1);
        assert_eq!(working.saved_gross, 600);
        assert_eq!(working.retrieved_tokens, 50);
        assert_eq!(working.saved_net(), 550, "retrieval is charged back");
    }

    /// The window is what makes it *this* session's work. A previous run's
    /// rows must not be reported as the current one's, or the check that is
    /// supposed to catch a silent wrap would be permanently satisfied by
    /// history.
    #[test]
    fn an_earlier_sessions_rows_are_not_counted_as_this_ones() {
        let harness = Harness::new();
        harness.write(vec![
            proxy(T0, "acme", "old"),
            optimization(T0, "acme", "old", 900, 100),
        ]);

        let ours = harness.reader().session_since(T0 + 1).unwrap();
        assert_eq!(ours.proxy_requests, 0, "that was a previous session");
        assert_eq!(ours.saved_gross, 0);

        let everything = harness.reader().session_since(0).unwrap();
        assert_eq!(everything.proxy_requests, 1, "and it is still on record");
    }

    /// The debit is charged to the mode of the request it was fetched during,
    /// and a retrieval belonging to no compressed request is in no mode row.
    #[test]
    fn the_retrieval_debit_is_attributed_to_the_mode_it_was_fetched_in() {
        let harness = Harness::new();
        // A second mode, so "recoverable gets everything" cannot pass by
        // accident on a single-mode fixture.
        let mut aggressive =
            OptimizationEvent::new(T0, "acme", "req-agg", Surface::Proxy, Mode::Aggressive);
        aggressive.tokens_in = 2_000;
        aggressive.tokens_out = 500;
        harness.write(vec![
            optimization(T0, "acme", "req-rec", 10_000, 2_000),
            Event::Optimization(aggressive),
            retrieval(T0 + 1, "acme", Some("req-rec"), "k_aaaaaaaaaaaa", true, 400),
            retrieval(T0 + 2, "acme", Some("req-agg"), "k_bbbbbbbbbbbb", true, 30),
            // Fetched during a request nothing compressed: no mode owns it.
            retrieval(T0 + 3, "acme", Some("req-none"), "k_cccccccccccc", true, 7),
            // A miss is charged to nobody at all.
            retrieval(T0 + 4, "acme", Some("req-rec"), "k_dddddddddddd", false, 99),
        ]);

        let summary = harness.reader().summary("acme", 0, i64::MAX).unwrap();
        let mode = |name: &str| {
            summary
                .by_mode
                .iter()
                .find(|m| m.mode == name)
                .unwrap_or_else(|| panic!("no {name} row"))
                .clone()
        };

        let rec = mode("recoverable");
        assert_eq!(rec.saved_gross, 8_000);
        assert_eq!(rec.retrieved_tokens, 400, "the miss must not be charged");
        assert_eq!(rec.saved_net(), 7_600);

        let agg = mode("aggressive");
        assert_eq!(agg.saved_gross, 1_500);
        assert_eq!(agg.retrieved_tokens, 30);
        assert_eq!(agg.saved_net(), 1_470);

        // The window total counts the orphan; the per-mode rows cannot, and the
        // report says so rather than letting the column quietly not add up.
        assert_eq!(summary.retrieved_tokens, 437);
        let attributed: i64 = summary.by_mode.iter().map(|m| m.retrieved_tokens).sum();
        assert_eq!(
            summary.retrieved_tokens - attributed,
            7,
            "the unattributable retrieval is missing from the rows, by design"
        );
    }

    /// `strip_usage` has to survive the write, or it cannot answer the question
    /// it exists for: whether a request the client could not account for is
    /// where an agent's totals and the proxy's part company.
    #[test]
    fn whether_usage_was_stripped_survives_the_write() {
        let harness = Harness::new();
        let stripped = match proxy(T0, "acme", "req-stripped") {
            Event::ProxyRequest(mut e) => {
                e.strip_usage = true;
                Event::ProxyRequest(e)
            }
            other => other,
        };
        harness.write(vec![proxy(T0, "acme", "req-kept"), stripped]);

        let conn = rusqlite::Connection::open(harness.path()).unwrap();
        let read = |id: &str| -> i64 {
            conn.query_row(
                "SELECT strip_usage FROM proxy_requests WHERE request_id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(read("req-stripped"), 1);
        assert_eq!(read("req-kept"), 0, "and the default is not sticky");
    }

    /// A miss puts nothing back, so it must not be charged against the saving.
    #[test]
    fn only_retrievals_that_found_something_are_charged_back() {
        let harness = Harness::new();
        harness.write(vec![
            proxy(T0, "acme", "req-1"),
            optimization(T0, "acme", "req-1", 1_000, 400),
            retrieval(T0, "acme", Some("req-1"), "k_AbC123dEf456", false, 99),
        ]);
        let outcome = harness.reader().session_since(0).unwrap();
        assert_eq!(outcome.retrieved_tokens, 0, "a miss re-injects nothing");
        assert_eq!(outcome.saved_net(), outcome.saved_gross);
    }
}
