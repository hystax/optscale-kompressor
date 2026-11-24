//! Liveness, readiness, metrics and the effective configuration.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::api::authenticate;
use crate::error::ApiError;
use crate::middleware::{json_response, with_request_middleware};
use crate::state::ServerState;

/// How long a readiness check may take before it counts as a failure.
///
/// A probe that hangs is worse than one that fails: an orchestrator waiting on
/// it keeps a broken instance in the rotation for as long as the hang lasts.
const READY_TIMEOUT: Duration = Duration::from_secs(2);

/// The operational endpoints.
///
/// `/v1/config` is versioned like the rest of the API and is therefore served
/// at `/v2/config` too; the probes and `/metrics` are not versioned, because
/// the thing scraping them is an orchestrator, not a client of ours.
pub fn ops_router(state: Arc<ServerState>) -> Router {
    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(render_metrics))
        .route("/v1/config", get(config))
        .route("/v2/config", get(config))
        .with_state(state.clone());

    with_request_middleware(router, state, "ops")
}

/// Liveness: is this process running.
///
/// It touches nothing — not the store, not the statistics database, not the
/// configuration — because a liveness probe that consults a dependency turns a
/// broken dependency into a restart loop, and restarting the server does not
/// repair a database.
async fn healthz() -> Response {
    json_response(&serde_json::json!({ "status": "ok" }))
}

/// Readiness: can this process serve a request right now.
///
/// Unlike liveness this does consult the dependencies, which is the difference
/// between the two: an instance whose ref store will not answer can still be
/// alive and should still be taken out of the rotation, because every
/// compression it serves mints handles that will never resolve.
async fn readyz(State(state): State<Arc<ServerState>>) -> Response {
    let core = state.core.clone();
    let store_ok = matches!(
        tokio::time::timeout(
            READY_TIMEOUT,
            tokio::task::spawn_blocking(move || core.refs().ping()),
        )
        .await,
        Ok(Ok(Ok(_)))
    );

    let sink = state.core.stats().clone();
    let stats_ok = if sink.is_enabled() {
        // `flush` is the only liveness question the sink can answer: it fails
        // when the writer thread is gone, which is exactly the state in which
        // this instance is silently recording nothing.
        matches!(
            tokio::time::timeout(
                READY_TIMEOUT,
                tokio::task::spawn_blocking(move || sink.flush()),
            )
            .await,
            Ok(Ok(Ok(())))
        )
    } else {
        true
    };

    let ready = store_ok && stats_ok;
    let body = serde_json::json!({
        "status": if ready { "ready" } else { "not ready" },
        "checks": { "ref_store": store_ok, "stats": stats_ok },
    });
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let mut response = json_response(&body);
    *response.status_mut() = status;
    response
}

/// Prometheus exposition.
async fn render_metrics(State(state): State<Arc<ServerState>>) -> Response {
    if let Some(store) = state.store_sample().await {
        crate::metrics::sample_gauges(
            store.live_refs,
            store.live_bytes,
            state.core.stats().dropped(),
        );
    }

    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics().render(),
    )
        .into_response()
}

/// The effective configuration, with every secret replaced.
///
/// Rendered by [`kompressor_core::Config::redacted_toml`] rather than by
/// anything here: one function decides what a secret is, so a field added to
/// the configuration cannot be secret in the file dump and public in the API.
async fn config(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    // Authenticated whenever auth is on: the effective configuration names
    // upstreams, paths and tenants, which is reconnaissance even with the keys
    // taken out.
    authenticate(&state, &headers)?;
    let rendered = state.core.config().redacted_toml();
    Ok((
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        rendered,
    )
        .into_response())
}
