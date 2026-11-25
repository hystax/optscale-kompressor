//! Unit-level SQL tests use a real migrated file and the production writer.
use crate::context::{ProxyTiming, RunContext};
use crate::history::HistoryFilter;
use crate::{
    Event, OptimizationEvent, ProxyRequestEvent, RetrievalEvent, StatsConfig, StatsReader,
    StatsWriter, Surface,
};
use kompressor_engine::types::Mode;
const DAY: i64 = 86_400_000;
const T: i64 = 1_700_006_400_000; // UTC midnight
struct Db {
    dir: tempfile::TempDir,
    writer: StatsWriter,
}
impl Db {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let writer = StatsWriter::open(&StatsConfig::at(dir.path().join("stats.db"))).unwrap();
        Self { dir, writer }
    }
    fn reader(&self) -> StatsReader {
        StatsReader::open(self.dir.path().join("stats.db")).unwrap()
    }
    fn write(&mut self, events: Vec<Event>) {
        self.writer.write_batch(&events).unwrap();
    }
}
fn ctx() -> RunContext {
    RunContext {
        build_version: "2.0.5".into(),
        config_fingerprint: "fingerprint".into(),
        experiment_id: Some("experiment-a".into()),
        run_id: "run-a".into(),
        settings: serde_json::json!({"engine":{"mode":"recoverable"},"proxy":{"compress":true}}),
    }
}
fn proxy(time: i64, tenant: &str, id: &str, ping: bool) -> Event {
    let mut timing = ProxyTiming::default();
    timing.record_attempt(10.0, 15.0, Some(200), false);
    timing.record_retrieval(2.0);
    Event::ProxyRequest(ProxyRequestEvent {
        prefix: None,
        context: Some(ctx()),
        timing: Some(timing),
        time_ms: time,
        tenant: tenant.into(),
        request_id: id.into(),
        api_family: "anthropic_messages".into(),
        model: Some("model-a".into()),
        upstream: "https://example.invalid".into(),
        status: Some(200),
        stream: false,
        strip_usage: false,
        prompt_tokens: Some(100),
        completion_tokens: Some(10),
        cache_read_tokens: Some(1000),
        cache_write_tokens: Some(100),
        cache_write_1h_tokens: Some(0),
        shadow_prompt_tokens: None,
        conversation: Some("0123456789abcdef".into()),
        retrieve_rounds: 2,
        retrieve_tokens_injected: 200,
        upstream_ms: Some(10.0),
        total_ms: Some(if ping { 9999.0 } else { 100.0 }),
        error: None,
        keep_warm: ping,
        output_shaping: None,
    })
}
fn opt(time: i64, tenant: &str, id: &str) -> Event {
    let mut e = OptimizationEvent::new(time, tenant, id, Surface::Proxy, Mode::Recoverable);
    e.context = Some(ctx());
    e.model = Some("model-a".into());
    e.tokens_in = 1000;
    e.tokens_out = 400;
    e.outcome = Some("compressed".into());
    e.engine_ms = 2.0;
    Event::Optimization(e)
}
fn retrieval(
    time: i64,
    tenant: &str,
    id: Option<&str>,
    found: bool,
    expired: bool,
    withheld: bool,
    ranged: bool,
) -> Event {
    Event::Retrieval(RetrievalEvent {
        context: None,
        time_ms: time,
        tenant: tenant.into(),
        request_id: id.map(str::to_owned),
        surface: Surface::Proxy,
        reference: "k_AbC123dEf456".into(),
        found,
        expired,
        ranged,
        fragment_bytes: 4000,
        fragment_chars: 4000,
        fragment_tokens: 1000,
        returned_chars: if found { 2000 } else { 0 },
        returned_tokens: if found { 500 } else { 0 },
        withheld,
        range_status: None,
    })
}
#[test]
fn retrieval_rankings_do_not_multiply_credits_and_keep_outcomes_separate() {
    let mut db = Db::new();
    db.write(vec![
        proxy(T, "a", "r", false),
        opt(T, "a", "r"),
        opt(T, "a", "r"),
        retrieval(T, "a", Some("r"), true, false, false, true),
        retrieval(T, "a", Some("r"), true, false, false, false),
        retrieval(T, "a", Some("r"), true, false, false, false),
        retrieval(T, "a", Some("r"), false, true, false, false),
        retrieval(T, "a", Some("r"), false, false, false, false),
        retrieval(T, "a", Some("r"), false, false, true, false),
        retrieval(T, "a", None, true, false, false, false),
        opt(T, "other", "r"),
        retrieval(T + 1, "a", Some("outside"), true, false, false, false),
    ]);
    let r = db.reader().retrieval_analysis("a", T, T + 1).unwrap();
    assert_eq!(r.saved_gross, 1200);
    assert_eq!(r.retrieved_tokens, 2000);
    assert_eq!(r.attempts, 6);
    assert_eq!(r.hits, 4);
    assert_eq!(r.expired, 1);
    assert_eq!(r.missing, 1);
    assert_eq!(r.withheld, 1);
    assert_eq!(r.ranged_tokens, 500);
    assert_eq!(r.full_tokens, 1500);
    assert_eq!(r.requests.len(), 1);
    assert_eq!(r.requests[0].net_reduction, -300);
    assert_eq!(r.repeated_refs[0].asks, 6);
    assert_eq!(r.requests[0].rounds, 2);
}
#[test]
fn latency_excludes_pings_and_missing_samples_and_preserves_error_buckets() {
    let mut db = Db::new();
    let mut missing = proxy(T + 1, "a", "missing", false);
    if let Event::ProxyRequest(p) = &mut missing {
        p.total_ms = None;
        p.upstream_ms = None;
        p.timing = None;
        p.status = None;
        p.error = Some("timeout".into());
    }
    db.write(vec![
        proxy(T, "a", "r", false),
        proxy(T, "a", "ping", true),
        proxy(T, "other", "r", false),
        opt(T, "a", "r"),
        missing,
    ]);
    let r = db.reader().latency_analysis("a", T, T + 2).unwrap();
    assert_eq!(r.requests, 2);
    assert_eq!(r.errors, 1);
    assert_eq!(r.total.samples, 1);
    assert_eq!(r.total.p95, Some(100.0));
    assert_eq!(r.first_head.p50, Some(15.0));
    assert_eq!(r.attempt_wait.p50, Some(10.0));
    assert_eq!(r.engine.p50, Some(2.0));
    assert_eq!(r.retrieval_execution.p50, Some(2.0));
    assert_eq!(r.with_timing, 1);
    assert_eq!(r.errors_over_time.iter().map(|b| b.errors).sum::<u64>(), 1);
}
#[test]
fn daily_comparison_survives_pruning_retries_and_late_events() {
    let mut db = Db::new();
    db.write(vec![
        proxy(T + 1, "a", "r1", false),
        opt(T + 1, "a", "r1"),
        retrieval(T + 1, "a", Some("r1"), true, false, false, false),
        proxy(T + 2, "a", "ping", true),
        proxy(T + DAY + 1, "a", "r2", false),
        opt(T + DAY + 1, "a", "r2"),
        proxy(T + 1, "b", "r1", false),
    ]);
    let filter = HistoryFilter::default();
    let before = db.reader().history("a", T, T + 2 * DAY, &filter).unwrap();
    assert_eq!(before.metrics.client_requests, 2);
    assert_eq!(before.conversations, 1);
    assert_eq!(before.net_reduction, 700);
    assert_eq!(before.metrics.ping_requests, 1);
    assert_eq!(before.metrics.input_tokens, 3600);
    assert_eq!(before.mean_latency_ms, Some(100.0));
    db.writer.maintain(T + 2 * DAY).unwrap();
    db.writer.maintain(T + 2 * DAY).unwrap();
    let after = db.reader().history("a", T, T + 2 * DAY, &filter).unwrap();
    assert_eq!(
        serde_json::to_value(&before).unwrap(),
        serde_json::to_value(&after).unwrap()
    );
    db.write(vec![
        proxy(T + 3, "a", "late", false),
        opt(T + 3, "a", "late"),
    ]);
    let late_before = db.reader().history("a", T, T + 2 * DAY, &filter).unwrap();
    assert_eq!(late_before.metrics.client_requests, 3);
    db.writer.maintain(T + 2 * DAY).unwrap();
    let late_after = db.reader().history("a", T, T + 2 * DAY, &filter).unwrap();
    assert_eq!(
        serde_json::to_value(late_before).unwrap(),
        serde_json::to_value(late_after).unwrap()
    );
    let filter = HistoryFilter {
        experiment_id: Some("other-experiment".into()),
        ..Default::default()
    };
    assert_eq!(
        db.reader()
            .history("a", T, T + 2 * DAY, &filter)
            .unwrap()
            .metrics
            .client_requests,
        0
    );
    let partial = db
        .reader()
        .history("a", T + 1, T + DAY, &HistoryFilter::default())
        .unwrap();
    assert_eq!(partial.archived_partial_days_omitted, 1);
    assert_eq!(partial.net_per_client, None);
}
#[test]
fn comparisons_have_nullable_zero_denominators_and_filter_both_periods() {
    let mut db = Db::new();
    db.write(vec![proxy(T + 1, "a", "r", false), opt(T + 1, "a", "r")]);
    let r = db
        .reader()
        .compare_history(
            "a",
            (T, T + DAY),
            (T - DAY, T),
            &HistoryFilter {
                model: Some("model-a".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(r.baseline.net_per_client, None);
    assert_eq!(r.changes["Client requests"].absolute, Some(1.0));
    assert_eq!(r.changes["Client requests"].relative, None);
    assert_eq!(r.current.metrics.saved_gross, 600);
    let none = db
        .reader()
        .compare_history(
            "a",
            (T, T + DAY),
            (T - DAY, T),
            &HistoryFilter {
                run_id: Some("not-this-run".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(none.current.metrics.client_requests, 0);
}
#[test]
fn missing_metadata_stays_unrecorded_after_archival_and_upgrade_is_idempotent() {
    let mut db = Db::new();
    let mut p = proxy(T + 1, "a", "old", false);
    if let Event::ProxyRequest(e) = &mut p {
        e.context = None;
        e.timing = None;
    }
    db.write(vec![p]);
    db.writer.maintain(T + DAY).unwrap();
    let r = db
        .reader()
        .history("a", T, T + DAY, &HistoryFilter::default())
        .unwrap();
    assert_eq!(r.rows[0].dimension.build_version, "");
    assert_eq!(r.rows[0].dimension.run_id, "");
    assert_eq!(r.rows[0].dimension.mode, "unrecorded");
    assert_eq!(
        crate::schema::apply(db.writer.connection()).unwrap(),
        crate::schema::latest_version()
    );
}

#[test]
fn archive_and_live_days_union_conversations_and_preserve_negative_net() {
    let mut db = Db::new();
    db.write(vec![
        proxy(T + 1, "a", "r1", false),
        opt(T + 1, "a", "r1"),
        retrieval(T + 1, "a", Some("r1"), true, false, false, false),
        retrieval(T + 1, "a", Some("r1"), true, false, false, false),
        proxy(T + DAY + 1, "a", "r2", false),
    ]);
    db.writer.maintain(T + DAY).unwrap();
    let r = db
        .reader()
        .history("a", T, T + 2 * DAY, &HistoryFilter::default())
        .unwrap();
    assert_eq!(r.metrics.client_requests, 2);
    assert_eq!(r.conversations, 1);
    assert_eq!(r.net_reduction, -400);
    assert_eq!(r.net_per_client, Some(-200.0));
    let mode = db
        .reader()
        .history(
            "a",
            T,
            T + 2 * DAY,
            &HistoryFilter {
                mode: Some("recoverable".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(mode.metrics.client_requests, 2);
}

#[test]
fn a_real_v12_database_upgrades_without_inventing_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stats.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    for (version, sql) in crate::schema::MIGRATIONS {
        if *version <= 12 {
            conn.execute_batch(sql).unwrap();
        }
    }
    conn.execute_batch("PRAGMA user_version=12;
      INSERT INTO proxy_requests(time_ms,tenant,request_id,api_family,model,upstream,stream,prompt_tokens,total_ms)
      VALUES(1700006400001,'a','old','openai_chat','old-model','https://example.invalid',0,1000,100);").unwrap();
    drop(conn);
    let mut writer = StatsWriter::open(&StatsConfig::at(&path)).unwrap();
    assert_eq!(
        crate::schema::apply(writer.connection()).unwrap(),
        crate::schema::latest_version()
    );
    let reader = StatsReader::open(&path).unwrap();
    let raw = reader
        .history("a", T, T + DAY, &HistoryFilter::default())
        .unwrap();
    assert_eq!(raw.metrics.client_requests, 1);
    assert_eq!(raw.rows[0].dimension.build_version, "");
    writer.maintain(T + DAY).unwrap();
    assert_eq!(
        reader
            .history("a", T, T + DAY, &HistoryFilter::default())
            .unwrap()
            .metrics
            .input_tokens,
        1000
    );
}

#[test]
fn historical_input_cost_matches_cache_accounting_for_partial_provider_usage() {
    let mut db = Db::new();
    let mut openai = proxy(T, "a", "openai", false);
    if let Event::ProxyRequest(e) = &mut openai {
        e.api_family = "openai_chat".into();
        e.prompt_tokens = Some(2000);
        e.cache_read_tokens = Some(1000);
        e.cache_write_tokens = Some(999);
    }
    let mut partial = proxy(T, "a", "partial", false);
    if let Event::ProxyRequest(e) = &mut partial {
        e.prompt_tokens = None;
        e.cache_read_tokens = Some(1000);
        e.cache_write_tokens = None;
    }
    db.write(vec![openai, partial]);
    let reader = db.reader();
    let h = reader
        .history("a", T, T + DAY, &HistoryFilter::default())
        .unwrap();
    let c = reader.cache("a", T, T + DAY, 100, None).unwrap();
    assert_eq!(h.metrics.input_tokens, c.totals.input);
    assert_eq!(h.metrics.input_equivalent_cost, c.totals.cost);
    assert_eq!(h.metrics.usage_rows, c.totals.with_usage);
}

#[test]
fn retrieval_without_request_id_keeps_run_attribution_after_retention() {
    let mut db = Db::new();
    let mut event = retrieval(T + 1, "a", None, true, false, false, false);
    if let Event::Retrieval(r) = &mut event {
        r.context = Some(ctx());
    }
    db.write(vec![event]);
    let filter = HistoryFilter {
        run_id: Some("run-a".into()),
        ..Default::default()
    };
    let before = db.reader().history("a", T, T + DAY, &filter).unwrap();
    assert_eq!(before.metrics.retrieved_tokens, 500);
    assert_eq!(before.net_reduction, -500);
    assert_eq!(before.net_per_client, None);
    db.writer.maintain(T + DAY).unwrap();
    let after = db.reader().history("a", T, T + DAY, &filter).unwrap();
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(after).unwrap()
    );
}
