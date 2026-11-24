//! What the writer thread does besides write: report its retention pass,
//! stand aside for a job that needs the database to itself, and carry on
//! after the statistics have been erased under it.

use kompressor_stats::events::{BackupEvent, Event};
use kompressor_stats::{StatsConfig, schema};
use rusqlite::Connection;

fn count(path: &std::path::Path, table: &str) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn sample(time_ms: i64) -> Event {
    Event::StoreSample {
        time_ms,
        live_refs: 3,
        live_bytes: 4_096,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[test]
fn erasing_removes_every_statistic_and_the_writer_carries_on() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stats.db");
    let (sink, handle) = kompressor_stats::spawn(&StatsConfig::at(&path)).unwrap();

    let now = now_ms();
    for offset in 0..50 {
        sink.send(sample(now + offset));
    }
    sink.send(Event::Backup(BackupEvent {
        time_ms: now,
        origin: "cli".to_string(),
        destination: "/backups/a.tar.gz".to_string(),
        bytes: 1_024,
        databases: 2,
        ok: true,
        error: None,
    }));

    // The first retention pass runs before the first event, and says so.
    sink.flush().unwrap();
    let upkeep = sink.upkeep().expect("a pass has run");
    assert_eq!(upkeep.error, None);
    assert_eq!(upkeep.removed.unwrap().total(), 0, "nothing is old yet");

    // What was queued before the job is committed before it runs: the job
    // sees, and deletes, all fifty.
    let for_job = path.clone();
    let erased = sink
        .exclusive(move || schema::erase(&for_job))
        .unwrap()
        .unwrap();
    assert_eq!(erased.rows, 50);
    assert!(erased.tables >= 5, "{erased:?}");
    assert_eq!(count(&path, "store_samples"), 0);
    assert_eq!(count(&path, "backups"), 1, "the record of backups is kept");

    // Same file, same schema, same writer.
    sink.send(sample(now + 100));
    sink.flush().unwrap();
    assert_eq!(count(&path, "store_samples"), 1);
    assert_eq!(
        schema::erase(&path).unwrap().rows,
        1,
        "and it can be erased again"
    );
    handle.shutdown().unwrap();
}

#[test]
fn with_statistics_off_an_exclusive_job_runs_where_it_is_called() {
    let sink = kompressor_stats::null_sink();
    assert_eq!(sink.exclusive(|| 7).unwrap(), 7);
    assert_eq!(sink.upkeep(), None);
}
