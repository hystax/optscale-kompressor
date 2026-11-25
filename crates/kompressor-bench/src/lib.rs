//! Replaying a corpus of real agent tool output through the engine.
//!
//! Every number that decides whether a compression change is an improvement
//! comes from here, and nothing here needs a service, a store or a network: it
//! is the engine, a corpus file, and arithmetic. That is deliberate. A metric
//! that needs a deployment to produce gets measured once and then trusted for
//! months.
//!
//! # What is measured, and why it is three numbers rather than one
//!
//! * [`ReplayReport::coverage`] — the share of *eligible* bytes removed. The
//!   headline. Eligible means "at or above `min_compress_chars`": a payload
//!   below the gate was never offered to a compressor, so counting it as a
//!   failure to compress measures the gate, not the compressors.
//! * [`ReplayReport::claimed_pct`] — the share of eligible bytes some
//!   compressor took at any ratio. The gap between claimed and coverage is the
//!   corpus still passing through untouched, and that gap is where the
//!   remaining work is: a claimed-but-barely-compressed bucket is a compressor
//!   to improve, an unclaimed bucket is a compressor that does not exist yet.
//! * [`ReplayReport::billed`] — coverage weighted by what the payload actually
//!   costs over a conversation. An agent re-sends its history every turn, so a
//!   payload is fresh input once and a prompt-cache read on every later
//!   request, at roughly a tenth of the input rate. Counting requests flat
//!   would overstate an early payload by an order of magnitude — one carried
//!   261 times costs about 27 first-sends, not 261.
//!
//! # Safety is not a metric
//!
//! The [`Violation`]s in [`PayloadResult::violations`] are not traded against
//! ratio at any exchange rate. A run that improves coverage and breaks one of
//! them is a failed run, which is why [`compare_baseline`] fails on a violation
//! regardless of the numbers.
//!
//! # Layout
//!
//! [`corpus`] loads payloads, [`mod@replay`] runs them and checks the invariants,
//! [`metrics`] aggregates, and [`baseline`] records a run and compares two.

#![forbid(unsafe_code)]

pub mod baseline;
pub mod conversation;
pub mod corpus;
pub mod economics;
pub mod metrics;
pub mod replay;
pub mod report;

pub use baseline::{
    BASELINE_FORMAT, Baseline, BucketBaseline, Comparison, MembershipChange, MetricDelta,
    Tolerance, compare_baseline, load_baseline, save_baseline,
};
pub use conversation::{
    COMMITTED_CONVERSATIONS, CONVERSATION_VERSION, Conversation, ConversationResult, Turn,
    load_conversations, replay_conversations, write_conversations,
};
pub use corpus::{
    BenchError, COMMITTED_CORPUS_PAYLOADS, CORPUS_VERSION, Payload, load_corpus, write_corpus,
};
pub use metrics::{Billed, BucketStats, CACHE_READ_RATE, Coverage, Timing, Violation};
pub use replay::{PayloadResult, ReplayReport, replay, warm_grammars};
