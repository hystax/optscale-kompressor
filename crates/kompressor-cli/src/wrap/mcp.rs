//! Offering the retrieval tools to an agent that can be told about them on the
//! command line.
//!
//! # Why the agent needs a tool server at all
//!
//! On the proxy, retrieval is in-band and invisible: the engine injects a
//! `optscale_kompressor_retrieve` tool into the request, the model calls it, and
//! [`retrieve`] answers it inside the same round trip. The client never sees
//! the tool and never needs to — which is exactly the limit. That loop only
//! exists while a request is in flight through the proxy, so a handle the model
//! meets anywhere else is dead text: in a file it reads later, in a transcript
//! from yesterday, in output a sub-agent captured. The marker says a fragment
//! can be fetched, and nothing in the session can fetch it.
//!
//! A tool server closes that gap. The agent holds a connection for the whole
//! session, so `optscale_kompressor_retrieve` is callable whenever the model meets a
//! handle rather than only while the proxy is mid-turn.
//!
//! # Why nothing the user owns is edited
//!
//! The obvious implementation registers the server in the agent's own MCP
//! configuration. It is also the wrong one, and the reason is worth stating
//! because the pull towards it is strong: that file is durable, so every way a
//! session can fail to reach its cleanup — SIGKILL, a lost terminal, a crash —
//! leaves a registration behind, pointing at a binary that may since have moved.
//! The residue outlives the thing that explains it.
//!
//! Claude Code takes `--mcp-config FILE`, so the registration can instead live
//! in this session's own run directory and be named on the command line. The
//! agent gets the same tools; the user's configuration is never opened, let
//! alone written. A session that dies in a way no handler can catch leaves a
//! stray file under `~/.kompressor/run` and nothing else — no entry in a config
//! this command did not create, and nothing for a later `unwrap` to find.
//!
//! This is why [`Agent::mcp`] is a separate field from [`Agent::config`] rather
//! than another [`ConfigFile`] variant: those edit a file the user owns and
//! carry the whole claim-and-restore apparatus in [`settings`] because they
//! must. This carries none of it, because it has nothing to restore.
//!
//! [`retrieve`]: kompressor_proxy
//! [`Agent::mcp`]: super::agents::Agent::mcp
//! [`Agent::config`]: super::agents::Agent::config
//! [`ConfigFile`]: super::agents::ConfigFile
//! [`settings`]: super::settings

use std::path::{Path, PathBuf};

use kompressor_core::Config;
use serde_json::{Value, json};

use super::agents::McpWiring;
use super::paths::Paths;
use crate::error::CliError;

/// The name the server is registered under.
///
/// Agents namespace tool names by server, so this is user-visible: in Claude
/// Code the tool reads
/// `mcp__optscale_kompressor__optscale_kompressor_retrieve`. The name is the
/// product a user sees in a tool list, so they can tell what put it there.
pub const SERVER_NAME: &str = "optscale_kompressor";

/// A registration written for one session, and the file it lives in.
pub struct Registration {
    /// The JSON handed to the agent.
    file: PathBuf,
    /// The configuration the tool server is started with.
    config: PathBuf,
    /// The argument pair that points the agent at `file`.
    args: Vec<String>,
}

impl Registration {
    /// Arguments to insert before the user's own.
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// The registration file, for reporting.
    pub fn path(&self) -> &Path {
        &self.file
    }
}

/// Write this session's registration, if the agent can be told about one.
///
/// `base` is the configuration the wrap resolved, and it is written out beside
/// the registration rather than referenced: the tool server opens the fragment
/// store this names, and a store that differs from the proxy's would resolve
/// none of the handles the proxy minted. Passing the path of a file the user
/// may edit mid-session would reintroduce exactly that risk.
pub fn register(
    wiring: Option<McpWiring>,
    paths: &Paths,
    base: &Config,
) -> Result<Option<Registration>, CliError> {
    let Some(McpWiring::Flag { flag }) = wiring else {
        return Ok(None);
    };

    let dir = paths.run_dir();
    super::paths::ensure_dir(&dir)
        .map_err(|e| CliError::runtime(&format!("could not create {}", dir.display()), e))?;

    let pid = std::process::id();
    let config_path = paths.mcp_config(pid);
    let text = toml::to_string_pretty(&anchored(base)?)
        .map_err(|e| CliError::runtime("could not serialise the tool-server configuration", e))?;
    std::fs::write(&config_path, text)
        .map_err(|e| CliError::runtime(&format!("could not write {}", config_path.display()), e))?;

    let file = paths.mcp_registration(pid);
    let document = document(&exe()?, &config_path);
    let rendered = serde_json::to_string_pretty(&document)
        .map_err(|e| CliError::runtime("could not render the tool-server registration", e))?;
    std::fs::write(&file, rendered + "\n")
        .map_err(|e| CliError::runtime(&format!("could not write {}", file.display()), e))?;

    Ok(Some(Registration {
        args: vec![flag.to_string(), file.display().to_string()],
        file,
        config: config_path,
    }))
}

/// Remove this session's registration.
///
/// Best-effort by design. The files are in this command's own run directory and
/// name a dead pid, so one left behind by a kill is inert clutter rather than
/// state anything will act on — worth cleaning up, never worth failing a
/// session's exit over.
pub fn release(registration: &Registration) {
    let _ = std::fs::remove_file(&registration.file);
    let _ = std::fs::remove_file(&registration.config);
}

/// The configuration with its store path made absolute.
///
/// The default store path is relative — `./data/refs.db` — and relative to
/// *the process's* working directory, which for every other surface is this
/// command's own. The tool server is the exception: the agent spawns it, from
/// whatever directory the agent happens to be in, and nothing says that is the
/// directory the proxy was started in.
///
/// Left alone, that difference is the worst kind of bug this codebase has. The
/// tool server would open a *different* SQLite file, create it happily, answer
/// every `tools/list`, and then miss every single handle — and a miss is a
/// legitimate answer here (a fragment can genuinely have expired), so it would
/// be reported to the model as "the data is gone" rather than as a failure.
/// Resolving the path here makes the two processes name the same file by
/// construction.
fn anchored(base: &Config) -> Result<Config, CliError> {
    let mut config = base.clone();
    if config.refs.path.is_relative() {
        let here = std::env::current_dir().map_err(|e| {
            CliError::runtime(
                "could not read the working directory, so the fragment store the tool server \
                 opens cannot be pinned to the one the proxy writes",
                e,
            )
        })?;
        config.refs.path = here.join(&config.refs.path);
    }
    Ok(config)
}

/// The MCP document an agent reads.
///
/// Split out from [`register`] so the shape can be asserted without writing
/// files or resolving the running executable.
fn document(binary: &Path, config: &Path) -> Value {
    json!({
        "mcpServers": {
            SERVER_NAME: {
                "type": "stdio",
                "command": binary.display().to_string(),
                "args": ["mcp", "serve", "--config", config.display().to_string()],
                // Deliberately empty rather than absent. The tool server reads
                // its tenant from the environment it is launched in, and it
                // inherits the agent's — which is the same environment this
                // wrap is running in, so a deployment that set a key already
                // has it. Naming values here would instead copy a credential
                // into a file on disk.
                "env": {},
            }
        }
    })
}

/// The path of the running binary.
///
/// An agent launches the tool server itself, from a working directory this
/// command does not choose, so the registration has to name an absolute path.
/// `argv[0]` would be whatever the user typed — often a bare `kompressor`,
/// resolved against a `PATH` the agent may not share.
fn exe() -> Result<PathBuf, CliError> {
    std::env::current_exe().map_err(|e| {
        CliError::runtime(
            "could not find this executable, so the retrieval tools cannot be offered to the agent",
            e,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registration_names_this_binary_and_this_sessions_config() {
        let document = document(
            Path::new("/opt/bin/kompressor"),
            Path::new("/run/mcp-7.toml"),
        );
        let server = &document["mcpServers"][SERVER_NAME];

        assert_eq!(server["command"], "/opt/bin/kompressor");
        assert_eq!(server["type"], "stdio");
        // The store the tool server opens is the whole point: a handle minted
        // by the proxy resolves here only because both read this file.
        assert_eq!(
            server["args"],
            json!(["mcp", "serve", "--config", "/run/mcp-7.toml"])
        );
    }

    #[test]
    fn no_credential_is_copied_into_the_registration() {
        let document = document(
            Path::new("/opt/bin/kompressor"),
            Path::new("/run/mcp-7.toml"),
        );
        assert_eq!(document["mcpServers"][SERVER_NAME]["env"], json!({}));
    }

    #[test]
    fn an_agent_with_no_mcp_wiring_is_offered_nothing() {
        let paths = Paths::at(std::env::temp_dir().join("kompressor-mcp-none"));
        let registration = register(None, &paths, &Config::default()).unwrap();
        assert!(
            registration.is_none(),
            "an agent that cannot be told about a tool server must not have one written for it"
        );
    }

    #[test]
    fn the_tool_servers_store_is_pinned_so_the_agents_directory_cannot_change_it() {
        let mut config = Config::default();
        assert!(
            config.refs.path.is_relative(),
            "this guards a default that is relative; if that changed the guard needs rewriting"
        );

        let pinned = anchored(&config).unwrap().refs.path;
        assert!(
            pinned.is_absolute(),
            "a relative store path would follow the agent's working directory, and every \
             handle the proxy minted would silently miss"
        );
        assert!(pinned.ends_with("data/refs.db"));

        // An absolute path is the operator's decision and is passed through.
        // Built from the temp directory rather than written as
        // `/var/lib/...`: that has no drive letter, so on Windows it is
        // relative and gets anchored like any other.
        let absolute = std::env::temp_dir().join("kompressor").join("refs.db");
        config.refs.path = absolute.clone();
        assert_eq!(anchored(&config).unwrap().refs.path, absolute);
    }

    #[test]
    fn registering_writes_both_files_and_releasing_removes_them() {
        let root = std::env::temp_dir().join(format!("kompressor-mcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::at(&root);

        let wiring = Some(McpWiring::Flag {
            flag: "--mcp-config",
        });
        let registration = register(wiring, &paths, &Config::default())
            .unwrap()
            .expect("an agent with flag wiring is offered a tool server");

        assert!(registration.path().exists());
        assert!(registration.config.exists());
        assert_eq!(registration.args()[0], "--mcp-config");
        assert_eq!(
            registration.args()[1],
            registration.path().display().to_string()
        );

        release(&registration);
        assert!(
            !registration.path().exists() && !registration.config.exists(),
            "a session that exits normally leaves nothing behind"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
