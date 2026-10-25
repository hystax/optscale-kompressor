//! A transparent reverse proxy in front of an LLM provider.
//!
//! Point a client's base URL at this and nothing about the client changes: the
//! same endpoints, the same request bodies, the same credentials, the same
//! streamed responses. What changes is what the provider is charged for — the
//! prompt arrives compressed — and what the model can do about it: it is given
//! a tool that fetches back anything the compression elided, and this proxy
//! executes that tool itself, so a client that has never heard of kompressor
//! still benefits from recoverable mode.
//!
//! # The contract
//!
//! **Fail open, always.** Every stage between reading the body and forwarding
//! it can decline — a body that is not JSON, a shape no walker recognises, an
//! engine that ran past its deadline or panicked, a compression that did not
//! shrink anything — and declining means one thing: the request the client sent
//! is the request the provider gets. Compression subtracts tokens, never
//! availability. Where a stage declines is counted, per stage, on
//! [`FailOpen`], because a deployment whose compression silently stopped
//! working otherwise looks exactly like one with nothing to compress.
//!
//! **The provider's answer is the client's answer.** An upstream error is
//! relayed as it arrived — status, headers and body. This proxy never turns a
//! provider's 200 into a 5xx of its own. The only statuses it originates are a
//! 404 for a path no upstream claims, a 413 for a body over the cap, and a 502
//! when the upstream could not be reached at all.
//!
//! **Bytes the proxy did not mean to change do not change.** A request below
//! the size gate reaches the provider byte-identical; so does one in a family
//! we do not walk, one with no compressible text, and one the engine declined.
//! Frames of a streamed response are relayed as the bytes they arrived as,
//! never decoded and re-encoded, except for a single usage frame when rounds
//! were hidden from the client's accounting.
//!
//! # What it does, in order (design §12.3)
//!
//! 1. Resolve a request id, a tenant and a mode. A mode of `off` forwards.
//! 2. Match the path to an upstream by prefix; a path nothing claims is a 404.
//! 3. Read the body, parse it, walk it for the family's compressible text.
//! 4. Gate on `proxy.min_tokens`, measured with the *engine's own* estimator.
//!    Gating on a cheap `len/4` while reporting savings from a different
//!    estimator makes the skip decision and the reported saving answer
//!    different questions.
//! 5. Compress off-runtime under a hard deadline, and write the result back
//!    through the walker, which touches only slots the engine changed.
//! 6. If the outgoing request carries a handle, offer the retrieve tool.
//! 7. Forward, relaying the answer — running the retrieve loop where the model
//!    asks for a fragment back, on streamed responses as well as buffered ones.
//! 8. Record what the provider billed.
//!
//! # Usage
//!
//! ```no_run
//! use std::sync::Arc;
//! # use kompressor_core::AppCore;
//! # fn build_core() -> Arc<AppCore> { unimplemented!() }
//! let core = build_core();
//! let client = reqwest::Client::new();
//! let app = axum::Router::new().merge(kompressor_proxy::proxy_router(core, client));
//! ```

#![forbid(unsafe_code)]

pub mod authmode;
mod bodycapture;
pub mod bodycodec;
mod cachettl;
pub mod failopen;
mod handler;
pub mod headers;
pub mod intercept;
pub mod mode;
mod nonstream;
mod piggyback;
pub mod prefixcache;
pub mod retrieve;
pub mod route;
pub mod sse;
mod stream;
pub mod tool;
pub mod turn;
mod upstream;
pub mod websocket;

use std::sync::Arc;

use axum::Router;

use kompressor_core::AppCore;

pub use crate::failopen::{FailOpen, Stage};
pub use crate::mode::ProxyMode;
pub use crate::tool::RETRIEVE_TOOL_NAME;

/// Everything the proxy needs, cheap to clone per request.
#[derive(Clone)]
pub struct ProxyState {
    pub(crate) core: Arc<AppCore>,
    pub(crate) client: reqwest::Client,
    pub(crate) failopen: Arc<FailOpen>,
    /// What the provider last reported about each frozen prefix, so the engine
    /// can be told when a freeze is protecting a cache that is already gone.
    /// Shared across requests, which is the whole point: one request's usage is
    /// the next one's evidence.
    pub(crate) prefixes: Arc<prefixcache::PrefixTracker>,
}

impl ProxyState {
    /// Build the state.
    ///
    /// The `reqwest::Client` comes from the caller rather than being built
    /// here: connection pooling, timeouts and TLS are deployment decisions, and
    /// one pool shared with the rest of the process is the point of passing it
    /// in. Configure its connect and read timeouts from
    /// `proxy.upstream_connect_timeout` and `proxy.upstream_read_timeout`; a
    /// buffered request is additionally bounded here, while a streamed one
    /// deliberately is not.
    pub fn new(core: Arc<AppCore>, client: reqwest::Client) -> ProxyState {
        let ttl = core.config().proxy.prefix_cache_ttl;
        ProxyState {
            core,
            client,
            failopen: Arc::new(FailOpen::default()),
            prefixes: Arc::new(prefixcache::PrefixTracker::new(ttl)),
        }
    }

    /// Per-stage fail-open counts, for `/metrics` and for tests.
    pub fn failopen(&self) -> Arc<FailOpen> {
        self.failopen.clone()
    }

    /// What the proxy remembers about each conversation's cached prefix.
    /// Exposed on the same terms as `failopen`: for `/metrics` and for tests.
    pub fn prefixes(&self) -> Arc<prefixcache::PrefixTracker> {
        self.prefixes.clone()
    }

    /// The router that serves every proxied path.
    pub fn into_router(self) -> Router {
        Router::new().fallback(handler::handle).with_state(self)
    }
}

/// The proxy's router, ready to be merged with the other surfaces.
///
/// It claims every path, because a reverse proxy in front of a provider has to:
/// which paths exist is the provider's business, and a path no upstream claims
/// is answered with a 404 rather than left to another surface. Merge it last,
/// or mount it under its own listener.
pub fn proxy_router(core: Arc<AppCore>, client: reqwest::Client) -> Router {
    ProxyState::new(core, client).into_router()
}
