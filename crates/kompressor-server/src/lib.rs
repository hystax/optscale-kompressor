//! The HTTP surface, and the pieces the other surfaces are built from.
//!
//! This crate is deliberately thin: it translates JSON into the calls
//! [`kompressor_core::AppCore`] already exposes and translates the answers
//! back. What it does own is everything a *surface* needs and the core does
//! not — the shared axum state, the concurrency bound in front of a synchronous
//! engine, the error envelope, the request identity, the span, the metrics —
//! and it owns those for the tool server and the proxy as well, so the three
//! cannot drift apart.
//!
//! # What the other surfaces use
//!
//! - [`ServerState`] and [`run_engine`]: never call the engine directly. It is
//!   synchronous and CPU-bound, so it runs on a blocking thread behind a
//!   semaphore, and a caller that cannot get a permit is told to send its prompt
//!   uncompressed rather than queue behind a saturated engine.
//! - [`extract_credentials`]: one reading of the credential headers, so a key
//!   accepted by the API is accepted by the tool server and the proxy too.
//! - [`with_request_middleware`]: the request id, span, body limit and timeout,
//!   applied to any router. It is idempotent — a router that is already wrapped
//!   keeps the id it was given — so mounting our routes inside another surface's
//!   stack is safe.
//! - [`api_router`] and [`ops_router`]: the routes themselves.
//!
//! # Versioned paths
//!
//! Every `/v1` path is also served at `/v2`. The service this replaces served
//! `/v2`, and an existing client should not have to be rewritten to point at
//! this one.

#![forbid(unsafe_code)]

pub mod error;
pub mod id;
pub mod metrics;
pub mod middleware;
pub mod state;
pub mod wire;

mod api;
pub mod live;
mod ops;
pub mod refs;
mod stats_api;

pub use api::api_router;
pub use error::ApiError;
pub use live::stats_listener_router;
pub use middleware::{RequestId, extract_credentials, with_request_middleware};
pub use ops::ops_router;
pub use state::{ServerState, run_engine};

use std::sync::Arc;

/// Every route this crate serves: the API and the operational endpoints.
///
/// A surface that adds routes of its own merges them with this rather than
/// re-deriving the mounting; a surface that wants only one half mounts
/// [`api_router`] or [`ops_router`] directly.
pub fn router(state: Arc<ServerState>) -> axum::Router {
    api_router(state.clone()).merge(ops_router(state))
}
