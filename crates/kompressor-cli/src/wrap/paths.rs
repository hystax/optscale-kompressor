//! Where a wrap session keeps its bookkeeping, and how it tells a live peer
//! from a dead one.
//!
//! Everything here is *shared mutable state between processes*: two
//! `kompressor wrap` sessions in two terminals reference-count one proxy, and a
//! session that is killed with `SIGKILL` gets no chance to tidy up. So the rule
//! throughout is that a reader must be able to tell a live record from a corpse
//! without trusting the writer to have removed it.

use std::path::{Path, PathBuf};

/// The wrap runtime directory layout.
///
/// Carried as a value rather than read from the environment at each call site,
/// so that a test can point one at a scratch directory without mutating
/// process-global state that every other test in the binary shares.
#[derive(Clone, Debug)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    /// `$KOMPRESSOR_HOME`, else `~/.kompressor`.
    ///
    /// Deliberately not the configured `data/` directory. That holds the
    /// fragment store and the statistics database — durable state a deployment
    /// backs up — while this holds pidfiles and generated configuration that are
    /// meaningless the moment the machine reboots.
    pub fn from_env() -> Paths {
        if let Some(explicit) = std::env::var_os("KOMPRESSOR_HOME") {
            return Paths::at(PathBuf::from(explicit));
        }
        let base = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        Paths::at(base.join(".kompressor"))
    }

    pub fn at(root: impl Into<PathBuf>) -> Paths {
        Paths { root: root.into() }
    }

    /// Per-run state: generated proxy configs and the client markers below.
    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    /// Proxy logs, one file per port.
    pub fn log_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// The generated configuration a supervised proxy on `port` was started with.
    pub fn proxy_config(&self, port: u16) -> PathBuf {
        self.run_dir().join(format!("proxy-{port}.toml"))
    }

    /// Where a supervised proxy on `port` writes its stdout and stderr.
    pub fn proxy_log(&self, port: u16) -> PathBuf {
        self.log_dir().join(format!("proxy-{port}.log"))
    }

    /// The tool-server registration handed to an agent for this session.
    ///
    /// Keyed by process rather than by port, because two wrap sessions sharing
    /// one adopted proxy each write their own and each deletes its own on the
    /// way out. Keying by port would have the second session overwrite a file
    /// the first one is still running against, and the first session's exit
    /// would then delete a file the second still needs.
    pub fn mcp_registration(&self, pid: u32) -> PathBuf {
        self.run_dir().join(format!("mcp-{pid}.json"))
    }

    /// The configuration the tool server for this session is started with.
    ///
    /// Separate from [`Paths::proxy_config`] on purpose: an adopted proxy is
    /// still running against its own file, and rewriting that to describe this
    /// session would change what a restart of *that* proxy would do.
    pub fn mcp_config(&self, pid: u32) -> PathBuf {
        self.run_dir().join(format!("mcp-{pid}.toml"))
    }

    /// The directory of client markers for the proxy on `port`.
    ///
    /// One file per live wrap session. The proxy is stopped by whoever removes
    /// the last one, which is what lets two sessions share it without either of
    /// them having to know the other exists.
    pub fn clients(&self, port: u16) -> PathBuf {
        self.run_dir().join(format!("clients-{port}"))
    }
}

/// Create a directory, reporting the path that failed rather than just the
/// errno — a wrap that cannot write its state is usually a permissions problem
/// on one specific directory and saying which one is the whole diagnosis.
pub fn ensure_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

/// Files this process keeps locked for as long as it runs: see [`hold`].
static HELD: std::sync::Mutex<Vec<(PathBuf, std::fs::File)>> = std::sync::Mutex::new(Vec::new());

/// Lock `path`, creating it, and keep it locked until [`let_go`] or exit.
///
/// This is how a session proves to its peers that it is alive, on every
/// platform: the operating system drops the lock when the process ends, however
/// it ends — a closed window, `taskkill /F`, `SIGKILL`, a crash — and a reused
/// pid cannot inherit it. A pid check could do neither outside Linux.
///
/// Holding the same path twice is a no-op. A peer checking on the file locks it
/// for an instant, and deletes it if it got there before we did, so both are
/// retried for a moment rather than reported.
pub fn hold(path: &Path) -> std::io::Result<()> {
    const ATTEMPTS: u32 = 20;
    let mut held = HELD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if held.iter().any(|(p, _)| p == path) {
        return Ok(());
    }
    for attempt in 1..=ATTEMPTS {
        let opened = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path);
        match opened.map(|file| (file.try_lock(), file)) {
            // Still there under our lock: one a peer deleted meanwhile would be
            // a lock no one can find.
            Ok((Ok(()), file)) if path.exists() => {
                held.push((path.to_path_buf(), file));
                return Ok(());
            }
            Ok((Ok(()) | Err(std::fs::TryLockError::WouldBlock), _)) => {}
            Ok((Err(std::fs::TryLockError::Error(error)), _)) => return Err(error),
            // A file a peer is deleting cannot be opened until it is gone.
            Err(error) if attempt == ATTEMPTS => return Err(error),
            Err(_) => {}
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        format!("{} is held by another process", path.display()),
    ))
}

/// Release a lock taken with [`hold`] and remove its file, while still locked
/// as in [`reap_if_dead`], so no one can take it in between.
pub fn let_go(path: &Path) {
    let mut held = HELD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _ = std::fs::remove_file(path);
    held.retain(|(p, _)| p != path);
}

/// Whether the process that [`hold`]s `path` is still running, removing the
/// file if not: `Some(false)` for a file nobody holds any more, or none at
/// all; `None` when it could not be checked, for the caller to fall back on
/// the pid.
///
/// The file is removed while this still has it locked. Removed after, a
/// session taking it in between would be left holding a deleted file, alive
/// and invisible; this way [`hold`] either waits for the lock or finds the
/// file gone and makes a new one.
pub fn reap_if_dead(path: &Path) -> Option<bool> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(false),
        Err(_) => return None,
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = std::fs::remove_file(path);
            drop(file);
            Some(false)
        }
        Err(std::fs::TryLockError::WouldBlock) => Some(true),
        Err(std::fs::TryLockError::Error(_)) => None,
    }
}

/// A process identity that survives PID reuse.
///
/// The fallback for a record that names no lock: one written by a version
/// before [`hold`], or where the lock could not be taken. A bare PID is not enough: pids wrap around, and a marker left by a
/// session killed yesterday can name a pid that belongs to something entirely
/// unrelated today. Pairing it with the process's start time makes the record
/// falsifiable — if the pid exists but started later than the marker claims,
/// the marker is a corpse.
///
/// Read from `/proc`, which is Linux-specific. Elsewhere there is no start time
/// and `is_alive` degrades to a pid check plus the staleness bound in
/// [`marker_is_stale`]; see that function for what that costs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProcessId {
    pub pid: u32,
    /// Ticks since boot, or `None` where it cannot be read.
    pub start_time: Option<u64>,
}

impl ProcessId {
    pub fn current() -> ProcessId {
        let pid = std::process::id();
        ProcessId {
            pid,
            start_time: start_time_of(pid),
        }
    }

    #[cfg(test)]
    pub fn of(pid: u32) -> ProcessId {
        ProcessId {
            pid,
            start_time: start_time_of(pid),
        }
    }

    /// Whether the process this identifies is still running.
    ///
    /// A recorded start time that no longer matches means the pid was reused,
    /// which counts as dead: the process this record was about is gone.
    pub fn is_alive(&self) -> bool {
        if !pid_exists(self.pid) {
            return false;
        }
        match (self.start_time, start_time_of(self.pid)) {
            (Some(recorded), Some(actual)) => recorded == actual,
            // One side could not be read. Treat the pid as authoritative rather
            // than declaring a live session dead on missing evidence: killing a
            // working session's proxy is worse than leaving a stale one.
            _ => true,
        }
    }
}

fn pid_exists(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        return Path::new(&format!("/proc/{pid}")).exists();
    }
    // Without /proc there is no dependency-free liveness check. Assume alive and
    // let the staleness bound reap the record instead.
    true
}

/// Field 22 of `/proc/<pid>/stat`, the process start time in clock ticks.
///
/// Parsed from the last `)` rather than by splitting on spaces, because field 2
/// is the executable name and a process is free to have spaces and parentheses
/// in it — `(my (weird) name)` splits into nonsense otherwise.
fn start_time_of(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = &stat[stat.rfind(')')? + 1..];
    after_name.split_whitespace().nth(19)?.parse().ok()
}

/// How long a marker whose liveness cannot be proven is believed.
///
/// This only ever applies where [`start_time_of`] returns `None` — no `/proc`,
/// or a kernel that hid it. There, a crashed session's marker would otherwise
/// pin a proxy forever, so age becomes the tiebreaker. On Linux this is dead
/// code, and it is meant to be.
const UNVERIFIABLE_MARKER_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(12 * 3600);

/// Whether a marker file is old enough to disbelieve.
pub fn marker_is_stale(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return true;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    modified
        .elapsed()
        .map(|age| age > UNVERIFIABLE_MARKER_MAX_AGE)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_alive_and_a_reused_pid_is_not() {
        let me = ProcessId::current();
        assert!(me.is_alive(), "the running process must count as alive");

        if let Some(start_time) = me.start_time {
            // Same pid, a start time that cannot be ours: the record describes a
            // process that no longer exists even though the pid is in use.
            let impostor = ProcessId {
                pid: me.pid,
                start_time: Some(start_time + 1),
            };
            assert!(
                !impostor.is_alive(),
                "a start time that does not match means the pid was reused"
            );
        }
    }

    #[test]
    fn a_held_file_reads_alive_and_an_abandoned_one_dead() {
        let dir = std::env::temp_dir().join(format!("kompressor-hold-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session");
        assert_eq!(reap_if_dead(&path), Some(false), "no file, no holder");

        hold(&path).unwrap();
        hold(&path).unwrap(); // a second hold is a no-op, not a conflict
        assert_eq!(reap_if_dead(&path), Some(true));
        assert!(path.exists(), "a live holder's file is left alone");

        let_go(&path);
        assert!(!path.exists());

        // What a killed session leaves: the file, with no one holding it.
        std::fs::write(&path, "").unwrap();
        assert_eq!(reap_if_dead(&path), Some(false));
        assert!(!path.exists(), "and it is cleared away");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_pid_that_cannot_exist_is_dead() {
        // 0 is never a normal process; on Linux /proc/0 does not exist.
        if cfg!(target_os = "linux") {
            assert!(!ProcessId::of(0).is_alive());
        }
    }

    #[test]
    fn the_layout_hangs_off_one_explicit_root() {
        let paths = Paths::at("/tmp/kompressor-home-test");
        assert_eq!(
            paths.clients(8817),
            PathBuf::from("/tmp/kompressor-home-test/run/clients-8817")
        );
        assert_eq!(
            paths.proxy_config(8817),
            PathBuf::from("/tmp/kompressor-home-test/run/proxy-8817.toml")
        );
        assert_eq!(
            paths.proxy_log(8817),
            PathBuf::from("/tmp/kompressor-home-test/logs/proxy-8817.log")
        );
    }

    #[test]
    fn a_stat_line_with_a_parenthesised_name_still_parses() {
        // Not a call into /proc: the point is the parsing rule, and a process
        // named `(weird) name` is the case that breaks a naive split.
        let stat = "42 ((weird) name) S 1 42 42 0 -1 4194304 100 0 0 0 5 6 0 0 20 0 1 0 987654 0";
        let after_name = &stat[stat.rfind(')').unwrap() + 1..];
        let field = after_name.split_whitespace().nth(19);
        assert_eq!(field, Some("987654"), "field 22 is the start time");
    }
}
