//! What the proxy remembers about each conversation, kept across a restart.
//!
//! Every per-conversation decision that changes the bytes sent upstream lives
//! in this process's memory: the prefix tracker's freeze watermark and cache
//! evidence, the conversation identities it learned, its fetch history and
//! inline promotions, the cold-turn read ledger, the pinned tool order and
//! beta tokens, and whether the tools are compacted. A restart used to forget
//! all of it, so every conversation open across one was new to the next
//! process: frozen to the client's marker, it went out as the client's
//! original bytes where the provider held them compressed, and the read
//! stopped at the first message that had been compressed (DESIGN §12.10).
//!
//! `proxy.state_path` keeps it in a file. The engine is deterministic and
//! handles are content-addressed, so the watermark and the decisions alone
//! reproduce the same bytes: nothing here holds message text. What is written
//! is hashes (FNV-1a, the same in every process), floors, flags, counts,
//! fragment handles, request ids, model names, tool names and beta tokens.
//!
//! # The file
//!
//! JSON, one object, with `format` and `version` first. It is written whole
//! to a temporary file beside the target, flushed, and renamed over it, so a
//! reader sees the old file or the new one and never half of one. Read once
//! at start: a file that is missing, cut short, not JSON, or of another
//! format version is ignored with a log line and the proxy starts with
//! nothing remembered, as it did before this existed. Loading never fails
//! startup and never blocks it beyond reading one bounded file.
//!
//! Loading is all or nothing. A ledger whose lowered floor was not restored
//! would replay reads into a conversation frozen to its marker, which is
//! worse than forgetting both.
//!
//! # Ages, not instants
//!
//! An `Instant` means nothing in another process, so each entry is written
//! with its age when the file was written, and the file with the wall-clock
//! time it was written. On load an entry is placed that age plus the time
//! the file sat on disk in the past. A host that rebooted cannot place an
//! instant before its boot; such an entry is placed at the moment of loading,
//! which reads as recently used — the side that never calls a live cache cold.
//!
//! Entries are not dropped for being older than the provider's cache
//! lifetime. The in-memory tables never expire entries by age either, only at
//! their caps, and an old entry is still the right one to keep: a
//! conversation whose cache lapsed during the restart is written again
//! whatever is sent, but with its watermark restored it is written in the
//! compressed form every later turn sends. Forgotten, it would be frozen to
//! the marker and its history would never be compressed again.
//!
//! # Bounded
//!
//! Each table is written as it stands, so the file is bounded by the caps the
//! tables already keep, and every cap is applied again on load, newest first,
//! so a file from a build with larger caps, or edited by hand, cannot grow
//! them. The one table with no cap per conversation — a read ledger — is
//! capped here at [`MAX_LEDGER_READS`]: a conversation past it is left out of
//! the file whole, its watermark included, and is new to the next process.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::ProxyState;

/// What the file says it is.
pub const FORMAT: &str = "kompressor-proxy-state";
/// The layout this build writes and reads. Any other is ignored on load.
pub const VERSION: u32 = 1;
/// The most reads one conversation's ledger may hold and still be written.
pub const MAX_LEDGER_READS: usize = 4096;
/// The longest string — a model, a tool name, a beta token, a request id —
/// written as it is. A longer one is left out, with what it belongs to.
pub(crate) const MAX_TEXT_BYTES: usize = 256;

/// Turning an `Instant` into an age and back.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Clock {
    now: Instant,
    /// How long the file sat on disk before it was read. Zero when writing.
    behind: Duration,
}

impl Clock {
    pub(crate) fn writing() -> Clock {
        Clock {
            now: Instant::now(),
            behind: Duration::ZERO,
        }
    }

    fn reading(written_at_ms: u64) -> Clock {
        let behind = unix_ms().saturating_sub(written_at_ms);
        Clock {
            now: Instant::now(),
            behind: Duration::from_millis(behind),
        }
    }

    /// How long ago `at` was, in milliseconds.
    pub(crate) fn age(&self, at: Instant) -> u64 {
        u64::try_from(self.now.saturating_duration_since(at).as_millis()).unwrap_or(u64::MAX)
    }

    /// The instant an entry written `age_ms` old now stands at.
    pub(crate) fn at(&self, age_ms: u64) -> Instant {
        let age = Duration::from_millis(age_ms).saturating_add(self.behind);
        self.now.checked_sub(age).unwrap_or(self.now)
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The header alone, read before the rest so that a file of another version
/// is told apart from a broken one.
#[derive(Deserialize)]
struct Header {
    format: String,
    version: u32,
}

/// The whole file.
#[derive(Serialize, Deserialize)]
struct StateFile {
    format: String,
    version: u32,
    /// Wall-clock milliseconds since the Unix epoch when it was written.
    written_at_ms: u64,
    tracker: crate::prefixcache::TrackerRecord,
    ledgers: Vec<crate::recompact::LedgerRecord>,
    pinned: Vec<crate::stabilize::PinnedRecord>,
    tool_policies: Vec<crate::toolcompact::PolicyRecord>,
    /// Each conversation's arm of `proxy.output_shaping`. Defaulted so a file
    /// written before the experiment existed still loads.
    #[serde(default)]
    output_shaping: crate::outputshape::ShapingRecord,
    /// Each conversation's `proxy.tools_defer` decision. Defaulted so a file
    /// written before deferral existed still loads.
    #[serde(default)]
    tool_deferral: Vec<crate::toolcompact::PolicyRecord>,
}

/// What loading the state file came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Loaded {
    /// `proxy.state_path` is empty: nothing is saved or loaded.
    Off,
    /// `proxy.state_load = false`: the file, if any, was not read.
    Disabled,
    /// No file at the path; the first start, or a path never written.
    Missing,
    /// The file was read and its conversations restored.
    Restored { conversations: usize },
    /// The file was there and could not be used; nothing was restored.
    Rejected { reason: String },
}

/// Why a save did not happen.
#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error("could not serialize the proxy state: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("could not write the proxy state to {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// The configured path, or `None` when saving is off.
pub(crate) fn path_of(state: &ProxyState) -> Option<PathBuf> {
    let path = state.core.config().proxy.state_path.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Restore `state` from its configured file, if it has one and loading is
/// on. Never fails: whatever goes wrong is logged and leaves `state` empty.
pub(crate) fn load(state: &ProxyState) -> Loaded {
    let Some(path) = path_of(state) else {
        return Loaded::Off;
    };
    if !state.core.config().proxy.state_load {
        tracing::info!(
            path = %path.display(),
            "proxy.state_load is off; starting with no conversations remembered"
        );
        return Loaded::Disabled;
    }
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(
                path = %path.display(),
                "no saved proxy state; starting with no conversations remembered"
            );
            return Loaded::Missing;
        }
        Err(error) => return rejected(&path, format!("unreadable: {error}")),
    };
    let header: Header = match serde_json::from_slice(&bytes) {
        Ok(header) => header,
        Err(error) => return rejected(&path, format!("not a state file: {error}")),
    };
    if header.format != FORMAT {
        return rejected(&path, format!("format {:?}, not {FORMAT:?}", header.format));
    }
    if header.version != VERSION {
        return rejected(
            &path,
            format!(
                "format version {}, and this build reads {VERSION}",
                header.version
            ),
        );
    }
    let file: StateFile = match serde_json::from_slice(&bytes) {
        Ok(file) => file,
        Err(error) => return rejected(&path, format!("damaged: {error}")),
    };
    let clock = Clock::reading(file.written_at_ms);
    let conversations = state.prefixes.restore(file.tracker, &clock);
    state.recompact.restore(file.ledgers, &clock);
    state.stabilizer.restore(file.pinned, &clock);
    state.tool_policies.restore(file.tool_policies, &clock);
    state.output_shaping.restore(file.output_shaping, &clock);
    state.defer_policies.restore(file.tool_deferral, &clock);
    tracing::info!(
        path = %path.display(),
        conversations,
        saved_ago_s = clock.behind.as_secs(),
        "restored the proxy's conversations from its saved state"
    );
    Loaded::Restored { conversations }
}

fn rejected(path: &Path, reason: String) -> Loaded {
    tracing::warn!(
        path = %path.display(),
        %reason,
        "ignoring the saved proxy state; starting with no conversations remembered"
    );
    Loaded::Rejected { reason }
}

/// Write `state` to its configured file. `Ok(None)` when saving is off.
pub(crate) fn save(state: &ProxyState) -> Result<Option<PathBuf>, SaveError> {
    let Some(path) = path_of(state) else {
        return Ok(None);
    };
    // One writer at a time: the timer and the shutdown save share a
    // temporary file name.
    let _writing = state
        .saving
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let clock = Clock::writing();
    let (ledgers, oversized) = state.recompact.snapshot(&clock);
    let file = StateFile {
        format: FORMAT.to_string(),
        version: VERSION,
        written_at_ms: unix_ms(),
        tracker: state.prefixes.snapshot(&clock, &oversized),
        ledgers,
        pinned: state.stabilizer.snapshot(&clock),
        tool_policies: state.tool_policies.snapshot(&clock),
        output_shaping: state.output_shaping.snapshot(&clock),
        tool_deferral: state.defer_policies.snapshot(&clock),
    };
    let bytes = serde_json::to_vec(&file)?;
    write_atomically(&path, &bytes).map_err(|source| SaveError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(Some(path))
}

/// Write `bytes` to a temporary file beside `path`, flush it to disk, and
/// rename it over `path`.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(".tmp-{}", std::process::id()));
    let temporary = PathBuf::from(temporary);
    let written = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            // What tenants' conversations were doing is nobody else's to read.
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}
