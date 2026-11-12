//! Where the proxy gave up and forwarded the request as it arrived.
//!
//! Fail-open is the proxy's whole contract, which makes it invisible by
//! construction: a deployment whose compression silently stopped working looks
//! exactly like one whose traffic is below the size gate. Counting the give-ups
//! by stage is what tells those two apart, and it is the only reason an
//! operator ever learns that, say, every request is timing out in the engine.

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
}

impl FailOpen {
    /// Record one give-up, with the reason on the log line.
    ///
    /// The reason is `Display`ed rather than stored: the counter answers "how
    /// often", the log answers "why this one", and keeping the last reason in
    /// memory would answer neither well.
    pub fn record(&self, stage: Stage, reason: &dyn std::fmt::Display) {
        self.counter(stage).fetch_add(1, Ordering::Relaxed);
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
        }
    }
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
    }
}
