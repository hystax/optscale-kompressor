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
pub mod drift;
pub mod failopen;
pub mod guard;
mod handler;
pub mod headers;
pub mod intercept;
pub mod keepwarm;
pub mod mode;
mod netgate;
mod nonstream;
pub mod outputshape;
pub mod persist;
mod piggyback;
pub mod prefixcache;
mod reasoning;
pub mod recompact;
mod retention;
pub mod retrieve;
pub mod route;
pub mod sigv4;
pub mod sse;
pub mod stabilize;
mod stream;
pub mod tool;
pub mod toolcompact;
pub mod tooldefer;
pub mod turn;
mod upstream;
pub mod websocket;

use std::sync::Arc;

use axum::Router;

use kompressor_core::AppCore;

pub use crate::failopen::{FailOpen, Stage};
pub use crate::guard::{Guard, Guards};
pub use crate::mode::ProxyMode;
pub use crate::tool::RETRIEVE_TOOL_NAME;

/// Everything the proxy needs, cheap to clone per request.
#[derive(Clone)]
pub struct ProxyState {
    pub(crate) core: Arc<AppCore>,
    pub(crate) client: reqwest::Client,
    pub(crate) failopen: Arc<FailOpen>,
    /// Trips of the stability guards (`proxy.guard_*`), beside `failopen`.
    pub(crate) guards: Arc<Guards>,
    /// What the provider last reported about each frozen prefix, so the engine
    /// can be told when a freeze is protecting a cache that is already gone.
    /// Shared across requests, which is the whole point: one request's usage is
    /// the next one's evidence.
    pub(crate) prefixes: Arc<prefixcache::PrefixTracker>,
    /// The keep-warm timers, one per conversation waiting on a tool result.
    /// Shared for the reason `prefixes` is: one request arms what the next
    /// request in the conversation cancels.
    pub(crate) keepwarm: Arc<keepwarm::KeepWarm>,
    /// The upstreams and models that refused `prompt_cache_retention`, so it
    /// is not sent there again.
    pub(crate) retention: Arc<retention::Rejections>,
    /// The previous request of each conversation, reduced to hashes, so the
    /// next can say which part of the prefix moved and on which side of this
    /// proxy. Observation only: nothing reads it to decide what to send.
    pub(crate) drift: Arc<drift::DriftTracker>,
    /// The tool order and beta tokens each conversation's first request
    /// established, for `proxy.tools_order` and `proxy.sticky_beta`.
    pub(crate) stabilizer: Arc<stabilize::Stabilizer>,
    /// What each conversation's cold turns decided, for `proxy.cold_recompact`.
    pub(crate) recompact: Arc<recompact::Recompact>,
    /// Whether each conversation's tool schemas are compacted, decided when
    /// it is first seen, for `proxy.tools_compact`.
    pub(crate) tool_policies: Arc<toolcompact::ToolPolicies>,
    /// Each conversation's arm of `proxy.output_shaping`, decided when it is
    /// first seen.
    pub(crate) output_shaping: Arc<outputshape::OutputShaping>,
    /// Whether each conversation's tools are deferred under
    /// `proxy.tools_defer`, decided when it is first seen.
    pub(crate) defer_policies: Arc<toolcompact::ToolPolicies>,
    /// Set by [`ProxyState::fail_next_encode_for_test`]. Never set otherwise.
    pub(crate) encode_fault: Arc<std::sync::atomic::AtomicBool>,
    /// Held while the state file is written, so a timed save and the
    /// shutdown save never write one temporary file together.
    pub(crate) saving: Arc<std::sync::Mutex<()>>,
    /// What reading `proxy.state_path` at start came to.
    pub(crate) loaded: Arc<persist::Loaded>,
    /// Set by [`ProxyState::corrupt_next_rewrite_for_test`]. Never set
    /// otherwise.
    pub(crate) rewrite_fault: Arc<std::sync::Mutex<Option<RewriteFault>>>,
}

/// A change made to the next rewritten body, standing in for a rewrite that
/// went wrong. See [`ProxyState::corrupt_next_rewrite_for_test`].
pub(crate) type RewriteFault = Box<dyn FnOnce(&mut serde_json::Value) + Send>;

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
        let prefixes = Arc::new(prefixcache::PrefixTracker::new(ttl));
        let keepwarm = Arc::new(keepwarm::KeepWarm::default());
        let recompact = Arc::new(recompact::Recompact::default());
        let failopen = Arc::new(FailOpen::default());
        let guards = Arc::new(Guards::default());
        // All of it lives in this process, which is what a restart costs
        // unless `proxy.state_path` keeps it (`persist`): the Maintenance page
        // asks for the count.
        let held = (
            prefixes.clone(),
            keepwarm.clone(),
            recompact.clone(),
            failopen.clone(),
            guards.clone(),
        );
        core.runtime().holding(move || {
            let (prefixes, keepwarm, recompact, failopen, guards) = &held;
            let (conversations, frozen, one_hour) = prefixes.held();
            kompressor_core::runtime::Held {
                conversations,
                frozen,
                one_hour,
                keep_warm: keepwarm.armed(),
                recompacted: recompact.tracked(),
                predicting: recompact.predicting(),
                wrong_predictions: recompact.wrong_predictions(),
                guard_trips: guards.total(),
                guard_enforced: guards.enforced(),
                failopen_after_compressed: failopen.count(Stage::AfterCompressed),
            }
        });
        let mut state = ProxyState {
            core,
            client,
            failopen,
            guards,
            prefixes,
            keepwarm,
            retention: Arc::new(retention::Rejections::default()),
            drift: Arc::new(drift::DriftTracker::default()),
            stabilizer: Arc::new(stabilize::Stabilizer::default()),
            recompact,
            tool_policies: Arc::new(toolcompact::ToolPolicies::default()),
            output_shaping: Arc::new(outputshape::OutputShaping::default()),
            defer_policies: Arc::new(toolcompact::ToolPolicies::default()),
            encode_fault: Arc::default(),
            rewrite_fault: Arc::default(),
            saving: Arc::default(),
            loaded: Arc::new(persist::Loaded::Off),
        };
        // Before the state serves anything, so no request can be decided
        // against a tracker that is about to be replaced. Never fails: a file
        // that cannot be used is logged and left alone.
        state.loaded = Arc::new(persist::load(&state));
        state
    }

    /// Write what this proxy remembers about each conversation to
    /// `proxy.state_path`, atomically. `Ok(None)` when no path is set.
    ///
    /// Blocking file IO: call it off the async runtime. `serve` calls it on
    /// a timer and once more after the listeners have drained.
    pub fn save_state(&self) -> Result<Option<std::path::PathBuf>, persist::SaveError> {
        persist::save(self)
    }

    /// What reading the state file at start came to.
    pub fn loaded(&self) -> &persist::Loaded {
        &self.loaded
    }

    /// The pinned tool orders and beta tokens. Exposed on the same terms as
    /// `prefixes`.
    pub fn stabilizer(&self) -> Arc<stabilize::Stabilizer> {
        self.stabilizer.clone()
    }

    /// The tool-compaction decisions. Exposed on the same terms as
    /// `prefixes`.
    pub fn tool_policies(&self) -> Arc<toolcompact::ToolPolicies> {
        self.tool_policies.clone()
    }

    /// Per-stage fail-open counts, for `/metrics` and for tests.
    pub fn failopen(&self) -> Arc<FailOpen> {
        self.failopen.clone()
    }

    /// Stability guard trips, for tests and the Maintenance page. Exposed on
    /// the same terms as `failopen`.
    pub fn guards(&self) -> Arc<Guards> {
        self.guards.clone()
    }

    /// What the proxy remembers about each conversation's cached prefix.
    /// Exposed on the same terms as `failopen`: for `/metrics` and for tests.
    pub fn prefixes(&self) -> Arc<prefixcache::PrefixTracker> {
        self.prefixes.clone()
    }

    /// The proxied requests under way as this is called, which the
    /// Maintenance page shows. Exposed on the same terms as `prefixes`.
    pub fn under_way(&self) -> kompressor_core::runtime::InFlight {
        self.core.runtime().in_flight()
    }

    /// What this proxy holds in memory alone, as the Maintenance page shows
    /// it. Exposed on the same terms as `prefixes`.
    pub fn held(&self) -> Option<kompressor_core::runtime::Held> {
        self.core.runtime().held()
    }

    /// What each conversation's cold turns decided, and how its lapse
    /// predictions have fared. Exposed on the same terms as `prefixes`.
    pub fn recompact(&self) -> Arc<recompact::Recompact> {
        self.recompact.clone()
    }

    /// Have the next rewritten body fail to re-encode in its client's coding,
    /// as an encoder that errors would.
    ///
    /// For tests: no input makes zstd or gzip fail on demand, and what the
    /// proxy does when one does is a path that has to hold. Nothing in the
    /// proxy calls it.
    #[doc(hidden)]
    pub fn fail_next_encode_for_test(&self) {
        self.encode_fault
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Apply `fault` to the next body this proxy rewrites, after every
    /// rewrite and before the stability guards look at it, as a rewrite with
    /// a defect would.
    ///
    /// For tests, on the terms of [`Self::fail_next_encode_for_test`]: no
    /// rewrite this proxy makes is known to break what the guards check, and
    /// what happens when one does is a path that has to hold. A body the
    /// proxy forwards unchanged is left alone and the fault waits. Nothing in
    /// the proxy calls it.
    #[doc(hidden)]
    pub fn corrupt_next_rewrite_for_test(
        &self,
        fault: impl FnOnce(&mut serde_json::Value) + Send + 'static,
    ) {
        if let Ok(mut slot) = self.rewrite_fault.lock() {
            *slot = Some(Box::new(fault));
        }
    }

    /// The keep-warm timers. Exposed on the same terms as `prefixes`.
    pub fn keepwarm(&self) -> Arc<keepwarm::KeepWarm> {
        self.keepwarm.clone()
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
