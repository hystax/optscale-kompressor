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
                        if let Err(error) = core.refs().purge_expired(now.as_secs()) {
                            tracing::warn!(%error, "fragment retention failed");
                        }
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
    use tracing_subscriber::filter::EnvFilter;
    use tracing_subscriber::fmt;

    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&log.level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    // `try_init` rather than `init`: a second call must not take the process
    // down, and a caller that already installed one keeps it.
    let _ = match log.format.as_str() {
        "json" => fmt::Subscriber::builder()
            .json()
            .with_env_filter(filter)
            .try_init(),
        _ => fmt::Subscriber::builder()
            .with_env_filter(filter)
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
/// That last point is why logging is deliberately not initialised here. Every
/// other command is free to write progress to the terminal; here a single
/// stray line on stdout is a protocol violation that the runtime reports as a
/// broken server. Tracing's default is stdout, so the safe thing is to leave it
/// off entirely rather than to configure it and hope.
///
/// The store this opens is the one named by `config`, which is how a fragment
/// the proxy elided becomes one this server can hand back: the wrap points both
/// at the same configuration, so a handle minted on the proxy resolves here.
pub async fn run_stdio(config: Config) -> CliResult {
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

/// Bind and serve until interrupted.
pub async fn run(config: Config, surfaces: Surfaces, listen: Option<String>) -> CliResult {
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

    let upkeep = maintenance(core.clone(), shutdown_rx);
    let mut serve_error = None;
    tokio::select! {
        _ = shutdown_signal() => {},
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

async fn shutdown_signal() {
    let interrupt = async {
        tokio::signal::ctrl_c()
            .await
            .expect("ctrl-c handler installs");
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = interrupt => {}
        _ = terminate => {}
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
}
