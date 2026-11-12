//! What every surface shares, and the one way any of them may call the engine.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kompressor_core::AppCore;
use kompressor_stats::StatsError;
use kompressor_stats::reader::StatsReader;
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::sync::Semaphore;

use crate::error::ApiError;

/// How long a request may take before the stack gives up on it.
///
/// A deployment-level knob in the design that the shared config does not carry
/// yet, so it lives here as a constant rather than as a second spelling of a
/// setting. It bounds the whole request including the queue wait, and is far
/// above anything a healthy compression takes; a request that hits it has found
/// a pathological payload, not a busy server.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Everything a request handler needs, built once at startup.
pub struct ServerState {
    /// Configuration, tenants, and the compress/retrieve operations themselves.
    pub core: Arc<AppCore>,
    metrics: PrometheusHandle,
    /// Permits for engine work: [`AppCore::engine_slots`], shared with every
    /// other surface. Sized [`kompressor_core::config::ServerConfig::inflight_limit`].
    inflight: Arc<Semaphore>,
    queue_timeout: Duration,
    /// Opened on first use, because a server can start before the statistics
    /// writer has created the file it will read.
    stats: Mutex<Option<StatsReader>>,
    stats_path: Option<PathBuf>,
}

impl ServerState {
    /// Build the shared state for `core`.
    pub fn new(core: Arc<AppCore>) -> Arc<ServerState> {
        let config = core.config();
        let stats_path = config.stats.enabled.then(|| config.stats.path.clone());
        let state = ServerState {
            metrics: crate::metrics::install(),
            // The process-wide permits, shared with the proxy and MCP, so
            // `max_inflight_compressions` bounds every surface together.
            inflight: core.engine_slots().clone(),
            queue_timeout: config.server.queue_timeout,
            stats: Mutex::new(None),
            stats_path,
            core,
        };
        Arc::new(state)
    }

    /// The renderer behind `/metrics`.
    pub fn metrics(&self) -> &PrometheusHandle {
        &self.metrics
    }

    /// Engine permits currently free. Zero means the next caller queues.
    pub fn available_permits(&self) -> usize {
        self.inflight.available_permits()
    }

    /// The configured body limit, in bytes.
    pub fn max_body_bytes(&self) -> usize {
        self.core.config().server.max_body_bytes
    }

    /// Run `f` against the statistics database.
    ///
    /// The reader owns a SQLite connection, which is `Send` but not `Sync`, so
    /// it sits behind a mutex and every query runs on a blocking thread: a
    /// stats page is a scan, and scanning on the runtime's executor stalls the
    /// compressions sharing it.
    pub(crate) async fn with_stats<T, F>(self: &Arc<Self>, f: F) -> Result<T, ApiError>
    where
        T: Send + 'static,
        F: FnOnce(&StatsReader) -> Result<T, StatsError> + Send + 'static,
    {
        let state = self.clone();
        let Some(path) = state.stats_path.clone() else {
            return Err(ApiError::NotFound(
                "statistics are disabled on this deployment".to_string(),
            ));
        };

        let joined = tokio::task::spawn_blocking(move || {
            // A poisoned lock means a previous query panicked. The connection
            // is read-only and a panic cannot have left it half-written, so
            // recovering beats making every later stats request fail.
            let mut guard = state
                .stats
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if guard.is_none() {
                *guard = Some(StatsReader::open(&path)?);
            }
            let reader = guard.as_ref().expect("just opened");
            f(reader)
        })
        .await;

        match joined {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(ApiError::Internal(format!("statistics: {error}"))),
            Err(error) => Err(ApiError::Internal(format!("statistics task: {error}"))),
        }
    }
}

/// Run one piece of engine work.
///
/// The engine is synchronous and CPU-bound, so it runs on a blocking thread and
/// behind a semaphore: without the bound, a burst of large payloads would put
/// every core on compression and leave the streaming relays sharing the runtime
/// with nothing to run on.
///
/// A caller that cannot get a permit within `server.queue_timeout` is refused
/// with 503 rather than queued. That is not a failure for it to retry: the
/// correct response is to send the prompt uncompressed, which costs tokens and
/// is strictly better than making the user wait behind an engine that is
/// already at capacity.
pub async fn run_engine<T, F>(state: &ServerState, f: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let permit =
        tokio::time::timeout(state.queue_timeout, state.inflight.clone().acquire_owned()).await;

    let permit = match permit {
        Ok(Ok(permit)) => permit,
        Ok(Err(_closed)) => {
            // The semaphore is never closed while the process runs; if it ever
            // is, refusing reads the same to the caller as saturation.
            return Err(ApiError::Saturated);
        }
        Err(_elapsed) => return Err(ApiError::Saturated),
    };

    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        f()
    })
    .await
    .map_err(|error| ApiError::Internal(format!("engine task: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kompressor_core::Config;
    use kompressor_store::memory::MemoryRefStore;
    use std::time::Instant;

    fn state(inflight: usize, queue_timeout: Duration) -> Arc<ServerState> {
        let mut config = Config::default();
        config.server.max_inflight_compressions = inflight;
        config.server.queue_timeout = queue_timeout;
        config.stats.enabled = false;
        ServerState::new(Arc::new(AppCore::new(
            config,
            Arc::new(MemoryRefStore::new()),
            kompressor_stats::null_sink(),
        )))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn engine_work_runs_off_the_runtime_and_comes_back() {
        let state = state(2, Duration::from_millis(50));
        let got = run_engine(&state, || 2 + 2).await.unwrap();
        assert_eq!(got, 4);
        assert_eq!(state.available_permits(), 2, "the permit is returned");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_caller_that_cannot_get_a_permit_in_time_is_told_to_go_uncompressed() {
        let state = state(1, Duration::from_millis(30));
        let blocker = {
            let state = state.clone();
            tokio::spawn(async move {
                run_engine(&state, || std::thread::sleep(Duration::from_millis(400)))
                    .await
                    .unwrap();
            })
        };
        // Let the blocking task take the only permit before competing for it.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let started = Instant::now();
        let refused = run_engine(&state, || 1).await.unwrap_err();
        assert_eq!(
            refused.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "the refusal comes from the queue timeout, not from waiting out the work"
        );
        blocker.await.unwrap();
    }

    /// The API's permits are the process's: work the proxy or MCP holds
    /// counts against `max_inflight_compressions` here too. Before the
    /// 2026-11-11 review this surface owned the only semaphore, and the other
    /// two ran unbounded beside it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_api_shares_its_engine_permits_with_every_other_surface() {
        let state = state(1, Duration::from_millis(30));
        // What the proxy or MCP does with the core's permit.
        let held = state
            .core
            .engine_slots()
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        assert_eq!(state.available_permits(), 0);
        let refused = run_engine(&state, || 1).await.unwrap_err();
        assert_eq!(
            refused.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        drop(held);
        assert_eq!(run_engine(&state, || 1).await.unwrap(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_panic_in_engine_work_is_a_500_rather_than_a_dead_runtime() {
        let state = state(2, Duration::from_millis(50));
        let error = run_engine(&state, || panic!("a compressor gave up"))
            .await
            .unwrap_err();
        assert_eq!(
            error.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
        // and the runtime is still usable afterwards
        assert_eq!(run_engine(&state, || 7).await.unwrap(), 7);
    }
}
