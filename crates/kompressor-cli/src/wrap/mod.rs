//! `kompressor wrap` — run a coding agent with its API traffic compressed.
//!
//! The proxy already does the work; the friction is everything around it.
//! Someone who wants compression has to start a proxy, know which environment
//! variable their agent reads, know which URL shape that variable wants, mount
//! an upstream for their provider, remember to stop the proxy afterwards, and
//! do it again in every terminal. Each step is small and every one of them is a
//! way to end up talking to the provider directly while believing otherwise.
//!
//! So: `kompressor wrap claude` — and the agent runs exactly as it would have,
//! with its traffic compressed on the way out.
//!
//! ```text
//! wrap claude ── resolve port ──▶ serving? ──yes──▶ adopt (never stopped by us)
//!                                     │no
//!                                     ▼
//!                       write config (user's + upstreams)
//!                                     │
//!                       spawn `serve --proxy`, own process group
//!                                     │
//!                            poll /healthz until ready
//!                                     │
//!        ┌────────────────────────────┴───────────────────────────┐
//!        ▼                                                         ▼
//!   launch: env + run agent in foreground              attach: write settings file,
//!        │                                                     wait for Ctrl-C
//!        ▼                                                         ▼
//!   agent exits ──────────▶ unregister; stop proxy only if we started it
//!                            and no other live session still holds it
//! ```
//!
//! The two modes exist because environment variables only reach a process this
//! command launches. An agent hosted inside an editor is started by the editor,
//! so for those the base URL has to go into the settings file instead — see
//! [`settings`], which is where the genuinely awkward part lives.

/// `eprintln!` that cannot panic.
///
/// A write to a console that has been closed fails, and `eprintln!` turns the
/// failure into a panic. The cleanup after a closed window runs exactly then,
/// and a panic would abandon it halfway, with the agent's config still pointed
/// at a proxy about to go.
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

pub mod agents;
pub mod configfile;
pub mod mcp;
pub mod paths;
pub mod proxy;
pub mod settings;

use std::path::PathBuf;

use kompressor_core::config::Config;

use crate::error::{CliError, CliResult};
use agents::{Agent, Endpoint};
use paths::Paths;

/// Arguments of `kompressor wrap`.
pub struct WrapArgs {
    pub agent: Option<String>,
    pub args: Vec<String>,
    pub port: Option<u16>,
    pub config: Option<PathBuf>,
    pub mode: Option<String>,
    pub no_proxy: bool,
    pub attach: bool,
    pub print: bool,
    pub env: Vec<String>,
    pub list: bool,
}

/// Arguments of `kompressor unwrap`.
pub struct UnwrapArgs {
    pub agent: String,
}

/// The port to run on, in the order a user would expect it to be decided.
fn resolve_port(args: &WrapArgs, config: &Config) -> Result<u16, CliError> {
    if let Some(port) = args.port {
        return Ok(port);
    }
    let configured = config
        .server
        .listen
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8817);
    Ok(configured)
}

/// The base-URL variables to set: the agent's, plus any `--env` override.
///
/// An override wins over a catalogue entry of the same name, so a tool whose
/// variable has been renamed upstream can be corrected without waiting for the
/// catalogue to catch up.
///
/// `already_set` is passed through to [`fixed_env_to_set`], for the same
/// reason it takes one: reading the real environment here made the result
/// depend on the shell the tests ran in — under Claude Code, which sets
/// `ENABLE_TOOL_SEARCH` itself, the catalogue test failed.
fn resolve_env(
    agent: Option<&Agent>,
    overrides: &[String],
    port: u16,
    already_set: impl Fn(&str) -> bool,
) -> Result<Vec<(String, String)>, CliError> {
    let mut resolved: Vec<(String, Endpoint)> = agent
        .map(|a| {
            a.env
                .iter()
                .map(|(k, s)| ((*k).to_string(), *s))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for spec in overrides {
        let (name, shape) = agents::parse_env_override(spec).map_err(CliError::Usage)?;
        resolved.retain(|(existing, _)| existing != &name);
        resolved.push((name, shape));
    }
    // An agent wired through its config file routes nothing by variable, and
    // that is correct rather than a mistake to report.
    let by_file = agent.is_some_and(|a| a.config.is_some());
    if resolved.is_empty() && !by_file {
        return Err(CliError::Usage(
            "nothing would be routed through the proxy: name an agent, or pass \
             --env NAME=root|v1 to say which variable your tool reads"
                .into(),
        ));
    }
    let mut env: Vec<(String, String)> = resolved
        .into_iter()
        .map(|(name, shape)| (name, shape.url(port)))
        .collect();

    env.extend(fixed_env_to_set(agent, already_set));
    Ok(env)
}

/// The fixed variables this wrap should set, given what is already set.
///
/// `already_set` rather than a direct read of the environment so the rule is
/// testable without mutating a process-global the rest of the suite shares —
/// tests run in threads, and one setting `ENABLE_TOOL_SEARCH` would decide
/// another's result.
fn fixed_env_to_set(
    agent: Option<&Agent>,
    already_set: impl Fn(&str) -> bool,
) -> Vec<(String, String)> {
    agent
        .map(|a| a.fixed_env)
        .unwrap_or(&[])
        .iter()
        // A value the user chose is a decision. Overriding it would change
        // behaviour nobody asked this command to change.
        .filter(|(name, _)| !already_set(name))
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect()
}

/// Run `kompressor wrap`.
pub fn wrap(args: WrapArgs) -> CliResult {
    if args.list {
        println!("Agents kompressor wrap knows:\n{}", Agent::listing());
        println!(
            "\nAnything else: kompressor wrap --env NAME=root|v1 -- <command> [args]\n\
             `root` for a base URL the client appends /v1/… to, `v1` for one that already ends in /v1."
        );
        return Ok(());
    }

    let agent = match args.agent.as_deref() {
        Some(name) => Some(Agent::find(name).ok_or_else(|| {
            CliError::Usage(format!(
                "unknown agent {name:?}. Known agents:\n{}\n\nFor anything else, \
                 pass --env NAME=root|v1 and the command after `--`.",
                Agent::listing()
            ))
        })?),
        None => None,
    };

    // Before anything is started or written. Everything below has side effects —
    // a config file edited, a proxy spawned — and doing them for a session that
    // cannot possibly compress is worse than doing nothing at all.
    if let Some(agents::Wiring::Unsupported { why }) = agent.map(|a| a.wiring) {
        let name = agent.map(|a| a.name).unwrap_or_default();
        let mut message = format!("{name} cannot be wrapped.\n");
        for line in wrap_note(why, 74) {
            message.push_str("  ");
            message.push_str(&line);
            message.push('\n');
        }
        return Err(CliError::Usage(message));
    }

    let mut config =
        Config::load(args.config.as_deref()).map_err(|e| CliError::Usage(e.to_string()))?;
    if let Some(mode) = &args.mode {
        config.engine.mode = mode
            .parse()
            .map_err(|_| CliError::Usage(format!("unknown mode {mode:?}")))?;
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::runtime("could not start the runtime", e))?;
    runtime.block_on(run(args, agent, config))
}

async fn run(args: WrapArgs, agent: Option<&'static Agent>, config: Config) -> CliResult {
    let client = reqwest::Client::new();
    let paths = Paths::from_env();
    let preferred = resolve_port(&args, &config)?;

    // Adopt a proxy that is already serving. Starting a second one on another
    // port would work and would also quietly split the fragment store, so a
    // handle minted in one terminal would not resolve in the other.
    let already = proxy::is_serving(&client, preferred).await;
    let port = if already || args.no_proxy {
        preferred
    } else {
        proxy::find_free_port(preferred)?
    };

    let env = resolve_env(agent, &args.env, port, |name| {
        std::env::var_os(name).is_some()
    })?;

    if args.print {
        for (name, value) in &env {
            println!("export {name}={value}");
        }
        return Ok(());
    }

    // Listened for from here on, before anything is started. A key or a closed
    // window that found no listener would end this process on the spot, and a
    // proxy it had started would run on in a console or process group of its
    // own, where nothing stops it.
    let mut stopping = Termination::new();
    let mut interrupts = Interrupts::new();

    proxy::register_client(&paths, port)
        .map_err(|e| CliError::runtime("could not record this session", e))?;

    let started = if already {
        say!("kompressor: using the proxy already listening on 127.0.0.1:{port}");
        Some(proxy::Proxy {
            port,
            ownership: proxy::Ownership::Adopted,
            log_path: paths.proxy_log(port),
            child: None,
        })
    } else if args.no_proxy {
        proxy::unregister_client(&paths, port);
        return Err(CliError::Runtime(format!(
            "--no-proxy was given but nothing is serving on 127.0.0.1:{port}"
        )));
    } else {
        let upstreams = agent
            .map(|a| agents::upstreams_for(a.providers))
            .unwrap_or_default();
        let config_path = proxy::write_config(&paths, &config, upstreams, port)?;
        // Giving up on the start stops the proxy it spawned; see `proxy::start`.
        let proxy = tokio::select! {
            proxy = proxy::start(&paths, &client, port, &config_path) => proxy,
            _ = stopping.recv() => Err(CliError::Runtime(
                "stopped while the proxy was starting".into(),
            )),
            _ = interrupts.recv() => Err(CliError::Runtime(
                "interrupted while the proxy was starting".into(),
            )),
        };
        match proxy {
            Ok(proxy) => {
                say!(
                    "kompressor: proxy on 127.0.0.1:{port} (mode {}), log {}",
                    config.engine.mode,
                    proxy.log_path.display()
                );
                Some(proxy)
            }
            Err(e) => {
                proxy::unregister_client(&paths, port);
                return Err(e);
            }
        }
    };

    // Edited before the agent starts and put back after it exits, so a wrapped
    // session never outlives its own changes to a file the user owns.
    let (injected, flags) = match agent.and_then(|a| a.config) {
        Some(kind) => match claim_config(kind, port) {
            Ok(wiring) => wiring,
            Err(e) => {
                shutdown(&paths, started, port, stopping.hurried()).await;
                return Err(e);
            }
        },
        None => (None, Vec::new()),
    };

    let started_ms = now_ms();
    let manual_wiring = matches!(agent.map(|a| a.wiring), Some(agents::Wiring::Manual { .. }));

    // Only the launch path can carry the offer: `--attach` and `Manual` never
    // build a command line, because the agent is started by an editor or by the
    // user. Registering durably so those two could see it as well is exactly
    // the trade `mcp` exists to refuse, so they simply go without.
    let offered = if manual_wiring || args.attach {
        None
    } else {
        match mcp::register(agent.and_then(|a| a.mcp), &paths, &config) {
            Ok(offered) => offered,
            Err(e) => {
                if let Some(target) = &injected {
                    release_config(target);
                }
                shutdown(&paths, started, port, stopping.hurried()).await;
                return Err(e);
            }
        }
    };

    let session = async {
        match agent.map(|a| a.wiring) {
            Some(agents::Wiring::Manual { where_ }) => {
                manual(agent, where_, &env, &mut interrupts).await
            }
            _ if args.attach => attach(agent, &env, &mut interrupts).await,
            _ => {
                let offered = offered.as_ref();
                launch(agent, &args.args, &flags, &env, offered, &mut interrupts).await
            }
        }
    };
    // A wrap that edits a file outside this repository has to put it back even
    // when it is not allowed to finish. Ctrl-C already belongs to the agent
    // (see `launch`); this is the other way a session ends early — a `timeout`,
    // a terminal closing, a supervisor stopping the job — and without it the
    // user is left with a config file pointing at a proxy that is gone.
    // SIGKILL still cannot be caught, which is what `kompressor unwrap` is for.
    let result = tokio::select! {
        result = session => result,
        _ = stopping.recv() => Err(CliError::Runtime(
            "stopped before the agent finished; everything this session changed has \
             been put back"
                .into(),
        )),
    };

    if let Some(target) = &injected {
        release_config(target);
    }
    if let Some(offered) = &offered {
        mcp::release(offered);
    }

    // Stop first: `proxy::stop` asks the proxy to go and waits, and `serve` drains its
    // statistics queue on the way out — so reading after it has finished is the
    // only way to see the last request's rows.
    shutdown(&paths, started, port, stopping.hurried()).await;
    report_session(&config, started_ms, manual_wiring);
    // Held to here, past the cleanup; see `Termination`.
    drop(stopping);
    result
}

/// The ways this process is asked to stop from outside: SIGTERM and SIGHUP on
/// Unix; the console window closing, or the user logging off, on Windows. A
/// restart or shutdown reaches an interactive process as a logoff: Windows
/// sends its shutdown event to services only.
///
/// Made before the session starts and kept until its cleanup is done. A console
/// tells its processes about a close one at a time, the agent first, so this
/// one may hear of it only once the agent has gone and the cleanup is under
/// way; a close that finds no listener then ends the process on the spot,
/// halfway through. While one listens, tokio holds the process for the few
/// seconds Windows allows instead.
struct Termination {
    #[cfg(unix)]
    signals: Option<(tokio::signal::unix::Signal, tokio::signal::unix::Signal)>,
    #[cfg(windows)]
    signals: Option<(
        tokio::signal::windows::CtrlClose,
        tokio::signal::windows::CtrlLogoff,
    )>,
    /// Whether it has been heard.
    fired: bool,
}

impl Termination {
    fn new() -> Termination {
        #[cfg(unix)]
        let signals = {
            use tokio::signal::unix::{SignalKind, signal};
            signal(SignalKind::terminate())
                .ok()
                .zip(signal(SignalKind::hangup()).ok())
        };
        #[cfg(windows)]
        let signals = {
            use tokio::signal::windows::{ctrl_close, ctrl_logoff};
            ctrl_close().ok().zip(ctrl_logoff().ok())
        };
        Termination {
            #[cfg(any(unix, windows))]
            signals,
            fired: false,
        }
    }

    /// Resolve when the process is asked to stop: never, where it cannot be.
    async fn recv(&mut self) {
        #[cfg(any(unix, windows))]
        if let Some((first, second)) = &mut self.signals {
            tokio::select! {
                _ = first.recv() => {}
                _ = second.recv() => {}
            }
            self.fired = true;
            say!("\nkompressor: stopping, restoring what this session changed");
            return;
        }
        std::future::pending().await
    }

    /// Resolve once the process has been asked to stop, whether that was
    /// before this was called or is yet to come.
    async fn hurried(&mut self) {
        if !self.fired {
            self.recv().await;
        }
    }
}

/// The keys that interrupt a session: Ctrl-C, and on Windows Ctrl-Break too.
///
/// Made once, before the session starts anything, and kept to its end: a key
/// pressed while nothing listens takes the default action, which ends this
/// process before it has put anything back.
struct Interrupts {
    #[cfg(unix)]
    keys: Option<tokio::signal::unix::Signal>,
    #[cfg(windows)]
    keys: Option<(
        tokio::signal::windows::CtrlC,
        tokio::signal::windows::CtrlBreak,
    )>,
}

impl Interrupts {
    fn new() -> Interrupts {
        Interrupts {
            #[cfg(unix)]
            keys: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok(),
            #[cfg(windows)]
            keys: tokio::signal::windows::ctrl_c()
                .ok()
                .zip(tokio::signal::windows::ctrl_break().ok()),
        }
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        if let Some(c) = &mut self.keys {
            c.recv().await;
            return;
        }
        #[cfg(windows)]
        if let Some((c, brk)) = &mut self.keys {
            tokio::select! {
                _ = c.recv() => {}
                _ = brk.recv() => {}
            }
            return;
        }
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Point an agent's config file at the proxy, reporting what was done.
///
/// Returns the file edited, if one was, and the flags to start the agent with
/// instead, if that is how this agent is pointed.
fn claim_config(
    kind: agents::ConfigFile,
    port: u16,
) -> Result<(Option<configfile::Target>, Vec<String>), CliError> {
    let target = match kind {
        agents::ConfigFile::Codex => {
            if let Some(flags) = configfile::Target::codex_flags(port) {
                let stale = configfile::Target::codex(port);
                match configfile::clear_stale(&stale) {
                    Ok(settings::Released::Restored) => say!(
                        "kompressor: removed the block a stopped session left in {}",
                        stale.path.display()
                    ),
                    Ok(_) => {}
                    Err(e) => say!(
                        "kompressor: could not check {} for a block left behind: {e}",
                        stale.path.display()
                    ),
                }
                say!(
                    "kompressor: codex (ChatGPT login) points at 127.0.0.1:{port} through \
                     `-c openai_base_url`; its config file is not edited"
                );
                return Ok((None, flags));
            }
            configfile::Target::codex(port)
        }
    };
    match configfile::claim(&target)? {
        configfile::Claimed::Injected => {
            say!(
                "kompressor: {} now points at 127.0.0.1:{port}",
                target.path.display()
            );
        }
        configfile::Claimed::AlreadyCurrent => {
            say!(
                "kompressor: {} already points at 127.0.0.1:{port}",
                target.path.display()
            );
        }
    }
    Ok((Some(target), Vec::new()))
}

/// Put the config file back, saying so only when something changed.
///
/// A failure here is reported and not returned: the agent has already run, and
/// turning a cleanup problem into the session's exit status would report the
/// user's work as failed. The path is named because the repair is manual.
fn release_config(target: &configfile::Target) {
    match configfile::release(target, false) {
        Ok(settings::Released::Restored) => {
            say!("kompressor: restored {}", target.path.display());
        }
        Ok(settings::Released::StillInUse) => {
            say!(
                "kompressor: {} left in place, another session still holds it",
                target.path.display()
            );
        }
        Ok(settings::Released::NotOurs) => {}
        Err(e) => {
            say!(
                "kompressor: could not restore {}: {e}\n           \
                 Run `kompressor unwrap codex` to remove the block by hand.",
                target.path.display()
            );
        }
    }
}

/// Milliseconds since the epoch, for scoping this session's statistics.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Say what the session did, and say loudly when it did nothing at all.
///
/// A wrap that routed no traffic is the failure this whole command is prone to
/// and the one it could never report: the proxy starts, answers `/healthz`,
/// the agent runs normally and talks straight to the provider. Measured on two
/// of the three agents in the catalogue. Every symptom of it looks like
/// success, so the only place to catch it is here, by counting what arrived.
///
/// Silent when statistics are off or unreadable: a missing database is not
/// evidence that nothing was compressed, and a false alarm would train the
/// reader to ignore a true one.
fn report_session(config: &Config, started_ms: i64, manual: bool) {
    if !config.stats.enabled {
        return;
    }
    // A manually wired tool is configured *after* this command prints the URL,
    // and is used for as long as the user leaves it open. A session that ends
    // with no traffic therefore means the user had not pasted the URL yet —
    // not that an agent ignored a variable, which is what the message below
    // says and which would be a lie here: nothing was launched and no variable
    // was set.
    if manual {
        return;
    }
    let Ok(reader) = kompressor_stats::StatsReader::open(&config.stats.path) else {
        return;
    };
    let Ok(outcome) = reader.session_since(started_ms) else {
        return;
    };

    if outcome.proxy_requests == 0 {
        // An unmatched path 404s before the request is ever recorded, so it
        // lands here too. Both causes are named because the fix differs and
        // guessing wrong sends the reader to the wrong file.
        say!(
            "kompressor: no requests reached the proxy — this session compressed \
             nothing.\n           Either the agent used a different base URL than \
             the one set above (most\n           likely: it ignores that variable), \
             or its requests matched no\n           configured upstream and were \
             refused. See docs/CODEX.md for a worked\n           example of the \
             first."
        );
        return;
    }
    if outcome.messages_compressed == 0 {
        say!(
            "kompressor: {} request(s), nothing compressed — every message was \
             withheld.\n           Either there was nothing worth rewriting, or the \
             client pinned its whole\n           prompt with cache_control; see \
             docs/CLAUDE-CODE.md for the second.",
            outcome.proxy_requests
        );
        return;
    }
    let net = outcome.saved_net();
    say!(
        "kompressor: {} request(s), {} message(s) compressed, ~{} tokens saved ({} net \
         of retrieval)",
        outcome.proxy_requests,
        outcome.messages_compressed,
        outcome.saved_gross,
        net
    );
}

/// Launch the agent with the proxy in its environment and wait for it.
async fn launch(
    agent: Option<&'static Agent>,
    extra: &[String],
    flags: &[String],
    env: &[(String, String)],
    offered: Option<&mcp::Registration>,
    interrupts: &mut Interrupts,
) -> CliResult {
    let mut parts = extra.iter();
    let binary = match agent {
        Some(agent) => agent.binary.to_string(),
        None => parts
            .next()
            .ok_or_else(|| {
                CliError::Usage(
                    "no command to run: put it after `--`, e.g. \
                     kompressor wrap --env OPENAI_BASE_URL=v1 -- my-agent --flag"
                        .into(),
                )
            })?
            .clone(),
    };
    let rest: Vec<&String> = parts.collect();

    for (name, value) in env {
        say!("kompressor: {name}={value}");
    }
    // Worth a line of its own: this is the one part of a wrap that adds a tool
    // to the agent's session rather than redirecting its traffic, so a user
    // seeing `mcp__optscale_kompressor__…` in a tool list can tell where it came from
    // and that it goes away when the session does.
    if let Some(offered) = offered {
        say!(
            "kompressor: retrieval tools offered to {binary} as `{}` ({}); \
             this session only, nothing durable is registered",
            mcp::SERVER_NAME,
            offered.path().display()
        );
    }
    // Setting the variable is the whole of the wrap, so when it is known not to
    // be enough the caveat belongs here, next to the claim it qualifies —
    // not in a document the person running this is not reading.
    if let Some(note) = agent.and_then(|agent| agent.note) {
        for line in note.lines() {
            say!("kompressor: note: {line}");
        }
    }

    let mut command = tokio::process::Command::new(&binary);
    // Before the user's own arguments, not after: the last of theirs may be a
    // positional the agent reads as a prompt, and a flag pair appended past it
    // would be swallowed into that string rather than parsed.
    command.args(flags);
    if let Some(offered) = offered {
        command.args(offered.args());
    }
    command.args(&rest);
    // If this wrapper is stopped, the agent goes with it. Leaving it running
    // against a proxy that is about to be torn down, with its config file
    // about to be restored, would be worse than ending it.
    command.kill_on_drop(true);
    for (name, value) in env {
        command.env(name, value);
    }
    let mut child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CliError::Usage(format!("{binary} is not on PATH"))
        } else {
            CliError::runtime(&format!("could not run {binary}"), e)
        }
    })?;

    // Ctrl-C belongs to the agent, not to this wrapper. Installing a handler
    // replaces the default action, so the wrapper stays alive to clean up after
    // the agent has decided what to do about the interrupt.
    let status = loop {
        tokio::select! {
            status = child.wait() => break status,
            _ = interrupts.recv() => continue,
        }
    }
    .map_err(|e| CliError::runtime("could not wait for the agent", e))?;

    match status.code() {
        Some(0) | None => Ok(()),
        // The agent's exit code is the user's answer, not ours. Reporting it as
        // a kompressor failure would make a failing test run look like a
        // compression problem.
        Some(code) => Err(CliError::Runtime(format!(
            "{binary} exited with status {code}"
        ))),
    }
}

/// Configure an agent this command does not launch, and hold the proxy up until
/// the user is done with it.
async fn attach(
    agent: Option<&'static Agent>,
    env: &[(String, String)],
    interrupts: &mut Interrupts,
) -> CliResult {
    let agent = agent.ok_or_else(|| {
        CliError::Usage("--attach needs a named agent whose settings file is known".into())
    })?;
    if agent.name != "claude" {
        return Err(CliError::Usage(format!(
            "--attach is only implemented for `claude` (Claude Code reads \
             .claude/settings.local.json); {} is launched with `kompressor wrap {}`",
            agent.name, agent.name
        )));
    }
    let root = std::env::current_dir()
        .map_err(|e| CliError::runtime("could not read the working directory", e))?;
    let target = settings::Target::claude_code(&root);

    for (name, value) in env {
        settings::claim(&target, name, value)?;
        say!("kompressor: {} {name}={value}", target.settings.display());
    }
    say!("kompressor: attached. Ctrl-C restores the settings and releases the proxy.");

    interrupts.recv().await;
    say!();
    for (name, _) in env {
        match settings::release(&target, name, false)? {
            settings::Released::Restored => {
                say!("kompressor: restored {name}");
            }
            settings::Released::StillInUse => {
                say!("kompressor: {name} left in place, another session still holds it");
            }
            settings::Released::NotOurs => {}
        }
    }
    Ok(())
}

/// Hold the proxy up for a tool this command cannot configure or launch.
///
/// A GUI editor is not a child process and never sees this command's
/// environment, so there is no variable to export and no file of ours to edit.
/// What is left is to print the URL, say where it goes, and keep the proxy
/// alive while the user pastes it — which is a smaller promise than `wrap`
/// makes elsewhere, and is why it is a separate path rather than a quiet
/// special case inside `launch`.
async fn manual(
    agent: Option<&'static Agent>,
    where_: &'static str,
    env: &[(String, String)],
    interrupts: &mut Interrupts,
) -> CliResult {
    let agent = agent.ok_or_else(|| CliError::Usage("manual wiring needs a named agent".into()))?;
    let mut title = agent.name.chars();
    let title: String = match title.next() {
        Some(c) => c.to_uppercase().collect::<String>() + title.as_str(),
        None => agent.name.to_string(),
    };
    say!();
    say!(
        "  {} is configured by hand — there is nothing to launch.",
        agent.about
    );
    // The precondition comes before the URL, not after it. Printed the other
    // way round, a reader who cannot satisfy it pastes the URL first and finds
    // out later that nothing was ever going to arrive — which is what happened
    // the first time this was tested against a real Cursor.
    if let Some(note) = agent.note {
        say!();
        for line in wrap_note(note, 74) {
            say!("  {line}");
        }
    }
    say!();
    say!("  If that holds, in {title}:");
    say!("      {where_}");
    for (name, value) in env {
        say!("      Base URL:  {value}");
        let _ = name;
    }
    say!();
    say!("kompressor: holding the proxy. Ctrl-C releases it.");

    interrupts.recv().await;
    say!();
    say!(
        "kompressor: released. Remember to clear the base URL in {}.",
        agent.name
    );
    Ok(())
}

/// Break a note into lines no wider than `width`, on word boundaries.
fn wrap_note(note: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = String::new();
    for word in note.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

/// Give up this session's claim, and stop the proxy if nothing else needs it.
///
/// `hurry` resolves when the process is asked to stop, which leaves the proxy
/// less time to go before it is killed.
async fn shutdown(
    paths: &Paths,
    started: Option<proxy::Proxy>,
    port: u16,
    hurry: impl std::future::Future<Output = ()>,
) {
    proxy::unregister_client(paths, port);
    let Some(mut started) = started else { return };
    if started.ownership == proxy::Ownership::Adopted {
        return;
    }
    let port = started.port;
    let others = proxy::other_live_clients(paths, port);
    if others > 0 {
        say!("kompressor: leaving the proxy on {port} up for {others} other session(s)");
        return;
    }
    if let Some(child) = started.child.as_mut() {
        proxy::stop(child, port, hurry).await;
    }
}

/// Run `kompressor unwrap`: put a settings file back regardless of who holds it.
pub fn unwrap_agent(args: UnwrapArgs) -> CliResult {
    let agent = Agent::find(&args.agent).ok_or_else(|| {
        CliError::Usage(format!(
            "unknown agent {:?}. Known agents:\n{}",
            args.agent,
            Agent::listing()
        ))
    })?;
    let mut restored = 0;

    // A config-file agent keeps its wiring outside the project, so `unwrap` has
    // to look there whatever directory it is run from. The port is irrelevant
    // to a removal; the block is identified by its markers.
    if let Some(kind) = agent.config {
        let target = match kind {
            agents::ConfigFile::Codex => configfile::Target::codex(0),
        };
        if configfile::release(&target, true)? == settings::Released::Restored {
            println!("restored {}", target.path.display());
            restored += 1;
        } else {
            println!(
                "nothing to undo in {}: no kompressor block is there",
                target.path.display()
            );
        }
    }

    let root = std::env::current_dir()
        .map_err(|e| CliError::runtime("could not read the working directory", e))?;
    let target = settings::Target::claude_code(&root);
    for (name, _) in agent.env {
        if settings::release(&target, name, true)? == settings::Released::Restored {
            println!("restored {name} in {}", target.settings.display());
            restored += 1;
        }
    }
    if restored == 0 && agent.config.is_none() {
        println!(
            "nothing to undo in {}: no kompressor claim was recorded there",
            target.settings.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> WrapArgs {
        WrapArgs {
            agent: None,
            args: Vec::new(),
            port: None,
            config: None,
            mode: None,
            no_proxy: false,
            attach: false,
            print: false,
            env: Vec::new(),
            list: false,
        }
    }

    #[test]
    fn the_port_comes_from_the_flag_then_the_config() {
        let config = Config::default();
        assert_eq!(resolve_port(&args(), &config).unwrap(), 8817);

        let mut explicit = args();
        explicit.port = Some(9000);
        assert_eq!(resolve_port(&explicit, &config).unwrap(), 9000);

        let mut moved = Config::default();
        moved.server.listen = "127.0.0.1:9999".into();
        assert_eq!(
            resolve_port(&args(), &moved).unwrap(),
            9999,
            "a configured listener is where a user expects the proxy"
        );
    }

    #[test]
    fn an_agents_variables_are_resolved_against_the_port() {
        let claude = Agent::find("claude").unwrap();
        let env = resolve_env(Some(claude), &[], 8817, |_| false).unwrap();
        assert_eq!(
            env,
            vec![
                (
                    "ANTHROPIC_BASE_URL".to_string(),
                    "http://127.0.0.1:8817".to_string()
                ),
                // Not a URL, and set for a different reason: see `fixed_env`.
                ("ENABLE_TOOL_SEARCH".to_string(), "true".to_string()),
            ]
        );
    }

    #[test]
    fn an_override_replaces_a_catalogue_entry_rather_than_duplicating_it() {
        let claude = Agent::find("claude").unwrap();
        let env = resolve_env(
            Some(claude),
            &["ANTHROPIC_BASE_URL=v1".to_string()],
            8817,
            |_| false,
        )
        .unwrap();
        let urls: Vec<_> = env
            .iter()
            .filter(|(name, _)| name == "ANTHROPIC_BASE_URL")
            .collect();
        assert_eq!(urls.len(), 1, "one base URL, not two conflicting ones");
        assert_eq!(urls[0].1, "http://127.0.0.1:8817/v1");
    }

    #[test]
    fn a_fixed_variable_is_set_but_never_overrides_the_users_own() {
        let claude = Agent::find("claude").unwrap();

        // Absent: the wrap supplies it, because Claude Code inlines every tool
        // schema without it once the base URL is custom.
        let supplied = fixed_env_to_set(Some(claude), |_| false);
        assert_eq!(
            supplied,
            vec![("ENABLE_TOOL_SEARCH".to_string(), "true".to_string())]
        );

        // Present: the user decided, and the wrap does not argue.
        let deferred = fixed_env_to_set(Some(claude), |_| true);
        assert!(
            deferred.is_empty(),
            "a value the user chose must survive the wrap: {deferred:?}"
        );
    }

    #[test]
    fn wrapping_nothing_at_all_is_refused_with_advice() {
        let error = resolve_env(None, &[], 8817, |_| false).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("--env"),
            "the error has to say how to wrap an unlisted tool: {message}"
        );
    }

    #[test]
    fn an_unlisted_tool_can_still_be_wrapped_by_naming_its_variable() {
        let env = resolve_env(None, &["LLM_BASE_URL=v1".to_string()], 9000, |_| false).unwrap();
        assert_eq!(
            env,
            vec![(
                "LLM_BASE_URL".to_string(),
                "http://127.0.0.1:9000/v1".to_string()
            )]
        );
    }
}
