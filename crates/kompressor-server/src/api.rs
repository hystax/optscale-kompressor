//! The compression endpoints.

use kompressor_engine::messages::PrefixCache;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::post;
use kompressor_core::{Identity, RequestInfo};
use kompressor_engine::types::ForcedType;
use kompressor_stats::Surface;

use crate::error::{ApiError, parse_body};
use crate::middleware::{
    RequestId, extract_credentials, json_response, log_compression, log_retrieval,
    with_request_middleware,
};
use crate::state::{ServerState, run_engine};
use crate::wire::{
    CompressRawRequest, CompressRequest, Conversation, DetectRequest, DetectResponse,
    RetrieveRequest, strategy_name,
};

/// The label this surface reports itself under in spans and metrics.
pub(crate) const SURFACE: &str = "api";

/// The compression API, mounted at both `/v1` and `/v2`.
///
/// Both prefixes are served so that a client pinned to either keeps working
/// without being rewritten. They are the same routes; `/v1` is the one to use.
pub fn api_router(state: Arc<ServerState>) -> Router {
    let routes = Router::new()
        .route("/compress", post(compress))
        .route("/compress/raw", post(compress_raw))
        .route("/retrieve", post(retrieve))
        .route("/detect", post(detect))
        .merge(crate::stats_api::stats_routes())
        .merge(crate::refs::refs_routes())
        .with_state(state.clone());

    let versioned = Router::new()
        .nest("/v1", routes.clone())
        .nest("/v2", routes);

    with_request_middleware(versioned, state, SURFACE)
}

/// Resolve the caller, and put the tenant on the span.
///
/// Called before the body is read, so an unauthenticated caller cannot make the
/// server buffer sixty megabytes before being told no.
pub(crate) fn authenticate(state: &ServerState, headers: &HeaderMap) -> Result<Identity, ApiError> {
    let identity = state.core.auth().resolve(&extract_credentials(headers))?;
    tracing::Span::current().record("tenant", identity.tenant.as_str());
    Ok(identity)
}

/// Buffer the body.
///
/// The limit layer has already refused anything with an oversized
/// `Content-Length`; this catches the chunked case, where the length only
/// becomes known as the bytes arrive.
pub(crate) async fn read_body(
    state: &ServerState,
    body: Body,
) -> Result<axum::body::Bytes, ApiError> {
    axum::body::to_bytes(body, state.max_body_bytes())
        .await
        .map_err(|_| ApiError::TooLarge)
}

/// The identifier this compression will be recorded under.
///
/// A caller may name it so that its own trace id joins our stats rows; anything
/// unusable falls back to the one the middleware assigned, which always exists.
pub(crate) fn request_id(headers_id: Option<&RequestId>, body_id: Option<&str>) -> String {
    body_id
        .and_then(crate::id::sanitize)
        .or_else(|| headers_id.map(|id| id.as_str().to_string()))
        .unwrap_or_else(crate::id::new_request_id)
}

async fn compress(
    State(state): State<Arc<ServerState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, body) = request.into_parts();
    let identity = authenticate(&state, &parts.headers)?;
    let bytes = read_body(&state, body).await?;
    let body: CompressRequest = parse_body(&bytes)?;

    let options = state.core.options_for(&identity, body.options.as_ref());
    // A knob the caller set out of range is a request that would not do what it
    // says, so it is refused rather than clamped.
    options.validate().map_err(ApiError::Invalid)?;

    let request_id = request_id(
        parts.extensions.get::<RequestId>(),
        body.request_id.as_deref(),
    );
    let conversation = Conversation::parse(body.messages)?;
    let frozen = body.frozen_message_count.unwrap_or(0);
    let context = body.context.unwrap_or_default();
    let model = body.model;
    let provider = body.provider;

    if state.core.config().log.payloads {
        // Off by default, and DEBUG even when on: a conversation is tool output
        // and can hold anything the commands in it printed, credentials
        // included.
        tracing::debug!(
            messages = conversation.len(),
            body = %String::from_utf8_lossy(&bytes),
            "compressing conversation"
        );
    }

    let core = state.core.clone();
    let caller = identity.clone();
    let recorded_id = request_id.clone();
    let started = Instant::now();
    let outcome = run_engine(&state, move || {
        let mut conversation = conversation;
        let info = RequestInfo {
            request_id: &recorded_id,
            api_family: None,
            model: model.as_deref(),
            provider: provider.as_deref(),
        };
        let outcome = core.compress(
            &caller,
            conversation.messages_mut(),
            &info,
            &options,
            frozen,
            &context,
            Surface::Api,
            PrefixCache::Warm,
            // No conversation and no retrieve loop on this surface: nothing has
            // ever been fetched back, so nothing has to stay inline.
            &[],
            &[],
            // No conversation here, so nothing measured: demand the most.
            1000,
        );
        (conversation, outcome)
    })
    .await;

    let (conversation, outcome) = match outcome {
        Ok(pair) => pair,
        Err(error) => {
            if matches!(error, ApiError::Saturated) {
                crate::metrics::saturated(SURFACE);
            } else {
                crate::metrics::compress_failed(SURFACE, "error");
            }
            return Err(error);
        }
    };

    let elapsed = started.elapsed();
    crate::metrics::compression(SURFACE, &outcome.stats, elapsed);
    log_compression(identity.tenant.as_str(), &outcome.stats, elapsed);

    Ok(json_response(&serde_json::json!({
        "messages": conversation.into_json(),
        "stats": outcome.stats,
    })))
}

async fn compress_raw(
    State(state): State<Arc<ServerState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, body) = request.into_parts();
    let identity = authenticate(&state, &parts.headers)?;
    let bytes = read_body(&state, body).await?;
    let body: CompressRawRequest = parse_body(&bytes)?;

    let options = state.core.options_for(&identity, body.options.as_ref());
    options.validate().map_err(ApiError::Invalid)?;

    let forced = match body.content_type.as_deref() {
        None | Some("") | Some("auto") => None,
        Some(name) => Some(ForcedType::from_str(name).map_err(ApiError::Invalid)?),
    };

    let request_id = request_id(
        parts.extensions.get::<RequestId>(),
        body.request_id.as_deref(),
    );
    let content = body.content;

    if state.core.config().log.payloads {
        tracing::debug!(chars = content.chars().count(), payload = %content, "compressing payload");
    }

    let core = state.core.clone();
    let caller = identity.clone();
    let recorded_id = request_id.clone();
    let started = Instant::now();
    let result = run_engine(&state, move || {
        let info = RequestInfo {
            request_id: &recorded_id,
            api_family: None,
            model: None,
            provider: None,
        };
        core.compress_raw(&caller, &content, forced, &info, &options, Surface::Api)
    })
    .await;

    let (compressed, outcome) = match result {
        Ok(pair) => pair,
        Err(error) => {
            if matches!(error, ApiError::Saturated) {
                crate::metrics::saturated(SURFACE);
            } else {
                crate::metrics::compress_failed(SURFACE, "error");
            }
            return Err(error);
        }
    };

    let elapsed = started.elapsed();
    crate::metrics::compression(SURFACE, &outcome.stats, elapsed);
    log_compression(identity.tenant.as_str(), &outcome.stats, elapsed);

    Ok(json_response(&serde_json::json!({
        "content": compressed,
        "stats": outcome.stats,
    })))
}

async fn retrieve(
    State(state): State<Arc<ServerState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, body) = request.into_parts();
    let identity = authenticate(&state, &parts.headers)?;
    let bytes = read_body(&state, body).await?;
    let body: RetrieveRequest = parse_body(&bytes)?;

    let request_id = request_id(
        parts.extensions.get::<RequestId>(),
        body.request_id.as_deref(),
    );
    let handle = body.reference;
    let range = body.range;

    let core = state.core.clone();
    let caller = identity.clone();
    let recorded_id = request_id.clone();
    let asked_for = handle.clone();

    // Deliberately not behind the engine semaphore: a retrieval is a store
    // read, not engine work, and queueing it behind a saturated engine would
    // stall the very path that makes recoverable compression safe.
    let result = tokio::task::spawn_blocking(move || {
        let info = RequestInfo {
            request_id: &recorded_id,
            api_family: None,
            model: None,
            provider: None,
        };
        core.retrieve(&caller, &asked_for, range.as_ref(), &info, Surface::Api)
    })
    .await
    .map_err(|error| ApiError::Internal(format!("retrieve task: {error}")))?;

    crate::metrics::retrieval(SURFACE, &result);
    log_retrieval(identity.tenant.as_str(), &handle, &result);
    if state.core.config().log.payloads {
        tracing::debug!(
            fragment = result.content.as_deref().unwrap_or(""),
            "retrieved"
        );
    }

    let mut value = serde_json::to_value(&result)
        .map_err(|error| ApiError::Internal(format!("could not render retrieval: {error}")))?;
    if !result.found {
        // A model that cannot fetch its fragment must regenerate it, never
        // invent it, and the only place to say so is the response it reads.
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "hint".to_string(),
                serde_json::Value::String(result.miss_hint().to_string()),
            );
        }
    }
    Ok(json_response(&value))
}

async fn detect(
    State(state): State<Arc<ServerState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, body) = request.into_parts();
    let identity = authenticate(&state, &parts.headers)?;
    let bytes = read_body(&state, body).await?;
    let body: DetectRequest = parse_body(&bytes)?;

    let options = state.core.options_for(&identity, None);
    let content = body.content;
    let role = body.role.unwrap_or_else(|| "user".to_string());

    let detection = run_engine(&state, move || {
        let detection = kompressor_engine::detect::detect_result(&content, &role, &options);
        let strategy = kompressor_engine::detect::strategy_for(&detection, &options);
        (detection, strategy)
    })
    .await;

    let (detection, strategy) = match detection {
        Ok(pair) => pair,
        Err(error) => {
            if matches!(error, ApiError::Saturated) {
                crate::metrics::saturated(SURFACE);
            }
            return Err(error);
        }
    };

    Ok(json_response(&DetectResponse {
        content_type: detection.content_type.as_str(),
        confidence: detection.confidence,
        strategy: strategy_name(strategy),
        kind: detection.content_type.kind().as_str(),
        meta: detection.meta,
    }))
}
