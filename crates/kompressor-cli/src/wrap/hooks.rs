//! The hooks branch of `wrap`: an agent that is handed hooks calling this
//! binary instead of being pointed at a proxy.
//!
//! `wrap cursor-agent` writes a plugin directory under `$KOMPRESSOR_HOME/run/`
//! — hooks, the retrieval server, and a configuration with its store pinned —
//! runs `cursor-agent --plugin-dir <dir> <args>` in the foreground, and removes
//! the directory when the agent exits. No proxy is started and no port is
//! resolved: every step of the wrap diagram above "launch" is skipped.
//!
//! A session killed without cleanup leaves an inert directory under `run/`
//! and nothing else, which is the property `--mcp-config` buys for Claude
//! Code. There is no `unwrap cursor-agent`.

use std::path::{Path, PathBuf};

use kompressor_core::config::Config;

use super::agents::Agent;
use super::paths::Paths;
use crate::error::{CliError, CliResult};

/// The executable on `PATH`, trying each name in turn.
pub fn find_on_path(names: &[&str], path: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let dirs: Vec<PathBuf> = path.iter().flat_map(std::env::split_paths).collect();
    for name in names {
        for dir in &dirs {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
            #[cfg(windows)]
            for ext in ["exe", "cmd", "bat"] {
                let candidate = dir.join(name).with_extension(ext);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// The arguments the agent is started with: the flag and directory first,
/// then the user's own.
pub fn agent_args(flag: &str, plugin: Option<&Path>, user: &[String]) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(dir) = plugin {
        args.push(flag.to_string());
        args.push(dir.display().to_string());
    }
    args.extend(user.iter().cloned());
    args
}

/// Run an agent wired with hooks.
pub fn run(
    agent: &'static Agent,
    flag: &'static str,
    user_args: &[String],
    config: Config,
) -> CliResult {
    let binary =
        find_on_path(&[agent.binary, "agent"], std::env::var_os("PATH")).ok_or_else(|| {
            CliError::Usage(format!(
                "{} is not on PATH (tried `{}` and `agent`). Install the Cursor CLI first: \
             https://cursor.com/docs/cli",
                agent.name, agent.binary
            ))
        })?;

    let paths = Paths::from_env();
    let started_ms = super::now_ms();
    let plugin = if crate::hook::install::installed_for_user() {
        say!(
            "kompressor: Cursor hooks are already installed (in ~/.cursor or this workspace; \
             `kompressor hook status cursor`), and the CLI reads those too; not adding a \
             second set for this session"
        );
        None
    } else {
        Some(write_session_plugin(&paths, &config)?)
    };

    let args = agent_args(flag, plugin.as_ref().map(|p| p.dir.as_path()), user_args);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::runtime("could not start the runtime", e))?;
    let status = runtime.block_on(async {
        let mut child = tokio::process::Command::new(&binary)
            .args(&args)
            .spawn()
            .map_err(|e| CliError::runtime(&format!("could not start {}", binary.display()), e))?;
        loop {
            tokio::select! {
                status = child.wait() => {
                    return status.map_err(|e| CliError::runtime("waiting for the agent", e));
                }
                // The agent shares the terminal and handles Ctrl-C itself;
                // this process only has to survive it to clean up.
                _ = tokio::signal::ctrl_c() => {}
            }
        }
    });

    if let Some(plugin) = &plugin {
        plugin.release();
    }
    let status = status?;
    // An agent that failed to start (a sandbox the system cannot provide,
    // say) has said why itself; a line about hooks would only mislead.
    report(&config, started_ms, plugin.is_some() && status.success());
    super::exit_of(status)
}

/// This session's plugin directory and pinned configuration.
pub struct SessionPlugin {
    pub dir: PathBuf,
    pub config: PathBuf,
}

impl SessionPlugin {
    pub fn release(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = std::fs::remove_file(&self.config);
    }
}

/// Write the plugin for one session, keyed by this process.
pub fn write_session_plugin(paths: &Paths, config: &Config) -> Result<SessionPlugin, CliError> {
    let run = paths.run_dir();
    let pid = std::process::id();
    let dir = run.join(format!("cursor-plugin-{pid}"));
    let config_path = run.join(format!("cursor-{pid}.toml"));
    let mut pinned = config.clone();
    let here = std::env::current_dir()
        .map_err(|e| CliError::runtime("could not read the working directory", e))?;
    if pinned.refs.path.is_relative() {
        pinned.refs.path = here.join(&pinned.refs.path);
    }
    if pinned.stats.path.is_relative() {
        pinned.stats.path = here.join(&pinned.stats.path);
    }
    crate::hook::install::write_pinned(&pinned, &config_path)?;
    crate::hook::prepare_stats(&pinned);
    let exe = crate::hook::exe()
        .ok_or_else(|| CliError::Runtime("could not find this executable".into()))?;
    crate::hook::install::write_plugin(&dir, &exe, &config_path, config.hooks.cursor.shadow)
        .map_err(|e| CliError::runtime(&format!("could not write {}", dir.display()), e))?;
    Ok(SessionPlugin {
        dir,
        config: config_path,
    })
}

/// Whether any hook ran since `started_ms`, from the mark each one leaves.
fn hooks_ran_since(started_ms: i64) -> bool {
    std::fs::read_to_string(crate::hook::last_call_file("cursor"))
        .ok()
        .and_then(|text| text.split_whitespace().next()?.parse::<i64>().ok())
        .is_some_and(|ms| ms >= started_ms)
}

/// One line about what the session's hooks did, from `stats.db`.
fn report(config: &Config, started_ms: i64, offered: bool) {
    if !config.stats.enabled || !config.stats.path.exists() {
        return;
    }
    let Ok(reader) = kompressor_stats::reader::StatsReader::open(&config.stats.path) else {
        return;
    };
    let Ok((rows, removed)) = reader.hook_session(started_ms) else {
        return;
    };
    if rows == 0 {
        if offered {
            // Whether Cursor called the hooks at all is a different failure
            // from hooks that ran and could record nothing — an unwritable
            // store, a deadline too short — and the two want different fixes.
            if hooks_ran_since(started_ms) {
                say!(
                    "kompressor: the hooks ran but recorded nothing: every result was too small \
                     or not text, or the store or the deadline failed (Cursor's hook log has \
                     the line). `kompressor hook status cursor` checks the store"
                );
            } else {
                say!(
                    "kompressor: no tool result reached the hooks this session. If tools ran, \
                     Cursor did not call them: `kompressor hook status cursor`"
                );
            }
        }
        return;
    }
    say!("kompressor: {rows} tool result(s) seen by the hooks, ~{removed} tokens removed");
}
