//! The health verdict: thresholds on hand-built reports, and the reader
//! against a real, migrated database.

use rusqlite::params;
use tempfile::TempDir;

use super::*;
use crate::dashboards::{StatusMix, StorePoint};
use crate::live::Latency;
use crate::schema;

const MIN: i64 = 60_000;

fn upstream(requests: u64, tweak: impl FnOnce(&mut StatusMix)) -> UpstreamReport {
    let mut totals = StatusMix {
        requests,
        ok: requests,
        ..StatusMix::default()
    };
    tweak(&mut totals);
    UpstreamReport {
        totals,
        ..UpstreamReport::default()
    }
}

fn engine(runs: u64, p95: f64) -> EngineReport {
    EngineReport {
        runs,
        latency: Latency {
            samples: runs,
            p95: Some(p95),
            ..Latency::default()
        },
        ..EngineReport::default()
    }
}

fn check<'a>(report: &'a HealthReport, id: &str) -> &'a Check {
    report.checks.iter().find(|c| c.id == id).unwrap()
}

fn runtime() -> Runtime {
    Runtime {
        now_ms: 100 * MIN,
        inflight_limit: 32,
        inflight_free: 31,
        store_max_bytes: 1_000,
        ..Runtime::default()
    }
}

#[test]
fn a_quiet_healthy_window_is_ok() {
    let report = assess(
        &upstream(1_000, |_| {}),
        &engine(500, 150.0),
        &StoreHealth::default(),
        &runtime(),
    );
    assert_eq!(report.level, Level::Ok);
    assert_eq!(check(&report, "upstream_incidents").level, Level::Ok);
    assert_eq!(check(&report, "engine_p95").level, Level::Ok);
    assert_eq!(check(&report, "engine_slots").level, Level::Ok);
    // Too few asks to judge retrieval: reported, not counted.
    assert_eq!(check(&report, "retrieval_misses").level, Level::Info);
}

#[test]
fn thresholds_turn_warn_then_bad_and_the_worst_wins() {
    let warn = assess(
        &upstream(1_000, |t| t.rate_limited = 20),
        &engine(500, 150.0),
        &StoreHealth::default(),
        &runtime(),
    );
    assert_eq!(check(&warn, "upstream_incidents").level, Level::Warn);
    assert_eq!(warn.level, Level::Warn);

    let bad = assess(
        &upstream(1_000, |t| t.server_error = 60),
        &engine(500, 2_500.0),
        &StoreHealth::default(),
        &runtime(),
    );
    assert_eq!(check(&bad, "upstream_incidents").level, Level::Bad);
    assert_eq!(check(&bad, "engine_p95").level, Level::Bad);
    assert_eq!(bad.level, Level::Bad);
}

#[test]
fn small_samples_are_reported_but_not_judged() {
    let report = assess(
        &upstream(5, |t| t.server_error = 3),
        &engine(3, 9_000.0),
        &StoreHealth::default(),
        &runtime(),
    );
    assert_eq!(check(&report, "upstream_incidents").level, Level::Info);
    assert!(
        check(&report, "upstream_incidents")
            .detail
            .contains("too few")
    );
    assert_eq!(check(&report, "engine_p95").level, Level::Info);
    assert_eq!(report.level, Level::Ok);
}

#[test]
fn a_recent_burst_is_bad_an_old_one_a_warning() {
    let burst = |to_ms| FailureBurst {
        from_ms: to_ms - MIN,
        to_ms,
        failures: 4,
        ..FailureBurst::default()
    };
    let mut report = upstream(1_000, |_| {});
    report.bursts = vec![burst(95 * MIN)];
    let recent = assess(
        &report,
        &engine(500, 1.0),
        &StoreHealth::default(),
        &runtime(),
    );
    assert_eq!(check(&recent, "incident_bursts").level, Level::Bad);

    report.bursts = vec![burst(30 * MIN)];
    let old = assess(
        &report,
        &engine(500, 1.0),
        &StoreHealth::default(),
        &runtime(),
    );
    assert_eq!(check(&old, "incident_bursts").level, Level::Warn);
}

#[test]
fn store_fill_reads_the_newest_all_tenant_sample_against_the_cap() {
    let store = StoreHealth {
        live_bytes: 10,
        samples: vec![
            StorePoint {
                time_ms: 1,
                live_refs: 1,
                live_bytes: 100,
            },
            StorePoint {
                time_ms: 2,
                live_refs: 9,
                live_bytes: 960,
            },
        ],
        asks: 100,
        missing_asks: 30,
        withheld_asks: 40,
        ..StoreHealth::default()
    };
    let report = assess(
        &upstream(1_000, |_| {}),
        &engine(500, 1.0),
        &store,
        &runtime(),
    );
    assert_eq!(check(&report, "store_fill").level, Level::Bad);
    assert_eq!(check(&report, "retrieval_misses").level, Level::Bad);
    assert_eq!(check(&report, "retrieval_withheld").level, Level::Warn);

    let uncapped = Runtime {
        store_max_bytes: 0,
        ..runtime()
    };
    let report = assess(
        &upstream(1_000, |_| {}),
        &engine(500, 1.0),
        &store,
        &uncapped,
    );
    assert_eq!(check(&report, "store_fill").level, Level::Info);
}

#[test]
fn dropped_statistics_and_busy_slots_warn() {
    let busy = Runtime {
        stats_dropped: 3,
        inflight_free: 0,
        ..runtime()
    };
    let report = assess(
        &upstream(1_000, |_| {}),
        &engine(500, 1.0),
        &StoreHealth::default(),
        &busy,
    );
    assert_eq!(check(&report, "stats_dropped").level, Level::Warn);
    assert_eq!(check(&report, "engine_slots").level, Level::Bad);
}

#[test]
fn the_reader_joins_the_reports_and_finds_the_last_request() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("stats.db");
    let conn = schema::open_read_write(&path).unwrap();
    for i in 0..30_i64 {
        let status: Option<i64> = if i < 3 { Some(503) } else { Some(200) };
        conn.execute(
            "INSERT INTO proxy_requests (time_ms, tenant, request_id, api_family, model, upstream,
               status, stream, upstream_ms, total_ms, keep_warm)
             VALUES (?1, '', ?2, 'anthropic_messages', 'm', 'anthropic', ?3, 1, 100, 100, ?4)",
            params![i * MIN, format!("r{i}"), status, (i == 29) as i64],
        )
        .unwrap();
    }
    drop(conn);
    let report = StatsReader::open(&path)
        .unwrap()
        .health("", 0, 60 * MIN, &[], false, &runtime())
        .unwrap();
    let incidents = check(&report, "upstream_incidents");
    assert_eq!(incidents.value, Some(3.0 / 30.0));
    assert_eq!(incidents.level, Level::Bad);
    // The keep-warm ping at minute 29 is not a client request.
    assert_eq!(report.last_request_ms, Some(28 * MIN));
    assert_eq!(report.last_compression_ms, None);
    assert_eq!(report.level, Level::Bad);
}

/// The last backup is reported always and judged only where the deployment
/// said how old is too old; a failed attempt since the last good one is a
/// warning at any age.
#[test]
fn the_last_backup_is_judged_against_what_the_deployment_expects() {
    const HOUR: i64 = 60 * MIN;
    let judged = |watch: BackupWatch| {
        let report = assess(
            &upstream(1_000, |_| {}),
            &engine(500, 20.0),
            &StoreHealth::default(),
            &Runtime {
                now_ms: 1_000 * HOUR,
                backup: watch,
                ..runtime()
            },
        );
        let found = check(&report, "last_backup").clone();
        (found.level, found.detail, found.value, report.level)
    };
    let day = Some(24 * HOUR);
    let at = |hours_ago: i64| Some((1_000 - hours_ago) * HOUR);

    // Nothing expected: reported, never a warning.
    let (level, detail, value, overall) = judged(BackupWatch::default());
    assert_eq!((level, value, overall), (Level::Info, None, Level::Ok));
    assert!(detail.starts_with("none on record"), "{detail}");
    let (level, detail, value, _) = judged(BackupWatch {
        last_ok_ms: at(500),
        ..BackupWatch::default()
    });
    assert_eq!(level, Level::Info);
    assert_eq!(value, Some((500 * HOUR) as f64));
    assert!(detail.starts_with("taken 20 d ago"), "{detail}");

    // A day expected.
    let with = |last_ok_ms, last_failed_ms| {
        judged(BackupWatch {
            max_age_ms: day,
            last_ok_ms,
            last_failed_ms,
        })
    };
    assert_eq!(with(at(3), None).0, Level::Ok);
    assert_eq!(with(at(3), None).1, "taken 3 h ago");
    assert_eq!(with(at(30), None).0, Level::Warn);
    let (level, _, _, overall) = with(at(80), None);
    assert_eq!(
        (level, overall),
        (Level::Bad, Level::Bad),
        "three times too old"
    );
    let (level, detail, ..) = with(None, None);
    assert_eq!(level, Level::Warn);
    assert_eq!(detail, "none on record; one is expected");

    // A failure after the last good one, and one before it.
    let (level, detail, ..) = with(at(3), at(1));
    assert_eq!(level, Level::Warn);
    assert!(detail.ends_with("the last attempt failed"), "{detail}");
    assert_eq!(with(at(1), at(3)).0, Level::Ok, "it has worked since");
    assert_eq!(with(None, at(1)).0, Level::Warn);
}
