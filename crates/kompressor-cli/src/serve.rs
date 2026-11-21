//! Running the surfaces.
//!
//! The HTTP API, the MCP tool server and the reverse proxy are three framings of
//! one engine over one set of retained fragments, so by default they share a
//! process and a listener. Splitting them across listeners is a deployment
//! decision — exposing the proxy publicly while keeping the API on a private
//! address — not an architectural one, so it is configuration rather than three
//! binaries.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use kompressor_core::{AppCore, Config, build_ref_store};
use kompressor_server::ServerState;

use crate::error::{CliError, CliResult};

/// Which surfaces to run. Selecting none runs all three, because a bare `serve`
/// should do the obvious thing.
#[derive(Clone, Copy, Debug, Default)]
pub struct Surfaces {
    pub api: bool,
    pub mcp: bool,
    pub proxy: bool,
}

impl Surfaces {
    pub fn resolve(api: bool, mcp: bool, proxy: bool) -> Surfaces {
        if !api && !mcp && !proxy {
            return Surfaces {
                api: true,
                mcp: true,
                proxy: true,
            };
        }
        Surfaces { api, mcp, proxy }
    }
}

/// Everything a running process owns, kept together so shutdown can flush it.
struct Running {
    stats: kompressor_stats::writer::StatsHandle,
}

/// Build the shared core from a configuration.
fn build_core(config: Config) -> Result<(Arc<AppCore>, Running), CliError> {
    let refs = build_ref_store(&config).map_err(|e| CliError::Runtime(e.to_string()))?;

    let mut stats_config = kompressor_stats::config::StatsConfig::at(config.stats.path.clone());
    stats_config.enabled = config.stats.enabled;
    stats_config.channel_capacity = config.stats.queue_size;
    stats_config.flush_interval = config.stats.flush_interval;
    stats_config.retention_days = config.stats.retention_days;
    stats_config.context_retention_days = config.stats.context_retention_days;
    if config.stats.enabled
        && let Some(parent) = config.stats.path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| CliError::Runtime(e.to_string()))?;
    }
    let (sink, handle) = kompressor_stats::writer::spawn(&stats_config)
        .map_err(|e| CliError::Runtime(e.to_string()))?;

    let core = Arc::new(AppCore::new(config, refs, sink));
    Ok((core, Running { stats: handle }))
}

/// Sweep off the request path and stop before the statistics queue is drained.
fn maintenance(
    core: Arc<AppCore>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(core.config().refs.purge_interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                _ = ticks.tick() => {
                    let core = core.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
                        let started = std::time::Instant::now();
                        let purged = core.refs().purge_expired(now.as_secs());
                        if let Err(error) = &purged {
                            tracing::warn!(%error, "fragment retention failed");
                        }
                        // What the sweep freed goes back to the disk a bounded
                        // step at a time, on a store that can do that.
                        let reclaimed = purged.as_ref().ok().map(|_| core.refs().reclaim());
                        if let Some(Err(error)) = &reclaimed {
                            tracing::warn!(%error, "fragment store did not give pages back");
                        }
                        core.note_refs_sweep(kompressor_core::RefsSweep {
                            at_ms: now.as_millis() as i64,
                            took_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                            removed: purged.as_ref().ok().copied(),
                            reclaimed_bytes: reclaimed.and_then(Result::ok),
                            error: purged.err().map(|error| error.to_string()),
                        });
                        match core.refs().stats() {
                            Ok(stats) => core.stats().send(kompressor_stats::events::Event::StoreSample {
                                time_ms: now.as_millis() as i64,
                                live_refs: stats.live_refs.min(i64::MAX as u64) as i64,
                                live_bytes: stats.live_bytes.min(i64::MAX as u64) as i64,
                            }),
                            Err(error) => tracing::warn!(%error, "could not sample fragment store"),
                        }
                    }).await;
                    if let Err(error) = result { tracing::warn!(%error, "fragment maintenance stopped"); }
                }
            }
        }
    })
}

/// How long after a scheduled backup fails before it is tried again. A full
/// disk does not clear in a minute, and a warning a minute buries the log.
const BACKUP_RETRY: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Take the scheduled backups (`maintenance.backup_dir`, `backup_every`).
///
/// `None` when there is no schedule. Only `serve` runs this: an `mcp serve`
/// started per agent session opens the same databases, and a dozen of them
/// would each take the same backup.
///
/// Whether one is due is read from the directory each time — the newest
/// archive's age — so a restart neither skips a backup nor takes an extra
/// one, and a service that was down over the hour takes it on coming back.
fn scheduled_backups(
    core: Arc<AppCore>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Option<tokio::task::JoinHandle<()>> {
    let (dir, every) = core.config().maintenance.schedule()?;
    tracing::info!(
        dir = %dir.display(),
        every = ?every,
        keep = core.config().maintenance.backup_keep,
        "backups are scheduled"
    );
    let tick = (every / 4).clamp(
        std::time::Duration::from_millis(50),
        std::time::Duration::from_secs(60),
    );
    let retry = BACKUP_RETRY.min(every);
    Some(tokio::spawn(async move {
        let mut ticks = tokio::time::interval(tick);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut failed_at: Option<std::time::Instant> = None;
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                _ = ticks.tick() => {
                    if failed_at.is_some_and(|at| at.elapsed() < retry) {
                        continue;
                    }
                    let for_task = core.clone();
                    let taken = tokio::task::spawn_blocking(move || {
                        let core = for_task;
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|since| since.as_millis() as i64)
                            .unwrap_or(0);
                        let config = core.config();
                        let (dir, every) = config.maintenance.schedule()?;
                        if kompressor_core::backup::next_due_ms(dir, every, now_ms) > now_ms {
                            return None;
                        }
                        // What is still queued belongs in the snapshot.
                        let _ = core.stats().flush();
                        Some(kompressor_core::backup::run_scheduled(config, now_ms))
                    })
                    .await;
                    // On record either way, for "when was the last backup".
                    let record = |destination: String, bytes: u64, databases: usize, error: Option<String>| {
                        core.stats().send(kompressor_stats::Event::Backup(
                            kompressor_stats::events::BackupEvent {
                                time_ms: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|since| since.as_millis() as i64)
                                    .unwrap_or(0),
                                origin: "schedule".to_string(),
                                destination,
                                bytes: i64::try_from(bytes).unwrap_or(i64::MAX),
                                databases: databases as i64,
                                ok: error.is_none(),
                                error,
                            },
                        ));
                    };
                    let directory = || {
                        core.config().maintenance.backup_dir.display().to_string()
                    };
                    match taken {
                        Ok(None) | Ok(Some(Ok(None))) => {}
                        Ok(Some(Ok(Some(done)))) => {
                            failed_at = None;
                            record(
                                done.archive.display().to_string(),
                                done.bytes,
                                done.manifest.databases.len(),
                                None,
                            );
                            tracing::info!(
                                archive = %done.archive.display(),
                                bytes = done.bytes,
                                databases = done.manifest.databases.len(),
                                removed = done.removed.len(),
                                "scheduled backup written"
                            );
                        }
                        Ok(Some(Err(error))) => {
                            failed_at = Some(std::time::Instant::now());
                            record(directory(), 0, 0, Some(error.to_string()));
                            tracing::warn!(%error, retry_in = ?retry, "scheduled backup failed");
                        }
                        Err(error) => {
                            failed_at = Some(std::time::Instant::now());
                            tracing::warn!(%error, "scheduled backup task stopped");
                        }
                    }
                }
            }
        }
    }))
}

/// Whether an optimize last done at `last_ms` (none on record: never) is due
/// again at `now_ms`.
fn optimize_due(last_ms: Option<i64>, every: std::time::Duration, now_ms: i64) -> bool {
    let every = i64::try_from(every.as_millis()).unwrap_or(i64::MAX);
    // A record dated in the future — a clock that was wrong — must not put
    // the next one off for ever.
    last_ms.is_none_or(|last| last > now_ms || now_ms - last >= every)
}

/// Refresh the query planner's statistics on each database every
/// `maintenance.optimize_every`, as the page's Optimize does by hand.
///
/// `None` when that is zero. Only `serve` runs this, as it runs the backups.
/// When each was last done is read from the record of maintenance work, so a
/// restart neither skips one nor repeats it; with statistics off it is this
/// process's own memory.
fn scheduled_optimize(
    core: Arc<AppCore>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Option<tokio::task::JoinHandle<()>> {
    let every = core.config().maintenance.optimize_every;
    if every.is_zero() {
        return None;
    }
    let tick = (every / 4).clamp(
        std::time::Duration::from_millis(50),
        std::time::Duration::from_secs(600),
    );
    Some(tokio::spawn(async move {
        let mut ticks = tokio::time::interval(tick);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let done_here: Arc<std::sync::Mutex<BTreeMap<&'static str, i64>>> = Arc::default();
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                _ = ticks.tick() => {
                    let core = core.clone();
                    let done_here = done_here.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        optimize_what_is_due(&core, every, &done_here);
                    })
                    .await;
                    if let Err(error) = result {
                        tracing::warn!(%error, "scheduled optimize stopped");
                    }
                }
            }
        }
    }))
}

fn optimize_what_is_due(
    core: &AppCore,
    every: std::time::Duration,
    done_here: &std::sync::Mutex<BTreeMap<&'static str, i64>>,
) {
    let config = core.config();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as i64);
    let recorded: Vec<kompressor_stats::MaintenanceEvent> =
        if config.stats.enabled && config.stats.path.is_file() {
            kompressor_stats::StatsReader::open(&config.stats.path)
                .and_then(|reader| reader.latest_maintenance())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
    let mut databases: Vec<(&'static str, PathBuf)> = Vec::new();
    if config.stats.enabled && config.stats.path.is_file() {
        databases.push(("stats.db", config.stats.path.clone()));
    }
    if config.refs.backend == kompressor_core::config::RefsBackend::Sqlite
        && config.refs.path.is_file()
    {
        databases.push(("refs.db", config.refs.path.clone()));
    }
    for (name, path) in databases {
        let on_record = recorded
            .iter()
            .filter(|work| work.target == name && work.action == "optimize" && work.ok)
            .map(|work| work.time_ms)
            .max();
        let here = done_here
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(name)
            .copied();
        if !optimize_due(on_record.max(here), every, now_ms) {
            continue;
        }
        let started = std::time::Instant::now();
        let result = if name == "stats.db" {
            core.stats()
                .exclusive(move || kompressor_store::sqlite::optimize(&path))
                .map_err(|error| error.to_string())
                .and_then(|done| done.map_err(|error| error.to_string()))
        } else {
            kompressor_store::sqlite::optimize(&path).map_err(|error| error.to_string())
        };
        let finished_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(now_ms, |since| since.as_millis() as i64);
        // Not tried again before the interval, failed or not: a failure is
        // on record and in the log, and one an interval is enough.
        done_here
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(name, finished_ms);
        match &result {
            Ok(described) => tracing::info!(database = name, described, "scheduled optimize"),
            Err(error) => tracing::warn!(database = name, %error, "scheduled optimize failed"),
        }
        core.stats().send(kompressor_stats::Event::Maintenance(
            kompressor_stats::MaintenanceEvent {
                time_ms: finished_ms,
                action: "optimize".to_string(),
                target: name.to_string(),
                origin: "schedule".to_string(),
                actor: None,
                took_ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
                ok: result.is_ok(),
                bytes_before: None,
                bytes_after: None,
                detail: result
                    .as_ref()
                    .ok()
                    .map(|described| serde_json::json!({"described": described}).to_string()),
                error: result.err(),
            },
        ));
    }
}

/// Compose the selected surfaces into one router.
///
/// Order matters, and only in one place: the proxy claims everything no other
/// surface named, because it has to forward paths this binary has never heard of
/// to an upstream that has. Merging it first would let it swallow `/v1/compress`.
/// What the intercept listener serves.
///
/// A connection arriving through CONNECT is a proxied request and nothing else:
/// the client believes it is talking to the provider, so the API and MCP
/// surfaces have no meaning on it and mounting them would answer paths the
/// provider also owns.
fn surfaces_for_intercept(core: &Arc<AppCore>) -> Surfaces {
    let _ = core;
    Surfaces {
        api: false,
        mcp: false,
        proxy: true,
    }
}

fn compose(core: &Arc<AppCore>, surfaces: Surfaces, client: reqwest::Client) -> Router {
    // The probes go on every listener, whatever it serves. With per-surface
    // addresses a proxy-only listener would otherwise have no `/healthz` at
    // all — and because the proxy is a catch-all, the probe would not 404, it
    // would be *forwarded to the provider*, so an orchestrator reads the
    // provider's 404 as a dead process while the proxy is serving normally.
    let mut app = Router::new().merge(kompressor_server::ops_router(ServerState::new(
        core.clone(),
    )));

    if surfaces.api {
        let state = ServerState::new(core.clone());
        app = app.merge(kompressor_server::api_router(state));
    }
    if surfaces.mcp {
        app = app.merge(kompressor_mcp::mcp_router(core.clone()));
    }
    if surfaces.proxy && core.config().proxy.enabled {
        app = app.merge(kompressor_proxy::proxy_router(core.clone(), client));
    }
    app
}

fn upstream_client(config: &Config) -> Result<reqwest::Client, CliError> {
    reqwest::Client::builder()
        .connect_timeout(config.proxy.upstream_connect_timeout)
        .read_timeout(config.proxy.upstream_read_timeout)
        // The proxy re-reads bodies it may need to resend for a retrieval round,
        // so redirects are followed by us rather than silently by the client.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| CliError::Runtime(format!("could not build the upstream client: {e}")))
}

/// Install the log subscriber the `[log]` section describes.
///
/// Without this the whole binary is silent. `tracing` calls compile to nothing
/// at runtime unless a subscriber consumes them, so every `info!`, `warn!` and
/// `error!` in every crate — the upstream stream ending early, a compressor
/// panicking, a client hanging up — went nowhere, and `log.level` and
/// `log.format` configured a subscriber that was never built.
///
/// That is only visible when something goes wrong: a smoke run produced a
/// request that was compressed, forwarded, and abandoned mid-stream, and the
/// proxy's log file for the whole run was zero bytes.
///
/// `RUST_LOG` wins over the configured level, as the configuration says it does.
fn init_logging(log: &kompressor_core::config::LogConfig) {
    init_logging_to(log, std::io::stdout, None);
}

/// [`init_logging`], writing somewhere other than stdout.
///
/// `ansi` is `Some(false)` for a destination that ends up in a file rather
/// than a terminal, where colour codes are noise in every line, and `None` to
/// leave the subscriber's own default alone. That default honours `NO_COLOR`;
/// `with_ansi(true)` overrides it, so stdout logging must not pass a value —
/// it did for one merge, and an operator's `NO_COLOR=1` stopped working.
fn init_logging_to<W>(log: &kompressor_core::config::LogConfig, writer: W, ansi: Option<bool>)
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    use tracing_subscriber::filter::EnvFilter;
    use tracing_subscriber::fmt;

    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&log.level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    // `try_init` rather than `init`: a second call must not take the process
    // down, and a caller that already installed one keeps it.
    let _ = match (log.format.as_str(), ansi) {
        ("json", Some(ansi)) => fmt::Subscriber::builder()
            .json()
            .with_env_filter(filter)
            .with_writer(writer)
            .with_ansi(ansi)
            .try_init(),
        ("json", None) => fmt::Subscriber::builder()
            .json()
            .with_env_filter(filter)
            .with_writer(writer)
            .try_init(),
        (_, Some(ansi)) => fmt::Subscriber::builder()
            .with_env_filter(filter)
            .with_writer(writer)
            .with_ansi(ansi)
            .try_init(),
        (_, None) => fmt::Subscriber::builder()
            .with_env_filter(filter)
            .with_writer(writer)
            .try_init(),
    };
}

/// Serve the MCP tools over stdin and stdout until the launching runtime hangs
/// up.
///
/// The other surfaces are reached over a socket, so a client finds them by URL.
/// An agent runtime instead *spawns* its tool servers and talks to them down a
/// pipe, which is why this is a separate entry point rather than a flag on
/// [`run`]: there is no address to bind and no listener to compose, and the
/// process must keep stdout clear for the protocol.
///
/// That last point is why logging goes to **stderr** here. Every other command
/// logs to stdout; here a single stray line on stdout is a protocol violation
/// that the runtime reports as a broken server. Stderr is the channel MCP
/// leaves a stdio server for exactly this, and runtimes keep it — Claude
/// Desktop in its per-server log file.
///
/// It used to be off entirely, which was safe and left proxy mode mute: a
/// fronted server that did not start, did not answer `initialize`, or was
/// skipped for its configuration vanished from the tool list with nothing
/// anywhere to say why.
///
/// The store this opens is the one named by `config`, which is how a fragment
/// the proxy elided becomes one this server can hand back: the wrap points both
/// at the same configuration, so a handle minted on the proxy resolves here.
pub async fn run_stdio(config: Config) -> CliResult {
    // rmcp logs every handshake and notification at `info`, peer info and all,
    // which in a per-server log buries the lines that say a fronted server was
    // skipped. Held to `warn` here; `RUST_LOG` still overrides the whole filter.
    let log = kompressor_core::config::LogConfig {
        level: format!("{},rmcp=warn", config.log.level),
        ..config.log.clone()
    };
    init_logging_to(&log, std::io::stderr, Some(false));
    let (core, running) = build_core(config)?;
    let (stop, rx) = tokio::sync::watch::channel(false);
    let upkeep = maintenance(core.clone(), rx);
    let result = kompressor_mcp::serve_stdio(core)
        .await
        .map_err(|e| CliError::runtime("the stdio tool server stopped", e));

    let _ = stop.send(true);
    let _ = upkeep.await;

    // Retrievals are recorded like any other work, and this process owns the
    // queue they are written from. Exiting the moment the pipe closes would
    // drop whatever the last call queued, so the same drain `run` performs
    // happens here.
    if let Err(error) = running.stats.shutdown() {
        tracing::warn!(%error, "statistics did not flush cleanly on shutdown");
    }
    result
}

/// How a supervising `kompressor wrap` may stop this process, beyond signals.
#[derive(Default)]
pub struct Supervision {
    /// Stop when [`STOP_LINE`] arrives on standard input.
    pub stop_on_stdin: bool,
    /// After standard input closes, stop once no live client marker is left in
    /// this directory.
    pub stop_when_unused: Option<PathBuf>,
}

/// Bind and serve until interrupted, or until the supervisor asks it to stop
/// (see [`Supervision`]).
pub async fn run(
    config: Config,
    surfaces: Surfaces,
    listen: Option<String>,
    supervision: Supervision,
) -> CliResult {
    init_logging(&config.log);
    let listen = listen.unwrap_or_else(|| config.server.listen.clone());
    let grace = config.server.shutdown_grace;
    let client = upstream_client(&config)?;
    let (core, running) = build_core(config)?;
    let groups = listener_groups(core.config(), surfaces, &listen);
    if groups.is_empty() {
        return Err(CliError::Usage("no enabled surface to serve".into()));
    }
    let mut servers = tokio::task::JoinSet::new();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    // Bind every address before serving any of them, so a typo in one surface
    // does not leave a process that appears healthy on the others.
    let mut bound = Vec::new();
    for (address, selected) in groups {
        let addr: SocketAddr = address
            .parse()
            .map_err(|e| CliError::Usage(format!("invalid listen address {address:?}: {e}")))?;
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| CliError::Runtime(format!("could not bind {addr}: {e}")))?;
        bound.push((addr, listener, selected));
    }
    let stats_listener = bind_stats_listener(core.config(), &bound).await?;
    for (addr, listener, selected) in bound {
        let mut enabled = Vec::new();
        if selected.api {
            enabled.push("api");
        }
        if selected.mcp {
            enabled.push("mcp");
        }
        if selected.proxy {
            enabled.push("proxy");
        }
        tracing::info!(address = %addr, surfaces = %enabled.join(","), mode = %core.config().engine.mode, "listening");
        core.runtime().listening(addr.to_string(), &enabled);
        let app = compose(&core, selected, client.clone());
        let mut rx = shutdown_rx.clone();
        servers.spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    while !*rx.borrow() {
                        if rx.changed().await.is_err() {
                            break;
                        }
                    }
                })
                .await
        });
    }
    if let Some((addr, listener)) = stats_listener {
        tracing::info!(address = %addr, "live statistics at http://{addr}/, cache analysis at http://{addr}/cache");
        core.runtime().listening(addr.to_string(), &["statistics"]);
        let app = kompressor_server::stats_listener_router(ServerState::new(core.clone()));
        let mut rx = shutdown_rx.clone();
        servers.spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    while !*rx.borrow() {
                        if rx.changed().await.is_err() {
                            break;
                        }
                    }
                })
                .await
        });
    }
    // The intercept listener, for clients that pin their own base URL and so
    // can never be pointed at the proxy. It serves the same router the ordinary
    // listeners do -- it only differs in how a connection reaches it -- and it
    // joins the same JoinSet, so `shutdown_grace` already covers it.
    let intercept = &core.config().proxy.intercept;
    if let Some(address) = intercept.listen.clone() {
        let bound = kompressor_proxy::intercept::Intercept::bind(
            &address,
            &intercept.intercept_hosts,
            &intercept.ca_path,
            &intercept.alpn,
            intercept.leaf_cache_entries,
        )
        .await
        .map_err(|e| CliError::Runtime(e.to_string()))?;
        tracing::info!(
            address = %bound.address(),
            hosts = %intercept.intercept_hosts.join(","),
            ca = %bound.certificate_path().display(),
            fingerprint = %bound.fingerprint(),
            "intercepting"
        );
        core.runtime()
            .listening(bound.address().to_string(), &["interception"]);
        let app = compose(&core, surfaces_for_intercept(&core), client.clone());
        let mut rx = shutdown_rx.clone();
        servers.spawn(async move {
            bound
                .serve(app, async move {
                    while !*rx.borrow() {
                        if rx.changed().await.is_err() {
                            break;
                        }
                    }
                })
                .await;
            Ok(())
        });
    }

    let backups = scheduled_backups(core.clone(), shutdown_rx.clone());
    let optimizes = scheduled_optimize(core.clone(), shutdown_rx.clone());
    let upkeep = maintenance(core.clone(), shutdown_rx);
    let mut serve_error = None;
    tokio::select! {
        _ = shutdown_signal(supervision) => {},
        result = servers.join_next() => {
            serve_error = Some(format!("listener stopped unexpectedly: {result:?}"));
        }
    }
    let _ = shutdown_tx.send(true);
    if tokio::time::timeout(grace, async {
        while servers.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        servers.abort_all();
        while servers.join_next().await.is_some() {}
    }

    let _ = upkeep.await;
    // A backup under way when the stop came is finished, not cut short: the
    // task checks for shutdown between backups, never inside one.
    if let Some(backups) = backups {
        let _ = backups.await;
    }
    if let Some(optimizes) = optimizes {
        let _ = optimizes.await;
    }

    // Statistics are written off the request path, so a process that exits the
    // moment the listener closes loses whatever was still queued. The shutdown
    // grace is what that queue is drained within.
    tracing::info!(?grace, "draining statistics");
    if let Err(error) = running.stats.shutdown() {
        // Losing the last few statistics rows is not worth a non-zero exit: the
        // work they describe already happened and already reached the caller.
        tracing::warn!(%error, "statistics did not flush cleanly on shutdown");
    }
    if let Some(error) = serve_error {
        return Err(CliError::Runtime(error));
    }
    Ok(())
}

/// Bind `server.stats_listen`, when it names an address.
///
/// Separate from [`listener_groups`] because it is not a surface: it serves
/// the live statistics whatever surfaces were selected, so `serve --proxy`
/// still has its dashboard. Sharing an address with a surface listener is
/// refused rather than merged — the API surface already serves
/// `/v1/stats/live` on its own listener, and a second bind would fail with a
/// less useful message.
async fn bind_stats_listener(
    config: &Config,
    surfaces: &[(SocketAddr, tokio::net::TcpListener, Surfaces)],
) -> Result<Option<(SocketAddr, tokio::net::TcpListener)>, CliError> {
    let address = config.server.stats_listen.trim();
    if address.is_empty() {
        return Ok(None);
    }
    let addr: SocketAddr = address
        .parse()
        .map_err(|e| CliError::Usage(format!("invalid server.stats_listen {address:?}: {e}")))?;
    if surfaces.iter().any(|(bound, _, _)| *bound == addr) {
        return Err(CliError::Usage(format!(
            "server.stats_listen {addr} is already a surface listener; give it a port of its \
             own, or leave it empty and use /v1/stats/live on the API listener"
        )));
    }
    if !config.stats.enabled {
        tracing::warn!(
            address = %addr,
            "server.stats_listen is set but statistics are disabled; not listening"
        );
        return Ok(None);
    }
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        CliError::Runtime(format!("could not bind server.stats_listen {addr}: {e}"))
    })?;
    Ok(Some((addr, listener)))
}

fn listener_groups(
    config: &Config,
    surfaces: Surfaces,
    shared: &str,
) -> BTreeMap<String, Surfaces> {
    let mut groups = BTreeMap::<String, Surfaces>::new();
    for (enabled, address, select) in [
        (surfaces.api, &config.server.api_listen, 0),
        (surfaces.mcp, &config.server.mcp_listen, 1),
        (
            surfaces.proxy && config.proxy.enabled,
            &config.server.proxy_listen,
            2,
        ),
    ] {
        if !enabled {
            continue;
        }
        let entry = groups
            .entry(if address.is_empty() {
                shared.to_string()
            } else {
                address.clone()
            })
            .or_default();
        match select {
            0 => entry.api = true,
            1 => entry.mcp = true,
            _ => entry.proxy = true,
        }
    }
    groups
}

/// The line `kompressor wrap` writes to a supervised proxy's standard input to
/// stop it. See [`stdin_stop`].
pub const STOP_LINE: &str = "stop";

async fn shutdown_signal(supervision: Supervision) {
    let interrupt = async {
        tokio::signal::ctrl_c()
            .await
            .expect("ctrl-c handler installs");
    };
    let requested = async {
        if supervision.stop_on_stdin {
            stdin_stop(supervision.stop_when_unused).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        let Ok(mut term) = signal(SignalKind::terminate()) else {
            return std::future::pending::<()>().await;
        };
        // The terminal closing. Not under `nohup`, which ignores it so the
        // proxy outlives the terminal: a handler of ours would undo that.
        let mut hup = if hangup_ignored() {
            None
        } else {
            signal(SignalKind::hangup()).ok()
        };
        let hangup = async {
            match &mut hup {
                Some(hup) => {
                    hup.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = hangup => {}
        }
    };
    // Ctrl-Break, the console window closing, and the system shutting down —
    // which a service hears too. Not logoff: a service hears that whenever
    // anyone logs off. While these listen, tokio holds the process for the few
    // seconds Windows allows, which the statistics drain runs in.
    #[cfg(windows)]
    let terminate = async {
        use tokio::signal::windows::{ctrl_break, ctrl_close, ctrl_shutdown};
        let (Ok(mut brk), Ok(mut close), Ok(mut shutdown)) =
            (ctrl_break(), ctrl_close(), ctrl_shutdown())
        else {
            return std::future::pending::<()>().await;
        };
        tokio::select! {
            _ = brk.recv() => {}
            _ = close.recv() => {}
            _ = shutdown.recv() => {}
        }
    };
    #[cfg(not(any(unix, windows)))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = interrupt => {}
        _ = terminate => {}
        _ = requested => {}
    }
}

/// Whether SIGHUP was ignored when this process started, as `nohup` leaves it.
///
/// Read from `/proc`, so Linux only. Elsewhere it counts as ignored, which
/// keeps the platform's default rather than guessing.
#[cfg(unix)]
fn hangup_ignored() -> bool {
    std::fs::read_to_string("/proc/self/status").map_or(true, |status| hangup_ignored_in(&status))
}

/// [`hangup_ignored`] for a given `/proc/<pid>/status`: bit 0 of `SigIgn` is
/// SIGHUP, signal 1.
#[cfg(unix)]
fn hangup_ignored_in(status: &str) -> bool {
    status
        .lines()
        .find_map(|line| line.strip_prefix("SigIgn:"))
        .and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok())
        .is_none_or(|mask| mask & 1 != 0)
}

/// Is this line from the supervisor a request to stop?
///
/// Read as bytes, not as `str` lines: a line that is not UTF-8 is not a stop,
/// but it must not end the listener either, or every later `stop` is lost.
/// A leading byte-order mark is ignored. A .NET supervisor's
/// `Process.StandardInput` writes one when the console encoding is UTF-8, and
/// `str::trim` does not remove U+FEFF, so `\u{feff}stop` used to leave the
/// proxy running for good.
fn is_stop_line(line: &[u8]) -> bool {
    let line = line.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(line);
    std::str::from_utf8(line).is_ok_and(|text| text.trim() == STOP_LINE)
}

/// Resolve when the supervising `kompressor wrap` writes [`STOP_LINE`].
///
/// This is how a wrap session stops the proxy it started, on every platform:
/// Windows has no SIGTERM, there is no admin endpoint, and a pipe only the
/// parent holds is a stop button nothing else can press.
///
/// End of input is *not* a request to stop. It is the supervisor exiting while
/// other wrap sessions still use this proxy, or being killed, and taking the
/// proxy down then would cut the others off mid-turn. Without `unused` it
/// leaves this pending for ever. With it, the proxy has no owner left, so it
/// watches the client markers in that directory and stops once no live session
/// remains: otherwise nothing would ever stop it, and on Windows it runs with
/// no window until logoff.
///
/// Read on a plain thread rather than through `tokio::io::stdin`, which reads
/// on the blocking pool: a read still pending there holds up the runtime's
/// shutdown after a Ctrl-C for as long as the supervisor keeps the pipe open.
async fn stdin_stop(unused: Option<PathBuf>) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        let mut stdin = std::io::stdin().lock();
        let mut line = Vec::new();
        loop {
            line.clear();
            match stdin.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) if is_stop_line(&line) => {
                    let _ = tx.send(());
                    return;
                }
                Ok(_) => continue,
            }
        }
    });
    // The sender is dropped on end of input, which resolves this with an
    // error. That is the case above, not a stop.
    if rx.await.is_ok() {
        return;
    }
    match unused {
        Some(dir) => stop_when_unused(&dir).await,
        None => std::future::pending::<()>().await,
    }
}

/// How often an orphaned proxy looks for live client markers.
const UNUSED_POLL: std::time::Duration = std::time::Duration::from_secs(2);
/// How many polls in a row must find none before it stops. More than one,
/// because a session that is starting registers its marker before it checks
/// for a running proxy, and one empty look can fall between two sessions.
const UNUSED_POLLS: u32 = 2;

/// Resolve once no live session holds a marker in `dir`, for
/// [`UNUSED_POLLS`] consecutive looks.
async fn stop_when_unused(dir: &std::path::Path) {
    let me = std::process::id();
    tracing::info!(
        clients = %dir.display(),
        "the supervising wrap has gone; stopping once no session uses this proxy"
    );
    let mut empty = 0;
    loop {
        tokio::time::sleep(UNUSED_POLL).await;
        let dir = dir.to_path_buf();
        let live =
            tokio::task::spawn_blocking(move || crate::wrap::proxy::live_clients_in(&dir, me))
                .await
                .unwrap_or(1);
        if live > 0 {
            empty = 0;
            continue;
        }
        empty += 1;
        if empty >= UNUSED_POLLS {
            tracing::info!("no session uses this proxy any more; stopping");
            return;
        }
    }
}

#[cfg(test)]
mod logging_tests {
    use super::*;
    use kompressor_core::config::LogConfig;

    fn log(level: &str, format: &str) -> LogConfig {
        LogConfig {
            level: level.into(),
            format: format.into(),
            payloads: false,
        }
    }

    /// Installing a global subscriber twice is an error, and an error here would
    /// take the process down at startup — the one moment nothing is logged yet
    /// to say why.
    #[test]
    fn installing_the_subscriber_twice_is_survivable() {
        init_logging(&log("info", "json"));
        init_logging(&log("debug", "text"));
        init_logging(&log("not a level", "not a format"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use kompressor_core::config::RefsBackend;
    use tower::ServiceExt;

    fn test_config() -> Config {
        let mut config = Config::default();
        config.refs.backend = RefsBackend::Memory;
        config.stats.enabled = false;
        config
    }

    /// With a schedule, `serve` takes a backup without being asked — at once
    /// when the directory holds none — keeps taking them, prunes to
    /// `backup_keep`, and stops with the service. With none, there is no task.
    #[tokio::test]
    async fn scheduled_backups_are_taken_pruned_and_stop_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config();
        let (_, idle) = tokio::sync::watch::channel(false);
        let unscheduled = Arc::new(AppCore::new(
            config.clone(),
            kompressor_core::build_ref_store(&config).unwrap(),
            kompressor_stats::null_sink(),
        ));
        assert!(scheduled_backups(unscheduled, idle).is_none());

        config.refs.backend = RefsBackend::Sqlite;
        config.refs.path = dir.path().join("refs.db");
        let backups = dir.path().join("backups");
        config.maintenance.backup_dir = backups.clone();
        // One a second: archives are named to the second.
        config.maintenance.backup_every = std::time::Duration::from_secs(1);
        config.maintenance.backup_keep = 2;
        let core = Arc::new(AppCore::new(
            config.clone(),
            kompressor_core::build_ref_store(&config).unwrap(),
            kompressor_stats::null_sink(),
        ));
        let (stop, rx) = tokio::sync::watch::channel(false);
        let task = scheduled_backups(core, rx).expect("a schedule");

        let listed = || kompressor_core::backup::list_archives(&backups);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut seen = std::collections::BTreeSet::new();
            while seen.len() < 3 {
                seen.extend(listed().into_iter().map(|archive| archive.name));
                // One over while the new archive is written: the old one
                // goes only once its replacement is there.
                assert!(listed().len() <= 3, "backup_keep, and the one in hand");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("three backups, the first of them at once");
        assert!(listed().iter().all(|archive| archive.bytes > 0));

        stop.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("stops with the service")
            .unwrap();
        let after = listed().len();
        assert_eq!(after, 2, "settled at backup_keep");
        tokio::time::sleep(std::time::Duration::from_millis(1_300)).await;
        assert_eq!(listed().len(), after, "and takes no more");
    }

    #[test]
    fn an_optimize_is_due_an_interval_after_the_last() {
        let day = std::time::Duration::from_secs(86_400);
        let now = 10 * 86_400_000;
        assert!(optimize_due(None, day, now), "never done: due now");
        assert!(!optimize_due(Some(now - 3_600_000), day, now));
        assert!(optimize_due(Some(now - 86_400_000), day, now));
        assert!(
            optimize_due(Some(now + 3_600_000), day, now),
            "a record from the future"
        );
    }

    /// Each database on disk is optimized when due, put on record as the
    /// schedule's work, and not again within the interval.
    #[tokio::test]
    async fn scheduled_optimizes_are_recorded_and_not_repeated() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config();
        config.refs.backend = RefsBackend::Sqlite;
        config.refs.path = dir.path().join("refs.db");
        let (sink, events) = kompressor_stats::writer::capturing_sink(32);
        let core = Arc::new(AppCore::new(
            config.clone(),
            kompressor_core::build_ref_store(&config).unwrap(),
            sink,
        ));
        let every = std::time::Duration::from_secs(3_600);
        let done_here = std::sync::Mutex::new(BTreeMap::new());
        optimize_what_is_due(&core, every, &done_here);
        let work: Vec<_> = events
            .drain()
            .into_iter()
            .filter_map(|event| match event {
                kompressor_stats::Event::Maintenance(work) => Some(work),
                _ => None,
            })
            .collect();
        assert_eq!(work.len(), 1, "the ref store; statistics are off: {work:?}");
        assert_eq!(work[0].target, "refs.db");
        assert_eq!(work[0].action, "optimize");
        assert_eq!(work[0].origin, "schedule");
        assert!(work[0].ok, "{work:?}");
        assert!(
            kompressor_store::sqlite::facts(&config.refs.path)
                .unwrap()
                .analyzed
        );
        optimize_what_is_due(&core, every, &done_here);
        assert!(events.drain().is_empty(), "not again within the interval");

        let mut off = config.clone();
        off.maintenance.optimize_every = std::time::Duration::ZERO;
        let (_, idle) = tokio::sync::watch::channel(false);
        let unscheduled = Arc::new(AppCore::new(
            off.clone(),
            kompressor_core::build_ref_store(&off).unwrap(),
            kompressor_stats::null_sink(),
        ));
        assert!(scheduled_optimize(unscheduled, idle).is_none());
    }

    #[test]
    fn a_stop_line_is_recognised_with_any_line_ending_and_a_bom() {
        for line in [
            &b"stop"[..],
            b"stop\n",
            b"stop\r\n",
            b"  stop  \r\n",
            b"\xEF\xBB\xBFstop\r\n",
        ] {
            assert!(is_stop_line(line), "{line:?} is a stop");
        }
        for line in [
            &b""[..],
            b"\n",
            b"stopp\n",
            b"please stop\n",
            b"\xFF\xFEs\x00t\x00o\x00p\x00\n",
            b"\xEF\xBB\xBF\xEF\xBB\xBFstop\n",
        ] {
            assert!(!is_stop_line(line), "{line:?} is not a stop");
        }
    }

    #[tokio::test]
    async fn maintenance_purges_samples_and_stops_on_shutdown() {
        use kompressor_store::types::{PutMeta, RefStore, Tenant};
        let clock = kompressor_store::clock::ManualClock::at(1);
        let store =
            Arc::new(kompressor_store::memory::MemoryRefStore::new().with_clock(clock.clock()));
        store
            .put_many(
                &Tenant::anonymous(),
                &[(kompressor_engine::refs::ref_for("old"), "old".into())],
                &PutMeta::new(kompressor_engine::types::Kind::Text).with_ttl(1),
            )
            .unwrap();
        let mut config = test_config();
        config.refs.purge_interval = std::time::Duration::from_millis(10);
        let (sink, events) = kompressor_stats::writer::capturing_sink(32);
        let core = Arc::new(AppCore::new(config, store.clone(), sink));
        let (stop, rx) = tokio::sync::watch::channel(false);
        let task = maintenance(core, rx);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if events.drain().iter().any(|event| {
                    matches!(
                        event,
                        kompressor_stats::Event::StoreSample {
                            live_refs: 0,
                            live_bytes: 0,
                            ..
                        }
                    )
                }) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            store.purge_expired(u64::MAX).unwrap(),
            0,
            "the timer already removed the expired row"
        );
        stop.send(true).unwrap();
        task.await.unwrap();
        events.drain();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(events.drain().is_empty());
    }

    #[tokio::test]
    async fn every_listener_answers_the_probes_whatever_it_serves() {
        // A proxy-only listener is the case that matters: its router is a
        // catch-all, so a missing probe route is not a 404 but a request
        // relayed to the provider.
        for surfaces in [
            Surfaces {
                api: false,
                mcp: false,
                proxy: true,
            },
            Surfaces {
                api: false,
                mcp: true,
                proxy: false,
            },
            Surfaces {
                api: true,
                mcp: false,
                proxy: false,
            },
        ] {
            let response = app(surfaces)
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/healthz")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .expect("the router answers");
            assert_eq!(
                response.status(),
                axum::http::StatusCode::OK,
                "{surfaces:?} served no /healthz"
            );
        }
    }

    #[test]
    fn per_surface_addresses_group_only_surfaces_sharing_an_address() {
        let mut config = test_config();
        config.server.api_listen = "127.0.0.1:9001".into();
        config.server.mcp_listen = "127.0.0.1:9001".into();
        let groups = listener_groups(
            &config,
            Surfaces::resolve(false, false, false),
            "127.0.0.1:9000",
        );
        assert_eq!(groups.len(), 2);
        let private = groups.get("127.0.0.1:9001").unwrap();
        assert!(private.api && private.mcp && !private.proxy);
        let shared = groups.get("127.0.0.1:9000").unwrap();
        assert!(!shared.api && !shared.mcp && shared.proxy);
    }

    fn app(surfaces: Surfaces) -> Router {
        let (core, _running) = build_core(test_config()).expect("core builds");
        compose(&core, surfaces, reqwest::Client::new())
    }

    async fn status(app: Router, method: &str, path: &str, body: &str) -> StatusCode {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        app.oneshot(request).await.unwrap().status()
    }

    /// The merge itself is the thing being tested: three routers that each claim
    /// paths, one of which claims *everything* left over, have to compose without
    /// any of them shadowing another.
    #[test]
    fn every_combination_of_surfaces_composes() {
        for (api, mcp, proxy) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
            (true, true, false),
            (true, false, true),
            (false, true, true),
            (true, true, true),
        ] {
            let _ = app(Surfaces { api, mcp, proxy });
        }
    }

    #[test]
    fn selecting_nothing_runs_everything() {
        let all = Surfaces::resolve(false, false, false);
        assert!(all.api && all.mcp && all.proxy);
        let one = Surfaces::resolve(false, false, true);
        assert!(!one.api && !one.mcp && one.proxy);
    }

    /// The proxy's catch-all must not shadow a route another surface named.
    #[tokio::test]
    async fn the_proxy_catch_all_does_not_swallow_the_api() {
        let surfaces = Surfaces {
            api: true,
            mcp: true,
            proxy: true,
        };
        assert_eq!(
            status(app(surfaces), "GET", "/healthz", "").await,
            StatusCode::OK,
            "the ops route still answers with the proxy merged in"
        );

        let body = r#"{"messages":[{"role":"user","content":"hello"}]}"#;
        let code = status(app(surfaces), "POST", "/v1/compress", body).await;
        assert_eq!(
            code,
            StatusCode::OK,
            "the api route still answers with the proxy merged in"
        );
    }

    /// And with the proxy present, a path no surface named reaches it rather
    /// than 404ing — that is the whole point of its fallback.
    #[tokio::test]
    async fn an_unclaimed_path_reaches_the_proxy() {
        let with_proxy = status(
            app(Surfaces {
                api: true,
                mcp: false,
                proxy: true,
            }),
            "POST",
            "/v1/chat/completions",
            "{}",
        )
        .await;
        // No upstream is configured in the test config, so the proxy answers for
        // itself; what matters is that it answered at all.
        assert_ne!(
            with_proxy,
            StatusCode::METHOD_NOT_ALLOWED,
            "the proxy took the request"
        );

        let without_proxy = status(
            app(Surfaces {
                api: true,
                mcp: false,
                proxy: false,
            }),
            "POST",
            "/v1/chat/completions",
            "{}",
        )
        .await;
        assert_eq!(without_proxy, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_mcp_surface_is_mounted_where_config_says() {
        let surfaces = Surfaces {
            api: false,
            mcp: true,
            proxy: false,
        };
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
        let request = Request::builder()
            .method("POST")
            .uri("/mcp")
            // a real client always sends Host; the transport requires it
            .header("host", "kompressor.internal")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(Body::from(body))
            .unwrap();
        let response = app(surfaces).oneshot(request).await.unwrap();
        assert!(
            response.status().is_success(),
            "tools/list answered {}",
            response.status()
        );
    }

    #[test]
    fn an_unparseable_listen_address_is_a_usage_error() {
        let err = "not:an:address".parse::<SocketAddr>();
        assert!(err.is_err());
    }

    fn stats_config(listen: &str, enabled: bool) -> Config {
        let mut config = Config::default();
        config.server.stats_listen = listen.to_string();
        config.stats.enabled = enabled;
        config
    }

    #[tokio::test]
    async fn the_statistics_listener_binds_only_when_named_and_statistics_are_on() {
        assert!(
            bind_stats_listener(&stats_config("", true), &[])
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            bind_stats_listener(&stats_config("127.0.0.1:0", false), &[])
                .await
                .unwrap()
                .is_none(),
            "with nothing recorded there is nothing to show; a warning, not a failure"
        );
        let (addr, _listener) = bind_stats_listener(&stats_config("127.0.0.1:0", true), &[])
            .await
            .unwrap()
            .expect("binds");
        assert!(addr.ip().is_loopback());
    }

    #[tokio::test]
    async fn a_statistics_address_that_is_not_one_or_is_taken_is_refused() {
        let bad = bind_stats_listener(&stats_config("localhost:abc", true), &[]).await;
        assert!(matches!(bad, Err(CliError::Usage(_))));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let surfaces = Surfaces::resolve(true, false, false);
        let shared = [(addr, listener, surfaces)];
        let clash = bind_stats_listener(&stats_config(&addr.to_string(), true), &shared).await;
        match clash {
            Err(CliError::Usage(message)) => assert!(message.contains("surface listener")),
            other => panic!(
                "sharing a surface's address must be refused, got {:?}",
                other.map(|o| o.map(|(a, _)| a))
            ),
        }
    }

    /// `nohup` leaves SIGHUP ignored, and the proxy must then outlive the
    /// terminal as asked rather than stop with it.
    #[cfg(unix)]
    #[test]
    fn a_hangup_nohup_ignored_is_left_alone() {
        let status = |ignored: &str| format!("Name:\tkompressor\nSigIgn:\t{ignored}\nSigCgt:\t0\n");
        assert!(hangup_ignored_in(&status("0000000000000001")));
        assert!(hangup_ignored_in(&status("0000000000001001")));
        assert!(!hangup_ignored_in(&status("0000000000001000")));
        // Unreadable counts as ignored: the default is kept, not guessed at.
        assert!(hangup_ignored_in("Name:\tkompressor\n"));
    }
}
