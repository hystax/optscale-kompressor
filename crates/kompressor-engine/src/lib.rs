//! Deterministic prompt compression.
//!
//! Agent conversations are dominated by machine-generated context: tool
//! results, API responses, log excerpts, directory listings, source files.
//! These payloads are highly redundant and routinely far larger than the
//! information the model actually needs. This crate rewrites them into a
//! shorter view that says explicitly what it removed, and — in
//! [`Mode::Recoverable`] — retains every removed fragment behind a handle the
//! model can ask for by name.
//!
//! # The invariants
//!
//! Four properties hold for every payload, in every mode, and none of them is
//! tradeable against compression ratio:
//!
//! 1. **Never larger.** A compressor that fails to shrink a payload yields the
//!    original, byte for byte.
//! 2. **Every shrink is accounted for.** Output that is shorter carries an
//!    omission marker, or is provably layout-only (and then says so), or
//!    declares a reversible envelope. Anything else is refused.
//! 3. **Deterministic.** The same input compresses to the same bytes, run after
//!    run and turn after turn. Most billed input on agent traffic is a prompt
//!    cache read, so byte instability costs more than any ratio wins.
//! 4. **Fail open.** A compressor that errors, panics or declines returns the
//!    input untouched. Compression subtracts tokens, never availability.
//!
//! # Layout
//!
//! [`detect`] classifies a payload; [`pipeline`] routes it, arbitrates between
//! the readings that compete for it, and enforces the invariants centrally so
//! no compressor can opt out. The compressors themselves are independent and
//! pure.

#![forbid(unsafe_code)]

/// Whether this build has the tree-sitter grammars `enable_code_aware` needs.
/// Without them the setting is accepted and code is compressed as text.
pub const CODE_AWARE: bool = cfg!(feature = "code-aware");

pub mod adaptive;
pub mod code;
pub mod configs;
pub mod crossturn;
pub mod crusher;
pub mod detect;
pub mod diff;
pub mod echo;
pub mod gate;
pub mod gutter;
pub mod listing;
pub mod logs;
pub mod longline;
pub mod markers;
pub mod messages;
pub mod mixed;
pub mod options;
pub mod pipeline;
pub mod preamble;
pub mod read_lifecycle;
pub mod redraw;
pub mod refs;
pub mod search;
pub mod tabular;
pub mod text;
pub mod tokens;
pub mod traces;
pub mod types;

pub use options::{EngineOptions, EngineOptionsPatch, TokenGuard};
pub use refs::{RefCollector, RefId, ref_for, refs_in};
pub use tokens::estimate_tokens;
pub use types::{
    ConfigFlavor, ContentType, Context, Detection, DetectionMeta, ForcedType, Kind, Language, Mode,
    Strategy,
};
