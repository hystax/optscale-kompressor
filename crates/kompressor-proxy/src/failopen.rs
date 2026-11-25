//! Where the proxy gave up and forwarded the request as it arrived.
//!
//! Fail-open is the proxy's whole contract, which makes it invisible by
//! construction: a deployment whose compression silently stopped working looks
//! exactly like one whose traffic is below the size gate. Counting the give-ups
//! by stage is what tells those two apart, and it is the only reason an
//! operator ever learns that, say, every request is timing out in the engine.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// The point in the pipeline at which a request stopped being compressed.
///
/// These are the label values of `kompressor_proxy_failopen_total{stage}` and
/// the `stage` field on the warning each one logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// A credential the proxy could not resolve to a tenant.
    Auth,
    /// The body was not JSON.
    Parse,
    /// No walker recognised the body's shape.
    Walk,
    /// No engine permit came free before the compression deadline. The
    /// process is at its `server.max_inflight_compressions`; this request went
    /// uncompressed rather than queue behind it.
    Admission,
    /// The engine timed out, panicked, or errored — retaining the result in
    /// the fragment store included, since that is inside the same deadline,
    /// and so is a task that only started after the deadline had passed.
    Compress,
    /// Compression produced nothing smaller, or nothing at all.
    Discarded,
    /// The retrieve tool could not be added to the outgoing request.
    Inject,
    /// A retrieve round failed upstream, so the previous response stands.
    RetrieveRound,
    /// A stability guard under `enforce` found a fault the rewrite
    /// introduced, so the client's original went out. See [`crate::guard`].
    Guard,
    /// Not a point in the pipeline but a second count of a request already
    /// counted at one: its body went out as the client sent it, and an
    /// earlier turn of the same conversation had gone out compressed.
    ///
    /// The case that matters is size. A conversation whose history has been
    /// going out compressed can have grown past the model's context window in
    /// its original form, and the one uncompressed turn is then refused by
    /// the provider rather than merely billed in full. Headroom refuses large
    /// WebSocket frames in that case; this only counts it, so the rate can be
    /// read before deciding whether anything more is needed. A request whose
    /// conversation could not be identified (a body that did not parse, an
    /// unknown credential) is never counted here.
    AfterCompressed,
}

impl Stage {
    /// The label this stage carries in metrics and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Auth => "auth",
            Stage::Parse => "parse",
            Stage::Walk => "walk",
            Stage::Admission => "admission",
            Stage::Compress => "compress",
            Stage::Discarded => "discarded",
            Stage::Inject => "inject",
            Stage::RetrieveRound => "retrieve_round",
            Stage::Guard => "guard",
            Stage::AfterCompressed => "after_compressed",
        }
    }
}

/// Per-stage fail-open counts for one proxy.
///
/// Held behind an `Arc` on the router's state, so the binary can export the
/// same numbers it serves `/metrics` from.
#[derive(Debug, Default)]
pub struct FailOpen {
    auth: AtomicU64,
    parse: AtomicU64,
    walk: AtomicU64,
    admission: AtomicU64,
    compress: AtomicU64,
    discarded: AtomicU64,
    inject: AtomicU64,
    retrieve_round: AtomicU64,
    guard: AtomicU64,
    after_compressed: AtomicU64,
}

impl FailOpen {
    /// Record one give-up, with the reason on the log line.
    ///
    /// The reason is `Display`ed rather than stored: the counter answers "how
    /// often", the log answers "why this one", and keeping the last reason in
    /// memory would answer neither well.
    pub fn record(&self, stage: Stage, reason: &dyn std::fmt::Display) {
        self.counter(stage).fetch_add(1, Ordering::Relaxed);
        // Outside a [`track`]ed request (a stream relayed on its own task,
        // a test) there is nothing to mark, and that is not an error.
        let _ = FAILED_OPEN.try_with(|first| {
            if first.get().is_none() {
                first.set(Some(stage));
            }
        });
        tracing::warn!(stage = stage.as_str(), %reason, "forwarding the request as it arrived");
    }

    /// How often each stage has given up.
    pub fn snapshot(&self) -> BTreeMap<&'static str, u64> {
        [
            Stage::Auth,
            Stage::Parse,
            Stage::Walk,
            Stage::Admission,
            Stage::Compress,
            Stage::Discarded,
            Stage::Inject,
            Stage::RetrieveRound,
            Stage::Guard,
            Stage::AfterCompressed,
        ]
        .into_iter()
        .map(|stage| (stage.as_str(), self.count(stage)))
        .collect()
    }

    /// The count for one stage.
    pub fn count(&self, stage: Stage) -> u64 {
        self.counter(stage).load(Ordering::Relaxed)
    }

    fn counter(&self, stage: Stage) -> &AtomicU64 {
        match stage {
            Stage::Auth => &self.auth,
            Stage::Parse => &self.parse,
            Stage::Walk => &self.walk,
            Stage::Admission => &self.admission,
            Stage::Compress => &self.compress,
            Stage::Discarded => &self.discarded,
            Stage::Inject => &self.inject,
            Stage::RetrieveRound => &self.retrieve_round,
            Stage::Guard => &self.guard,
            Stage::AfterCompressed => &self.after_compressed,
        }
    }
}

tokio::task_local! {
    /// The first stage the request running on this task gave up at.
    static FAILED_OPEN: Cell<Option<Stage>>;
}

/// Run one request with a note of where, if anywhere, it gave up, which
/// [`failed_open_here`] reads. Every [`FailOpen::record`] made on the
/// request's own task is noted; one made on a task it spawned is not.
pub(crate) async fn track<F: std::future::Future>(request: F) -> F::Output {
    FAILED_OPEN.scope(Cell::new(None), request).await
}

/// The first stage the current request gave up at, if it has.
pub(crate) fn failed_open_here() -> Option<Stage> {
    FAILED_OPEN.try_with(Cell::get).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_stage_counts_separately_and_starts_at_zero() {
        let counters = FailOpen::default();
        assert!(counters.snapshot().values().all(|count| *count == 0));

        counters.record(Stage::Compress, &"deadline");
        counters.record(Stage::Compress, &"deadline");
        counters.record(Stage::Parse, &"not json");

        assert_eq!(counters.count(Stage::Compress), 2);
        assert_eq!(counters.count(Stage::Parse), 1);
        assert_eq!(counters.count(Stage::Walk), 0);
        assert_eq!(counters.snapshot()["compress"], 2);
        assert_eq!(counters.snapshot()["after_compressed"], 0);
    }

    #[tokio::test]
    async fn a_tracked_request_remembers_the_first_stage_it_gave_up_at() {
        let counters = FailOpen::default();
        let seen = track(async {
            assert_eq!(failed_open_here(), None);
            counters.record(Stage::Walk, &"shape");
            counters.record(Stage::Discarded, &"grew");
            failed_open_here()
        })
        .await;
        assert_eq!(seen, Some(Stage::Walk));
        // Outside any tracked request nothing is remembered, and recording
        // still counts.
        counters.record(Stage::Parse, &"not json");
        assert_eq!(failed_open_here(), None);
        assert_eq!(counters.count(Stage::Parse), 1);
    }
}
