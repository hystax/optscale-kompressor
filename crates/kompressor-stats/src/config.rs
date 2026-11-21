//! Configuration for the stats writer.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// Queue depth before events start being dropped.
///
/// Sized to absorb a multi-second stall of the writer thread — a WAL checkpoint
/// on a slow disk, say — at the request rate a single node sustains. Making it
/// much larger does not buy more safety, it only buys a larger pile of events
/// to lose when the process exits, since nothing in the queue is durable.
pub const DEFAULT_CHANNEL_CAPACITY: usize = 16_384;

/// How long a batch may sit unwritten.
pub const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_millis(100);

/// How many events force a commit before the interval expires.
pub const DEFAULT_BATCH_MAX: usize = 500;

/// How long rows survive before [`crate::StatsWriter::prune`] removes them.
pub const DEFAULT_RETENTION_DAYS: u32 = 90;

/// Where and how the event store is written.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct StatsConfig {
    /// When false, [`crate::spawn`] starts no thread, opens no file and returns
    /// a sink that discards everything.
    pub enabled: bool,

    /// Path to `stats.db`. Missing parent directories are created.
    pub path: PathBuf,

    /// Bounded queue depth; see [`DEFAULT_CHANNEL_CAPACITY`].
    pub channel_capacity: usize,

    /// Commit at least this often.
    #[serde(with = "humantime_serde")]
    pub flush_interval: Duration,

    /// Commit at least this often in events, whichever comes first.
    pub batch_max: usize,

    /// Age at which rows become eligible for pruning. Aggregates survive it
    /// through `daily_rollups`.
    ///
    /// **Zero disables retention**, which is what someone writing `0` to turn it
    /// off means. Taken literally it would be a cutoff of *now*: every row older
    /// than this midnight deleted, once an hour, for ever. A knob whose "off"
    /// value is the most destructive setting it has is a trap, so it is spelled
    /// out here and enforced in [`StatsConfig::retention_cutoff_ms`].
    pub retention_days: u32,

    /// Age at which a request's copy of the effective settings
    /// (`request_context.context`) is emptied, the request itself kept. That
    /// copy is most of a busy database; the analysis history is archived
    /// before it goes, so no breakdown loses a request. Zero, the default,
    /// keeps it as long as the request.
    pub context_retention_days: u32,
}

impl Default for StatsConfig {
    fn default() -> StatsConfig {
        StatsConfig {
            enabled: true,
            path: PathBuf::from("stats.db"),
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
            flush_interval: DEFAULT_FLUSH_INTERVAL,
            batch_max: DEFAULT_BATCH_MAX,
            retention_days: DEFAULT_RETENTION_DAYS,
            context_retention_days: 0,
        }
    }
}

impl StatsConfig {
    /// An enabled configuration writing to `path`, everything else default.
    pub fn at(path: impl Into<PathBuf>) -> StatsConfig {
        StatsConfig {
            path: path.into(),
            ..StatsConfig::default()
        }
    }

    /// The cutoff for [`crate::StatsWriter::prune`] given a current clock
    /// reading, in milliseconds since the Unix epoch.
    ///
    /// `None` when `retention_days` is zero, which disables pruning rather than
    /// pruning everything — see the field's documentation.
    pub fn retention_cutoff_ms(&self, now_ms: i64) -> Option<i64> {
        (self.retention_days > 0).then(|| now_ms - i64::from(self.retention_days) * 86_400_000)
    }

    /// The cutoff for emptying requests' settings: `None` with
    /// `context_retention_days` zero, or when it is no shorter than
    /// `retention_days` and the requests go first anyway.
    pub fn context_cutoff_ms(&self, now_ms: i64) -> Option<i64> {
        let days = self.context_retention_days;
        (days > 0 && (self.retention_days == 0 || days < self.retention_days))
            .then(|| now_ms - i64::from(days) * 86_400_000)
    }
}
