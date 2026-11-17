//! Prometheus instrumentation, shared by every surface.
//!
//! The numbers here answer one question an operator asks about a compressor
//! that nothing else can: is it still worth running? That is `tokens_in` against
//! `tokens_out` against `retrieved_tokens` — the credit and the debit — beside
//! the two ways this service degrades, which are saturation (the engine could
//! not keep up, so prompts went through uncompressed) and retrieval misses (a
//! handle outlived its fragment, so a model was invited to ask for something
//! that is gone).
//!
//! Counters are emitted through the global recorder, so a process that mounts
//! several surfaces reports one set of series with a `surface` label rather than
//! one registry per surface.

use std::sync::OnceLock;
use std::time::Duration;

use kompressor_core::Retrieved;
use kompressor_engine::messages::CompressStats;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// Buckets for `kompressor_compress_duration_seconds`.
///
/// Weighted toward the low end because the interesting question is what
/// fraction of compressions stayed under a few milliseconds — the engine's
/// budget is well under a millisecond per kilobyte, and a payload that takes a
/// whole second is the tail an operator is hunting.
const DURATION_BUCKETS: &[f64] = &[
    0.000_5, 0.001, 0.002_5, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
];

/// Install the global recorder once and hand back a renderer for `/metrics`.
///
/// Idempotent: the recorder can only be installed once per process, and a
/// process that mounts the API, the tool server and the proxy builds three
/// [`crate::ServerState`]s. They share this one.
pub fn install() -> PrometheusHandle {
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    HANDLE
        .get_or_init(|| {
            let builder = PrometheusBuilder::new()
                .set_buckets(DURATION_BUCKETS)
                .expect("the bucket list is non-empty and sorted");
            match builder.install_recorder() {
                Ok(handle) => {
                    describe();
                    handle
                }
                Err(error) => {
                    // Something else owns the global recorder — an embedding
                    // process with its own exporter. Our own /metrics then
                    // renders an empty registry rather than the process failing
                    // to start over a telemetry detail.
                    tracing::warn!(%error, "another recorder is already installed; /metrics will be empty");
                    PrometheusBuilder::new().build_recorder().handle()
                }
            }
        })
        .clone()
}

fn describe() {
    use metrics::{describe_counter, describe_gauge, describe_histogram};

    describe_counter!(
        "kompressor_compress_requests_total",
        "Compression requests, by surface, mode and outcome"
    );
    describe_counter!(
        "kompressor_tokens_in_total",
        "Estimated tokens presented for compression"
    );
    describe_counter!(
        "kompressor_tokens_out_total",
        "Estimated tokens after compression"
    );
    describe_counter!(
        "kompressor_saved_tokens_total",
        "Estimated tokens removed, gross of anything retrieval puts back"
    );
    describe_histogram!(
        "kompressor_compress_duration_seconds",
        "Wall time of one compression, permit wait included"
    );
    describe_counter!(
        "kompressor_messages_by_kind_total",
        "Messages compressed, by the content type the engine decided they were"
    );
    describe_counter!(
        "kompressor_protected_total",
        "Messages withheld from compression, by why"
    );
    describe_counter!(
        "kompressor_refs_emitted_total",
        "Handles minted for elided fragments"
    );
    describe_counter!(
        "kompressor_ref_bytes_emitted_total",
        "Bytes retained behind those handles"
    );
    describe_counter!(
        "kompressor_retrievals_total",
        "Retrieval attempts, by whether the fragment was there"
    );
    describe_counter!(
        "kompressor_retrieved_tokens_total",
        "Estimated tokens re-injected by retrievals: the debit against saved tokens"
    );
    describe_counter!(
        "kompressor_engine_saturated_total",
        "Requests refused because no engine permit came free in time"
    );
    describe_gauge!("kompressor_refs_live", "Fragments currently retained");
    describe_gauge!("kompressor_refs_bytes_live", "Bytes those fragments occupy");
    describe_gauge!(
        "kompressor_stats_dropped_total",
        "Analytics events dropped by a full or dead queue, or lost to a failed commit; any report from a window with a non-zero value is an undercount"
    );
}

/// Record everything one compression produced.
pub fn compression(surface: &'static str, stats: &CompressStats, elapsed: Duration) {
    let mode = stats.mode.as_str();
    metrics::counter!(
        "kompressor_compress_requests_total",
        "surface" => surface,
        "mode" => mode,
        "outcome" => "ok",
    )
    .increment(1);
    metrics::counter!("kompressor_tokens_in_total").increment(u64::from(stats.tokens_in));
    metrics::counter!("kompressor_tokens_out_total").increment(u64::from(stats.tokens_out));
    // Saturating because a payload the engine declined to shrink comes back
    // byte-identical but can still be re-tokenised a hair longer; a negative
    // saving is not a thing a counter can express, and it is noise either way.
    metrics::counter!("kompressor_saved_tokens_total", "mode" => mode)
        .increment(u64::from(stats.tokens_in.saturating_sub(stats.tokens_out)));
    metrics::histogram!("kompressor_compress_duration_seconds", "surface" => surface)
        .record(elapsed.as_secs_f64());
    for (kind, count) in &stats.by_type {
        metrics::counter!("kompressor_messages_by_kind_total", "kind" => kind.clone())
            .increment(u64::from(*count));
    }
    for (reason, count) in &stats.protected_by_reason {
        metrics::counter!("kompressor_protected_total", "reason" => reason.clone())
            .increment(u64::from(*count));
    }
    metrics::counter!("kompressor_refs_emitted_total").increment(u64::from(stats.refs_emitted));
    metrics::counter!("kompressor_ref_bytes_emitted_total").increment(stats.ref_bytes_emitted);
}

/// Record a compression that did not produce anything.
pub fn compress_failed(surface: &'static str, outcome: &'static str) {
    metrics::counter!(
        "kompressor_compress_requests_total",
        "surface" => surface,
        "mode" => "unknown",
        "outcome" => outcome,
    )
    .increment(1);
}

/// Record a retrieval and what it cost.
pub fn retrieval(surface: &'static str, result: &Retrieved) {
    let outcome = if result.found {
        "hit"
    } else if result.expired {
        "expired"
    } else {
        "miss"
    };
    metrics::counter!(
        "kompressor_retrievals_total",
        "surface" => surface,
        "outcome" => outcome,
        "ranged" => if result.ranged { "true" } else { "false" },
    )
    .increment(1);
    if let Some(content) = &result.content {
        metrics::counter!("kompressor_retrieved_tokens_total")
            .increment(u64::from(kompressor_engine::estimate_tokens(content, "")));
    }
}

/// Record a request refused because the engine was saturated.
pub fn saturated(surface: &'static str) {
    metrics::counter!("kompressor_engine_saturated_total", "surface" => surface).increment(1);
    compress_failed(surface, "saturated");
}

/// Publish the store and analytics gauges.
///
/// Sampled when `/metrics` is scraped rather than kept up to date on the
/// request path: both readings cost a query, and neither changes meaningfully
/// between scrapes.
pub fn sample_gauges(live_refs: u64, live_bytes: u64, stats_dropped: u64) {
    metrics::gauge!("kompressor_refs_live").set(live_refs as f64);
    metrics::gauge!("kompressor_refs_bytes_live").set(live_bytes as f64);
    metrics::gauge!("kompressor_stats_dropped_total").set(stats_dropped as f64);
}
