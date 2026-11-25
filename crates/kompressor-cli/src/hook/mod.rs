//! Hook mode: an agent calls this binary from its own hook points, once per
//! event, instead of sending its traffic through the proxy.
//!
//! Cursor is the agent this exists for (docs/design/CURSOR.md). Its traffic is
//! Connect RPC to its own backend and the prompt is assembled server-side, so
//! the proxy has nothing to read — but Cursor runs a command, with JSON on
//! standard input, at exactly the moment a tool result enters the
//! conversation. A result compressed then is sent once, smaller, and never
//! changes again.
//!
//! ```text
//! kompressor hook cursor            one event on stdin, one reply on stdout
//! kompressor hook emit FILE         print FILE compressed (the tail of a rewritten shell command)
//! kompressor hook install cursor    register the hooks and the retrieval server in ~/.cursor
//! kompressor hook uninstall cursor  remove exactly what install wrote
//! kompressor hook status cursor     what is installed, what else is, is the store reachable
//! ```
//!
//! The agent-specific half is [`cursor`]; [`shell`] is the command rewrite and
//! [`install`] the registration. What lives here is the part every hook-driven
//! agent shares: a reply that is always valid JSON, a deadline, a one-shot
//! statistics sink, and `emit`.

pub mod cursor;
pub mod install;
pub mod shell;

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kompressor_core::config::Config;
use kompressor_stats::writer::{StatsHandle, StatsSink};

use crate::error::CliResult;

/// Whether this process has written its one reply.
static REPLIED: AtomicBool = AtomicBool::new(false);

/// Write `reply` as the process's one line on standard output.
///
/// Exactly once: a second call is ignored, so a reply raced by the deadline or
/// a panic hook cannot produce two documents. A failed write is ignored — the
/// reader is gone, and nothing printed now would reach it.
pub fn reply(value: &serde_json::Value) {
    if REPLIED.swap(true, Ordering::SeqCst) {
        return;
    }
    let mut text = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    text.push('\n');
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

/// The empty reply: "change nothing".
pub fn nothing() -> serde_json::Value {
    serde_json::json!({})
}

/// Arguments of `kompressor hook cursor`.
pub struct CursorArgs {
    pub config: Option<PathBuf>,
    /// Force shadow mode, whatever the configuration says. Written into the
    /// command by `install --shadow`.
    pub shadow: bool,
}

/// Run `kompressor hook cursor`: read one event, write one reply, exit 0.
///
/// Every failure — unreadable input, a configuration error, a store error, the
/// deadline, a panic — becomes `{}`. On `preToolUse` an invalid reply with
/// exit 0 *blocks the tool call*, and exit 2 blocks it on any hook, so this
/// never returns an error and never lets a panic choose the exit status.
pub fn run_cursor(args: CursorArgs) -> CliResult {
    std::panic::set_hook(Box::new(|info| {
        let _ = writeln!(std::io::stderr(), "kompressor hook: internal error: {info}");
        reply(&nothing());
        std::process::exit(0);
    }));

    let mut input = Vec::new();
    if let Err(error) = std::io::stdin().lock().read_to_end(&mut input) {
        note(&format!("could not read the event: {error}"));
        reply(&nothing());
        return Ok(());
    }

    let config = match Config::load(args.config.as_deref()) {
        Ok(config) => config,
        Err(error) => {
            note(&format!("configuration: {error}"));
            reply(&nothing());
            return Ok(());
        }
    };
    let deadline = config.hooks.cursor.deadline;
    let config_path = args
        .config
        .as_deref()
        .map(|p| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()))
        .or_else(|| config.source.clone());

    let (tx, rx) = std::sync::mpsc::channel();
    let shadow = args.shadow;
    std::thread::Builder::new()
        .name("kompressor-hook".into())
        .spawn(move || {
            let value = cursor::handle(&input, &config, config_path.as_deref(), shadow);
            let _ = tx.send(value);
        })
        .ok();

    match rx.recv_timeout(deadline) {
        Ok(value) => reply(&value),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            note(&format!(
                "gave up after {} ms (hooks.cursor.deadline); the tool result is unchanged",
                deadline.as_millis()
            ));
            reply(&nothing());
            // The worker may be inside the engine or a store write. SQLite
            // commits atomically, so leaving now loses at most this one row.
            std::process::exit(0);
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => reply(&nothing()),
    }
    Ok(())
}

/// One diagnostic line on standard error, where Cursor logs a hook's output.
/// Standard output is the protocol.
pub fn note(message: &str) {
    let _ = writeln!(std::io::stderr(), "kompressor hook: {message}");
}

/// A statistics sink for a process that lives for one event.
///
/// The writer's batch timer is 100 ms and a hook is gone in 40, so whatever is
/// sent has to be committed explicitly before exit: [`OneShot::finish`].
pub struct OneShot {
    pub sink: StatsSink,
    handle: Option<StatsHandle>,
}

impl OneShot {
    /// Open the configured `stats.db`, or a sink that records nothing (with
    /// one line on standard error) when it cannot be opened.
    pub fn open(config: &Config) -> OneShot {
        if !config.stats.enabled {
            return OneShot {
                sink: kompressor_stats::writer::null_sink(),
                handle: None,
            };
        }
        let mut stats = kompressor_stats::config::StatsConfig::at(config.stats.path.clone());
        stats.channel_capacity = config.stats.queue_size;
        // Retention is the long-running process's job. A hook that pruned on
        // every tool call would spend its budget on someone else's chores.
        stats.retention_days = 0;
        stats.context_retention_days = 0;
        if let Some(parent) = config.stats.path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        match kompressor_stats::writer::spawn(&stats) {
            Ok((sink, handle)) => OneShot {
                sink,
                handle: Some(handle),
            },
            Err(error) => {
                note(&format!(
                    "statistics are not being recorded ({}): {error}",
                    config.stats.path.display()
                ));
                OneShot {
                    sink: kompressor_stats::writer::null_sink(),
                    handle: None,
                }
            }
        }
    }

    /// Commit everything sent and stop the writer.
    pub fn finish(self) {
        if let Some(handle) = self.handle
            && let Err(error) = handle.shutdown()
        {
            note(&format!("statistics did not flush: {error}"));
        }
    }
}

/// Bring `stats.db` up to the current schema now, outside any hook.
///
/// A hook lives under a 2-second deadline, and the first one to open a
/// database from an older build would run the migration inside it. v20
/// rebuilds `optimization_events`, which on a database of a few hundred
/// megabytes takes longer than that: every hook would be cut off mid-step,
/// roll back, and the next would start again. So `install` and `wrap`, which
/// have no deadline, do it first.
pub fn prepare_stats(config: &Config) {
    if !config.stats.enabled {
        return;
    }
    if let Some(parent) = config.stats.path.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    let stats = kompressor_stats::config::StatsConfig::at(config.stats.path.clone());
    if let Err(error) = kompressor_stats::StatsWriter::open(&stats) {
        note(&format!(
            "statistics database {} could not be prepared: {error}",
            config.stats.path.display()
        ));
    }
}

/// Where hook mode keeps its own small state: `$KOMPRESSOR_HOME/installed`.
pub fn state_dir() -> PathBuf {
    crate::wrap::paths::Paths::from_env()
        .run_dir()
        .parent()
        .map(|root| root.join("installed"))
        .unwrap_or_else(|| PathBuf::from("installed"))
}

/// The file `hook status` reads to say when a hook last ran.
pub fn last_call_file(agent: &str) -> PathBuf {
    state_dir().join(format!("{agent}-last-call"))
}

/// Note that a hook ran, best-effort. A managed account can disable user
/// hooks, and then `install` succeeds and nothing ever fires: this is what
/// makes that visible instead of silent.
pub fn touch_last_call(agent: &str, event: &str) {
    let path = last_call_file(agent);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let line = format!("{} {event}\n", now_ms());
    let _ = std::fs::write(path, line);
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Arguments of `kompressor hook emit`.
pub struct EmitArgs {
    pub file: PathBuf,
    pub status: i32,
    pub config: Option<PathBuf>,
    pub request: Option<String>,
    pub conversation: Option<String>,
    pub model: Option<String>,
}

/// `kompressor hook emit --status N FILE`: print FILE compressed, delete it,
/// and exit N.
///
/// The tail of a rewritten shell command. Whatever goes wrong, the captured
/// output is printed unchanged and the exit status is still `N`: the command
/// ran, and its result must reach the model one way or the other. The file is
/// deleted only once something has been printed from it.
pub fn run_emit(args: EmitArgs) -> ! {
    let status = args.status;
    let code = |s: i32| -> i32 { s.clamp(0, 255) };
    let path = args.file.clone();

    let Ok(meta) = std::fs::metadata(&path) else {
        note(&format!(
            "the captured output {} is missing",
            path.display()
        ));
        std::process::exit(code(status));
    };
    let config = Config::load(args.config.as_deref()).ok();
    let limit = config
        .as_ref()
        .map_or(64 * 1024 * 1024, |c| c.hooks.cursor.shell.max_capture_bytes);

    let printed = if meta.len() > limit {
        copy_through(&path)
    } else {
        match std::fs::read(&path) {
            Err(error) => {
                note(&format!("could not read {}: {error}", path.display()));
                false
            }
            Ok(bytes) => match (String::from_utf8(bytes), config) {
                (Ok(text), Some(config)) if config.hooks.cursor.enabled => {
                    let out = std::panic::catch_unwind(|| compress_capture(&text, &config, &args))
                        .unwrap_or_else(|_| text.clone());
                    write_out(out.as_bytes())
                }
                (Ok(text), _) => write_out(text.as_bytes()),
                (Err(raw), _) => write_out(raw.as_bytes()),
            },
        }
    };
    if printed {
        let _ = std::fs::remove_file(&path);
    }
    std::process::exit(code(status));
}

fn write_out(bytes: &[u8]) -> bool {
    let mut out = std::io::stdout().lock();
    out.write_all(bytes).and_then(|()| out.flush()).is_ok()
}

/// Print a file too large to hold in memory, as it is.
fn copy_through(path: &Path) -> bool {
    match std::fs::File::open(path) {
        Ok(mut file) => {
            let mut out = std::io::stdout().lock();
            std::io::copy(&mut file, &mut out).is_ok() && out.flush().is_ok()
        }
        Err(error) => {
            note(&format!("could not read {}: {error}", path.display()));
            false
        }
    }
}

/// Compress captured shell output, or return it unchanged.
///
/// The store is tried first; when it cannot be opened — a sandbox that denies
/// writes outside the workspace, say — the engine runs `lossless`, so nothing
/// is elided behind a handle that could not be kept, and nothing is recorded.
fn compress_capture(text: &str, config: &Config, args: &EmitArgs) -> String {
    let identity = kompressor_core::Identity::anonymous();
    let request = kompressor_core::RequestInfo {
        request_id: args.request.as_deref().unwrap_or("cursor-shell"),
        api_family: None,
        model: args.model.as_deref(),
        provider: Some("cursor"),
    };
    let attribution = kompressor_core::Attribution {
        tool: Some("Shell"),
        conversation: args.conversation.as_deref(),
        shadow: false,
        inline_limit_bytes: Some(crate::hook::cursor::SPILL_BYTES),
    };
    let (store, lossless) = match kompressor_core::build_ref_store(config) {
        Ok(store) => (store, false),
        Err(error) => {
            note(&format!(
                "the fragment store is unreachable ({error}); output compressed losslessly"
            ));
            (
                kompressor_core::build_ref_store(&memory_store(config))
                    .expect("an in-memory store always opens"),
                true,
            )
        }
    };
    let stats = if lossless {
        None
    } else {
        Some(OneShot::open(config))
    };
    let sink = stats
        .as_ref()
        .map_or_else(kompressor_stats::writer::null_sink, |s| s.sink.clone());
    let core = kompressor_core::AppCore::new(config.clone(), store, sink);
    let mut options = config.engine.clone();
    if lossless {
        options.mode = kompressor_engine::types::Mode::Lossless;
    }
    let (out, _) = core.compress_raw_attributed(
        &identity,
        text,
        None,
        &request,
        &options,
        kompressor_stats::events::Surface::Hook,
        &attribution,
    );
    drop(core);
    if let Some(stats) = stats {
        stats.finish();
    }
    if out.len() < text.len() {
        out
    } else {
        text.to_string()
    }
}

fn memory_store(config: &Config) -> Config {
    let mut config = config.clone();
    config.refs.backend = kompressor_core::config::RefsBackend::Memory;
    config
}

/// How old an empty capture file must be before it is taken for an orphan.
///
/// The hook makes the file before Cursor decides whether to run the command,
/// so a refused command leaves it behind, empty — measured under an
/// allow-list on CLI 2026.11.22. A command that is still running and has
/// printed nothing has an empty file too, so only files older than any
/// command a session would wait on are removed.
const ORPHAN_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Remove empty capture files older than [`ORPHAN_AGE`]. Best-effort.
fn sweep_captures(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("kompressor-cursor-") && name.ends_with(".out")) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let old = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > ORPHAN_AGE);
        if meta.is_file() && meta.len() == 0 && old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// A capture file for one shell call: under the temporary directory, owner
/// only, unique per tool call. Made by the hook itself, before it returns a
/// rewrite, so a temporary directory that cannot be written is found here —
/// where declining costs nothing — and not by the shell, where the redirect
/// would fail and the command would not run at all.
pub fn create_capture(tool_use_id: Option<&str>) -> Option<PathBuf> {
    let dir = std::env::temp_dir();
    sweep_captures(&dir);
    let id: String = tool_use_id
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    let base = if id.is_empty() {
        format!("{}-{}", std::process::id(), now_ms())
    } else {
        id
    };
    for attempt in 0..8u32 {
        let name = if attempt == 0 {
            format!("kompressor-cursor-{base}.out")
        } else {
            format!("kompressor-cursor-{base}-{attempt}.out")
        };
        let path = dir.join(name);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(_) => return Some(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// The running binary, made absolute: hooks and rewritten commands run from
/// directories this process does not choose, with a `PATH` it does not set.
pub fn exe() -> Option<PathBuf> {
    std::env::current_exe().ok()
}

/// How long ago `ms` was, for humans.
pub fn ago(ms: i64) -> String {
    let secs = ((now_ms() - ms) / 1000).max(0) as u64;
    let d = Duration::from_secs(secs);
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", d.as_secs() / 60),
        3600..=86_399 => format!("{}h ago", d.as_secs() / 3600),
        _ => format!("{}d ago", d.as_secs() / 86_400),
    }
}
