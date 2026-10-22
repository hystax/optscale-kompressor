//! Conversation-level policy: what may be compressed at all, and the accounting
//! that comes back.
//!
//! The engine itself is role-blind — it sees text and decides what that text is.
//! Which *messages* are eligible is decided here, once, so every surface (a
//! direct API call, a tool call, a proxied completion) enforces the same rules
//! rather than each re-deriving them.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::crossturn::SeenText;
use crate::detect::detect_result;
use crate::markers::stale_read_marker;
use crate::options::EngineOptions;
use crate::pipeline::compress_content;
use crate::read_lifecycle::{ReadMessage, ReadState, classify_reads};
use crate::refs::{RefCollector, RefId, refs_in};
use crate::text::chars_len;
use crate::tokens::estimate_tokens;
use crate::types::{Context, Kind, Mode};

/// One independently compressible run of text inside a message.
///
/// Block-structured requests (an Anthropic `tool_result`, an OpenAI content
/// part) put several texts in one message, and some of them must not be touched
/// at all — a block carrying a cache-control marker is a prompt-cache breakpoint
/// whose bytes are load-bearing. Modelling slots rather than one string per
/// message is what lets block content be compressed without flattening it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextSlot {
    pub text: String,
    /// Set when the surrounding structure says these exact bytes must survive.
    #[serde(default)]
    pub protected: bool,
    /// *Why*, when it is protected.
    ///
    /// A bool could not answer the question that matters once a caller can
    /// tell a live provider cache from a lapsed one: a cache breakpoint is
    /// load-bearing because its bytes are in a cache key, and a cache key that
    /// no longer exists protects nothing. A signature or a citation anchor is
    /// load-bearing whatever any cache is doing.
    ///
    /// Added beside `protected` rather than replacing it, so the serialized
    /// shape stays compatible — this type crosses the MCP wire — and so an
    /// older payload deserializes to [`Protection::Always`], which is the
    /// answer that cannot be wrong.
    #[serde(default)]
    pub protection: Protection,
}

/// Why a slot's exact bytes must survive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protection {
    /// Load-bearing regardless: a signature a rewrite invalidates, a citation
    /// whose offsets point into this exact text. The safe default, and what an
    /// unlabelled slot means.
    #[default]
    Always,
    /// Load-bearing only while the provider is serving this prefix from cache.
    /// A `cache_control` breakpoint: its bytes are in the cache key, and once
    /// [`PrefixCache::Cold`] says that cache is gone there is nothing left to
    /// protect.
    WhileCached,
}

impl TextSlot {
    pub fn new(text: impl Into<String>) -> Self {
        TextSlot {
            text: text.into(),
            protected: false,
            protection: Protection::Always,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    /// A plain string body.
    Text(String),
    /// Several text runs, compressed independently and written back in place.
    Blocks(Vec<TextSlot>),
    /// Nothing this engine can read: an image, audio, a structured tool result.
    Opaque,
}

impl MessageContent {
    /// Total characters across everything readable.
    pub fn text_len(&self) -> usize {
        match self {
            MessageContent::Text(t) => chars_len(t),
            MessageContent::Blocks(slots) => slots.iter().map(|s| chars_len(&s.text)).sum(),
            MessageContent::Opaque => 0,
        }
    }

    /// Every text this message carries, joined — what a later message can
    /// point back at. Blocks are joined with a newline so a run cannot match
    /// across a boundary that the model sees as a break.
    pub fn joined_text(&self) -> String {
        match self {
            MessageContent::Text(t) => t.clone(),
            MessageContent::Blocks(slots) => slots
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            MessageContent::Opaque => String::new(),
        }
    }

    pub fn is_opaque(&self) -> bool {
        matches!(self, MessageContent::Opaque)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageMeta {
    /// The caller marked this message as a prompt-cache breakpoint.
    #[serde(default)]
    pub cache_control: bool,
    /// The caller marked this message as not to be touched.
    #[serde(default)]
    pub protected: bool,
    /// The tool that produced this message, when it is a tool result.
    #[serde(default)]
    pub tool_name: Option<String>,
    /// The file the tool acted on, when it names one.
    #[serde(default)]
    pub file_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: MessageContent,
    #[serde(default)]
    pub meta: MessageMeta,
}

impl Message {
    pub fn text(role: impl Into<String>, text: impl Into<String>) -> Self {
        Message {
            role: role.into(),
            content: MessageContent::Text(text.into()),
            meta: MessageMeta::default(),
        }
    }
}

/// Whether the provider's prompt-cache prefix is still alive.
///
/// The frozen prefix exists for one reason: rewriting a byte inside it costs
/// the cache read for everything after that byte. When the cache has already
/// lapsed there is nothing left to protect — the provider will re-read the
/// whole prompt at full price either way — and the bytes the prefix was holding
/// back become the cheapest tokens in the request to remove.
///
/// This is the one place the engine acts on cache *state* rather than assuming
/// the worst. It assumes the worst by default, because a caller that guesses
/// `Cold` for a warm cache pays for its mistake on every message after the
/// first rewritten byte, while a caller that guesses `Warm` for a cold cache
/// merely leaves a saving on the table.
///
/// Deciding it is the caller's job, because only the caller knows: the prefix
/// is cold when the gap since the last turn on this conversation exceeds the
/// provider's cache TTL (five minutes on Anthropic's default, an hour on the
/// extended one), or when the reported cache-read token count on the previous
/// response was zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PrefixCache {
    /// Assume the prefix is cached and must not move. The safe answer, and the
    /// only one a caller that cannot tell should use.
    #[default]
    Warm,
    /// The prefix has lapsed, so nothing is gained by preserving it.
    Cold,
}

/// Per-request knobs that are not engine options.
#[derive(Clone, Debug)]
pub struct RequestOptions<'a> {
    /// Messages `[0, frozen_message_count)` are the provider-cached prefix and
    /// must come back byte-identical — unless [`Self::prefix_cache`] says that
    /// cache is already gone.
    pub frozen_message_count: usize,
    /// The latest user text, carried for compressors that want conversation
    /// context. Nothing reads it in a way that changes output bytes.
    pub context: &'a str,
    /// Whether the provider-cached prefix is still live.
    pub prefix_cache: PrefixCache,
    /// Handles whose content the model has already fetched back in this
    /// conversation, and which must therefore stop being elided.
    ///
    /// Eliding a fragment is a bet that nobody will ask for it. A retrieval
    /// settles that bet, and settles it badly: the round that answers it
    /// re-sends the entire prompt upstream, so one retrieval of a 237-token
    /// fragment cost ~90,000 prompt tokens on a measured run, and the same
    /// three handles were fetched on three consecutive turns because the
    /// fetched bytes live only in that one hidden round and never reach the
    /// client's history. Keeping the content inline from then on ends the loop.
    ///
    /// **This deliberately breaks prefix-monotonicity, once per fragment.** A
    /// message elided on turn five comes back whole on turn six, so the
    /// provider re-reads from that message on. That is a real cost and it is
    /// taken knowingly: it is paid once, against a round that was being paid
    /// every turn the content was wanted.
    pub keep_inline: &'a [RefId],
    /// How many distinct requests have fetched each handle in `keep_inline`.
    ///
    /// Empty means "not known", and every candidate is then priced at one
    /// demand — the conservative reading, since one fetch rarely justifies
    /// rewriting a prefix.
    pub fetches: &'a [(RefId, u32)],
    /// What share of billed prompt tokens the provider has been serving from
    /// cache, in parts per thousand.
    ///
    /// Sets what a retrieve round costs relative to a rewritten prefix, so it
    /// sets how many demands a promotion needs. 1000 — the default, and what a
    /// caller with no measurement should send — makes a round look as cheap as
    /// it can be and so demands the most before rewriting anything.
    pub cache_hit_per_mille: u32,
}

impl Default for RequestOptions<'_> {
    /// Hand-written for one field. `#[derive(Default)]` would make
    /// `cache_hit_per_mille` zero, which does not mean "unknown" — it means
    /// "nothing is ever cached", and that is the reading under which a retrieve
    /// round is most expensive and a prefix rewrite cheapest. The derive would
    /// therefore promote on a single demand, which is the opposite of what an
    /// absent measurement should do and the exact mistake that cost 46,379
    /// tokens of cache in one turn.
    fn default() -> Self {
        RequestOptions {
            frozen_message_count: 0,
            context: "",
            prefix_cache: PrefixCache::default(),
            keep_inline: &[],
            fetches: &[],
            cache_hit_per_mille: 1000,
        }
    }
}

impl RequestOptions<'_> {
    /// How many messages must come back byte-identical.
    ///
    /// Zero on a cold prefix however many the caller froze: the bytes it was
    /// protecting are not in any cache to be busted.
    pub fn effective_frozen(&self) -> usize {
        match self.prefix_cache {
            PrefixCache::Warm => self.frozen_message_count,
            PrefixCache::Cold => 0,
        }
    }
}

/// Why a run removed nothing.
///
/// Ported from headroom's `uncompressed_requests` taxonomy after a wrapped
/// Claude Code session removed 0 of 178,665 characters and *both* tools
/// reported it as a healthy request. Headroom counted it as `prefix_frozen`;
/// kompressor recorded a 0 and a clean validity line, which is the same fact
/// told in a way nobody can act on. A run that removes nothing is not a
/// failure, but "nothing was eligible" and "everything was eligible and nothing
/// was worth taking" are different problems with different fixes, and the
/// number alone cannot tell them apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Something was removed.
    Compressed,
    /// Every message sat inside the provider-cached prefix.
    ///
    /// The expensive one to misread. A client that puts a `cache_control`
    /// breakpoint on its final message — Claude Code does — pins the whole
    /// request, and no compressor that honours the cache can touch a byte of
    /// it. Nothing is wrong; nothing can be done either, short of trading the
    /// cache away, which at a tenth the read rate needs a 90% reduction just to
    /// break even.
    PrefixFrozen,
    /// Every message was withheld for some other protection: a protected role,
    /// or a `cache_control`/`protected` flag on the block itself.
    AllProtected,
    /// The caller shrank the messages and threw the result away.
    ///
    /// Set by the proxy, not the engine: the body it serialized came out larger
    /// than the one it was given, so the original was forwarded. The engine's
    /// own numbers stay on the row; this says they never left the machine.
    Discarded,
    /// Messages were eligible and no compressor took anything.
    ///
    /// The one worth chasing: the content reached the engine and the engine
    /// had nothing for it.
    NoCompressibleContent,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Compressed => "compressed",
            Outcome::PrefixFrozen => "prefix_frozen",
            Outcome::AllProtected => "all_protected",
            Outcome::Discarded => "discarded",
            Outcome::NoCompressibleContent => "no_compressible_content",
        }
    }
}

/// How much compression may be forfeited to keep one fetched fragment inline,
/// as a multiple of that fragment's own size.
///
/// Keeping a fetched fragment inline costs the *whole message's* compression on
/// every later turn, because the rollback is message-granular while the loop is
/// fragment-granular. That is usually a good trade — measured on a 48-request
/// run, three fragments of 700 tokens held two messages inline at ~3,960
/// tokens a turn against retrieve rounds costing ~20,000 a turn — but it stops
/// being one when the message is enormous and the fragment is tiny: inlining a
/// 50,000-token log to protect 233 tokens adds more to every prompt than the
/// round it avoids.
///
/// **This number is derived, and it used to be guessed.** It was 32, chosen to
/// sit between an observed healthy ratio of ~8.5 and a pathological ~215, with
/// a note saying the engine could not do better locally because "the real
/// denominator is what a retrieve round costs, which is the whole prompt, and
/// the engine never sees it". That reasoning was sound and the conclusion was
/// avoidable: the denominator is not the prompt, it is the *rate the prompt is
/// billed at*, and that is one number.
///
/// Keeping a fragment inline pays `size` every turn, of which the first is a
/// fresh read and the rest are cache reads. Re-fetching pays `fetched` every
/// turn at the fresh rate. So the two cross at
///
/// ```text
/// size x cached_rate  ==  fetched x fresh_rate
/// size / fetched      ==  fresh_rate / cached_rate   ==  the cache discount
/// ```
///
/// The crossover ratio *is* the cache discount, whatever it happens to be.
/// Solved by least squares against seven live terminal-bench runs — cost,
/// fresh, cached and output tokens, three unknowns, seven observations — the
/// provider's rates come out to $5.00, $0.50 and $30.00 per MTok with a fit
/// error of 0.0% on every run. A discount of exactly 10, so the crossover is
/// 10 and this constant is 10.
///
/// Every ratio measured in the field sits under it: 8.5 on the original
/// observed case, and 3.0, 2.4 and 3.4 on three later runs that kept one, two
/// and three messages inline. So the bound admits everything seen to work and
/// refuses only what does not pay — which is what it should have been doing at
/// 32, where it would have approved decisions losing money threefold.
///
/// **One assumption, and it favours keeping inline.** The crossover above
/// assumes the fragment would otherwise be re-fetched on *every* remaining
/// turn. A fragment wanted only twice more costs far less to re-fetch than to
/// hold, and this ratio would still wave it through. That frequency is
/// unobservable by construction — once a fragment is promoted, nothing ever
/// reveals whether it was wanted again — so 10 is a ceiling on the generous
/// reading, not a break-even on the true one.
const KEEP_INLINE_MAX_FORFEIT: u32 = 10;

/// The provider's cache discount: what a fresh prompt token costs as a multiple
/// of a cached one.
///
/// Solved by least squares against seven live runs at $5.00 fresh and $0.50
/// cached per MTok, and confirmed on four Anthropic runs at the same two
/// numbers (§12.6). The figure [`KEEP_INLINE_MAX_FORFEIT`] is also set from.
const CACHE_DISCOUNT: u32 = 10;

/// How many future fetches a promotion at this position has to save to pay.
///
/// A promotion rewrites the prompt at one message, so the provider re-reads
/// everything behind it at the fresh rate instead of the cached one. A retrieve
/// round re-sends the *whole* prompt at the blended rate — mostly cached, but
/// paying fresh for whatever is not. Both are "the prompt", so its size cancels
/// and only the rates remain:
///
/// ```text
/// break   = behind × (fresh − cached)
/// a round = whole  × (hit × cached + (1 − hit) × fresh)
/// promote once   fetches ≥ (behind / whole) × (fresh − cached) / blended
/// ```
///
/// **The blend is measured, not assumed.** Writing that denominator as `cached`
/// — treating a round as entirely cache reads — is tidy and wrong, and it is
/// what the first version of this shipped: it put the multiple at 9 where the
/// traffic made it 6.7, so the rule asked for 35% more demands than the
/// economics required and refused 72 promotions in one run while the fragment
/// it was protecting was fetched five times. The blend moves with the hit rate
/// — 5.0 at 91.3%, 6.7 at 96.1%, 7.3 at 97.5% — which is why this is computed
/// from what the provider has actually reported rather than being a constant
/// that is right for one run.
///
/// `hit_per_mille` of 1000 recovers the old value of 9 and is the default where
/// nothing has been observed: it is the most demanding reading, and erring
/// towards refusing is the cheap mistake. Anthropic bills the rewrite as well,
/// which puts its true figure above this, so there too the bias is to refuse.
fn fetches_to_repay_a_break(behind: u32, whole: u32, hit_per_mille: u32) -> u64 {
    if whole == 0 {
        return 0;
    }
    let hit = u64::from(hit_per_mille.min(1000));
    // blended / cached, in per-mille: hit + (1 − hit) × discount.
    let blended = hit + (1000 - hit) * u64::from(CACHE_DISCOUNT);
    let numerator = u64::from(CACHE_DISCOUNT - 1) * 1000 * u64::from(behind.min(whole));
    numerator / (blended * u64::from(whole))
}

/// Is shipping this message whole worth it, to stop re-eliding a fragment the
/// model has already fetched back?
///
/// Two costs, and they are different shapes. `forfeited` is the compression
/// given up on *every later turn*, weighed against `protected`, the fetched
/// fragment it saves — that is [`KEEP_INLINE_MAX_FORFEIT`]. `break_cost` is the
/// prompt behind this message, re-read *once* because promoting rewrites the
/// prefix there — and what repays that is how often the fragment would
/// otherwise be fetched again, which is `fetches`.
///
/// **Size is not frequency, and the rule used to confuse them.** Requiring
/// `break_cost ≤ protected × 8` compared a prompt against a fragment and let
/// the answer depend on how big the fragment happened to be: for the ~300-token
/// fragments these runs produce it permitted about one round's worth of break
/// while claiming eight, and for a 4,500-token one it permitted twelve.
/// Measured 2026-10-08, that rule refused seventeen promotions on one OpenAI
/// run whose handles were then fetched four, four, three and two more times —
/// nine avoidable rounds, $0.35, against $0.25 to promote.
fn keeping_it_inline_is_worth_it(
    forfeited: u32,
    protected: u32,
    break_cost: u32,
    whole: u32,
    fetches: u32,
    hit_per_mille: u32,
) -> bool {
    if protected == 0 {
        return false;
    }
    if forfeited > protected.saturating_mul(KEEP_INLINE_MAX_FORFEIT) {
        return false;
    }
    // Nothing behind this message, so no cached byte moves and there is nothing
    // to repay. The live tail is the common case and must stay free, or nothing
    // would ever be promotable.
    if break_cost == 0 || whole == 0 {
        return true;
    }
    // Worth it once the fragment has been asked for as many times as the
    // rewrite costs rounds.
    u64::from(fetches) >= fetches_to_repay_a_break(break_cost, whole, hit_per_mille)
}

/// What one compression run did.
#[derive(Clone, Debug, Default, Serialize)]
pub struct CompressStats {
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub ratio: f32,
    pub chars_in: u64,
    pub chars_out: u64,
    pub compressed_messages: u32,
    pub skipped_messages: u32,
    pub by_type: BTreeMap<String, u32>,
    pub mode: Mode,
    pub refs_emitted: u32,
    pub ref_bytes_emitted: u64,
    /// Why a message was withheld: `role:assistant`, `frozen`, `flagged`.
    pub protected_by_reason: BTreeMap<String, u32>,
    /// Why each skipped message was skipped, over *every* skip path.
    ///
    /// `skipped_messages` has always been a bare count, and on agent traffic it
    /// is the majority: one Harbor run skipped 6,368 of 8,928 messages and
    /// nothing recorded why. That number is where the remaining headroom is, so
    /// leaving it unattributed means the most useful question about a run —
    /// "what would it take to compress the rest?" — cannot be asked of the data.
    ///
    /// `protected_by_reason` answered part of it and only part: it covers the
    /// messages protection withheld, not the ones that reached the compressors
    /// and were declined, nor the opaque ones that never had readable text.
    /// Those two were invisible, and the declines are the interesting half —
    /// on a git workload every diff is declined by the §7.6 gates and the run
    /// reported `diff: 0`, indistinguishable from "no diffs present", because
    /// `by_type` only counts successes.
    ///
    /// The keys are a closed set, so they can be counted on:
    ///
    /// * `opaque` — no text this engine can read.
    /// * `protected:<reason>` — the `protected_by_reason` key, prefixed, so the
    ///   two maps reconcile against `skipped_messages` without double counting.
    /// * `below_min_chars` — shorter than `min_compress_chars`; the engine
    ///   never looked at it.
    /// * `declined:<type>` — the compressors ran on content detected as
    ///   `<type>` and none of them shrank it. This is the key that answers the
    ///   diff question.
    /// * `retrieved_before` — it compressed, and the compression was thrown
    ///   away because it would have re-elided a fragment the model already
    ///   fetched back. Not a decline: the engine could have taken these tokens
    ///   and chose not to, because a retrieve round costs far more than they
    ///   are worth.
    pub skipped_by_reason: BTreeMap<String, u32>,
    /// The same partition, weighted by tokens instead of messages.
    ///
    /// The count map alone cannot rank anything, and ranking is the whole
    /// point of attributing skips. Measured over 16 Harbor runs, 32,082
    /// skipped messages: `below_min_chars` is 20.9% of them and tiny by
    /// construction, while `opaque` is 17.9% and could be enormous. Those two
    /// read identically in a count and could hardly differ more in what they
    /// are worth pursuing.
    ///
    /// **`opaque` is necessarily 0 here, and that is not a measurement.** The
    /// engine holds no text for an opaque message — an image, a structured
    /// tool result, an assistant turn of `tool_use` blocks — so it cannot say
    /// how many tokens the provider billed for it. The honest reading of an
    /// `opaque` row is "unknown, and invisible from here"; sizing it needs
    /// instrumentation at the wire layer, where the serialized bytes exist.
    /// Every other key is a real estimate over text the engine did hold.
    pub skipped_tokens_by_reason: BTreeMap<String, u32>,
    /// Handles found inside content this request declined to compress.
    ///
    /// A handle can never be *created* in protected content, because protection
    /// short-circuits compression. It gets there by drift: a tool result
    /// compressed on an early turn keeps its marker verbatim, and later the
    /// frozen prefix grows past that message, or the model quotes the marker
    /// into its own turn. From then on nothing re-stashes the fragment, so it
    /// ages out while the marker lives on, inviting the model to ask for
    /// something that is gone. Reporting them lets the caller refresh their
    /// retention instead.
    #[serde(skip_serializing)]
    pub protected_refs: Vec<RefId>,
    /// Fragments this run shipped whole rather than eliding, because the model
    /// had fetched them back often enough to pay for the prefix rewrite.
    ///
    /// Reported so the caller can **pin** them. The decision is priced against
    /// a bar that moves — the prompt grows behind the message, and the measured
    /// cache hit rate converges — so re-taking it every turn lets a fragment be
    /// promoted on one turn and demoted on the next, which rewrites the prefix
    /// a second time and loses the cache again. Measured 2026-10-08: a message
    /// promoted while the running hit rate read 82% was re-elided once it
    /// reached 94%, for 8,192 tokens. A promotion is paid for once and must
    /// therefore be taken once.
    #[serde(skip_serializing)]
    pub kept_inline_refs: Vec<RefId>,
    pub protected_ref_reasons: BTreeSet<String>,
    pub protected_ref_messages: u32,
    pub router_version: &'static str,

    /// How many of the skipped messages were skipped because they sat inside
    /// the frozen prefix, as opposed to any other protection.
    pub frozen_messages: u32,
}

/// Why the compressors left a message alone.
///
/// The size gate is answered without detecting anything: below
/// `min_compress_chars` nothing ran, so naming a type would describe a
/// classification the engine never performed. Above it, the content is
/// detected *again* — the pipeline consumed its own answer and returns only
/// `Option<Kind>`, which is `None` for every decline regardless of what the
/// payload was. Widening that return would touch every caller of
/// `compress_content`; paying one more detection pass on the declines alone is
/// the cheaper trade, and declines are exactly the messages where the
/// compressors already did their work and stopped.
///
/// Blocks report the type of their largest slot: a message is one row in the
/// breakdown, and the biggest piece is the one worth naming.
fn decline_reason(content: &MessageContent, role: &str, opts: &EngineOptions) -> String {
    let text = match content {
        MessageContent::Text(text) => text.as_str(),
        MessageContent::Blocks(slots) => slots
            .iter()
            .filter(|slot| !slot.protected)
            .max_by_key(|slot| slot.text.len())
            .map(|slot| slot.text.as_str())
            .unwrap_or(""),
        MessageContent::Opaque => return "opaque".to_string(),
    };
    if chars_len(text) < opts.min_compress_chars {
        return "below_min_chars".to_string();
    }
    format!(
        "declined:{}",
        detect_result(text, role, opts).content_type.as_str()
    )
}

impl CompressStats {
    /// Record why one message was skipped, and how much it was worth.
    ///
    /// `tokens` is 0 for an opaque message because the engine has no text to
    /// measure, not because the message was empty — see
    /// [`CompressStats::skipped_tokens_by_reason`].
    fn skip(&mut self, reason: &str, tokens: u32) {
        *self
            .skipped_by_reason
            .entry(reason.to_string())
            .or_insert(0) += 1;
        *self
            .skipped_tokens_by_reason
            .entry(reason.to_string())
            .or_insert(0) += tokens;
    }

    /// Name what this run did, so a zero can be acted on.
    pub fn outcome(&self) -> Outcome {
        if self.compressed_messages > 0 {
            return Outcome::Compressed;
        }
        let protected: u32 = self.protected_by_reason.values().sum();
        if self.frozen_messages > 0 && self.frozen_messages == self.skipped_messages {
            return Outcome::PrefixFrozen;
        }
        if protected > 0 && protected == self.skipped_messages {
            return Outcome::AllProtected;
        }
        Outcome::NoCompressibleContent
    }

    fn finish(mut self, mode: Mode, refs: &RefCollector) -> Self {
        self.mode = mode;
        self.refs_emitted = refs.len() as u32;
        self.ref_bytes_emitted = refs.bytes();
        self.ratio = if self.tokens_in > 0 {
            self.tokens_out as f32 / self.tokens_in as f32
        } else {
            1.0
        };
        self.router_version = "algorithmic-v2-rs";
        self
    }
}

/// Why a message was not compressed.
fn protection_for(
    index: usize,
    message: &Message,
    opts: &EngineOptions,
    req: &RequestOptions,
) -> Option<String> {
    if index < req.effective_frozen() {
        return Some("frozen".to_string());
    }
    // An operator's own flag, which no cache state changes.
    if message.meta.protected {
        return Some("flagged".to_string());
    }
    // A cache breakpoint on the message is load-bearing for exactly the reason
    // one on a block is — its bytes are in a cache key — and stops being so
    // for exactly the same reason. `cache_control` protects at three levels:
    // the frozen count (`effective_frozen`), this flag, and `TextSlot`. All
    // three had to learn the same distinction, and lifting any two of them
    // changed nothing measurable, because the third still held.
    if message.meta.cache_control && req.prefix_cache == PrefixCache::Warm {
        return Some("flagged".to_string());
    }
    if opts.is_protected_role(&message.role) {
        // The model's own analysis and plan turns are the agent's working
        // memory: clipping them makes it re-investigate every turn, which costs
        // far more than compression saves. System and developer turns carry the
        // instructions.
        return Some(format!("role:{}", message.role.trim().to_ascii_lowercase()));
    }
    None
}

/// Compress a conversation in place.
pub fn compress_messages(
    messages: &mut [Message],
    opts: &EngineOptions,
    req: &RequestOptions,
    refs: &mut RefCollector,
) -> CompressStats {
    let mut stats = CompressStats::default();
    let mut protected_ref_ids: BTreeSet<RefId> = BTreeSet::new();
    let mut kept_inline_ids: BTreeSet<RefId> = BTreeSet::new();

    let read_states = if opts.enable_stale_read {
        let view: Vec<ReadMessage> = messages
            .iter()
            .map(|m| ReadMessage {
                tool_name: m.meta.tool_name.as_deref(),
                path: m.meta.file_path.as_deref(),
                protected: m.meta.cache_control || m.meta.protected,
            })
            .collect();
        classify_reads(&view, req.effective_frozen())
    } else {
        BTreeMap::new()
    };

    let mut preceding_user_text = req.context.to_string();
    // Every message's *emitted* text, in order, for the cross-message fold.
    // Emitted rather than received: a pointer may only name bytes the model
    // will actually see.
    let mut seen = SeenText::default();

    #[allow(clippy::needless_range_loop)]
    for index in 0..messages.len() {
        // indexed rather than iterated: protection depends on the message's
        // position in the conversation, not just its contents
        let role = messages[index].role.clone();
        if messages[index].content.is_opaque() {
            stats.skipped_messages += 1;
            stats.skip("opaque", 0);
            seen.push("");
            continue;
        }

        let chars_before = messages[index].content.text_len() as u64;
        stats.chars_in += chars_before;
        stats.tokens_in += count_tokens(&messages[index].content, &role);

        if role.eq_ignore_ascii_case("user")
            && let MessageContent::Text(text) = &messages[index].content
        {
            preceding_user_text = text.clone();
        }

        if let Some(reason) = protection_for(index, &messages[index], opts, req) {
            // Taken before `reason` is moved into `protected_ref_reasons`.
            let skipped_as = format!("protected:{reason}");
            if reason == "frozen" {
                stats.frozen_messages += 1;
            }
            *stats.protected_by_reason.entry(reason.clone()).or_insert(0) += 1;
            let mut carried = false;
            for text in texts_of(&messages[index].content) {
                let found = refs_in(text);
                if !found.is_empty() {
                    carried = true;
                    protected_ref_ids.extend(found);
                }
            }
            if carried {
                stats.protected_ref_reasons.insert(reason);
                stats.protected_ref_messages += 1;
            }
            stats.skipped_messages += 1;
            stats.skip(&skipped_as, count_tokens(&messages[index].content, &role));
            stats.tokens_out += count_tokens(&messages[index].content, &role);
            stats.chars_out += chars_before;
            seen.push(&messages[index].content.joined_text());
            continue;
        }

        // A file read a later edit invalidated is factually wrong, and the model
        // cannot tell. Replacing it is worth more than compressing it.
        let stale = matches!(read_states.get(&index), Some(ReadState::Stale));
        let mut compressed_here = false;
        let mut applied: Vec<Kind> = Vec::new();

        // Only cloned when there is something to check against, because the
        // overwhelming majority of requests have retrieved nothing.
        let inline_mark = refs.mark();
        let before_compression =
            (!req.keep_inline.is_empty()).then(|| messages[index].content.clone());

        if stale
            && refs.is_enabled()
            && let MessageContent::Text(text) = &messages[index].content
            && text.len() <= refs.max_ref_bytes()
        {
            let mark = refs.mark();
            let path = messages[index]
                .meta
                .file_path
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            let marker = stale_read_marker(&path, text, refs);
            if chars_len(&marker) < chars_len(text) {
                messages[index].content = MessageContent::Text(marker);
                applied.push(Kind::StaleRead);
                compressed_here = true;
            } else {
                refs.rollback(mark);
            }
        }

        if !compressed_here {
            let ctx_index = index;
            match &mut messages[index].content {
                MessageContent::Text(text) => {
                    let ctx = Context {
                        role: &role,
                        message_index: Some(ctx_index),
                        preceding_user_text: &preceding_user_text,
                        source_tool: None,
                        file_path: None,
                    };
                    let (out, kind) =
                        compress_content(text, &role, opts, None, refs, &ctx, &mut applied);
                    if let Some(kind) = kind {
                        let out = out.into_owned();
                        *text = out;
                        if applied.is_empty() {
                            applied.push(kind);
                        }
                        compressed_here = true;
                    }
                }
                MessageContent::Blocks(slots) => {
                    for slot in slots.iter_mut() {
                        // A slot protected only by a cache breakpoint stops
                        // being protected when the cache it names is gone.
                        // Everything else stays untouchable: a signature a
                        // rewrite invalidates, a citation whose offsets point
                        // into these exact bytes.
                        //
                        // This is the second half of `PrefixCache::Cold`.
                        // Lifting `effective_frozen` alone changed nothing on
                        // Anthropic, because `cache_control` both puts a
                        // message in the frozen count *and* pins its block —
                        // so the message freeze lifted and the slot stayed
                        // shut.
                        let lifted = slot.protection == Protection::WhileCached
                            && req.prefix_cache == PrefixCache::Cold;
                        if slot.protected && !lifted {
                            continue;
                        }
                        let ctx = Context {
                            role: &role,
                            message_index: Some(ctx_index),
                            preceding_user_text: &preceding_user_text,
                            source_tool: None,
                            file_path: None,
                        };
                        let mut slot_applied = Vec::new();
                        let (out, kind) = compress_content(
                            &slot.text,
                            &role,
                            opts,
                            None,
                            refs,
                            &ctx,
                            &mut slot_applied,
                        );
                        if let Some(kind) = kind {
                            slot.text = out.into_owned();
                            if slot_applied.is_empty() {
                                slot_applied.push(kind);
                            }
                            applied.extend(slot_applied);
                            compressed_here = true;
                        }
                    }
                }
                MessageContent::Opaque => {}
            }
        }

        // Last, on what the message actually ships: the pointer has to name
        // bytes an earlier message emitted, not bytes it arrived with.
        if opts.enable_cross_turn_dedup
            && !seen.is_empty()
            && let MessageContent::Text(text) = &mut messages[index].content
            && let Some(folded) = seen.fold(text, opts)
        {
            *text = folded;
            applied.push(Kind::CrossTurn);
            compressed_here = true;
        }
        // A compression that would re-elide something already fetched back is
        // thrown away whole. Placed after cross-turn and before `seen.push` on
        // purpose: `seen` must hold the bytes this message really ships, or a
        // later message folds against text that is not there.
        let mut kept_inline = false;
        if let Some(original) = before_compression {
            // Only the fragments this message would re-elide, and only the ones
            // already fetched back.
            let protected: u32 = refs.pairs()[inline_mark.pairs..]
                .iter()
                .filter(|(id, _)| req.keep_inline.contains(id))
                .map(|(_, fragment)| estimate_tokens(fragment, &role))
                .sum();
            // The most-demanded fragment in this message decides it: promoting
            // the message serves all of them, so the one asked for most often
            // is what the rewrite buys. Unknown counts read as one demand.
            let fetches: u32 = refs.pairs()[inline_mark.pairs..]
                .iter()
                .filter(|(id, _)| req.keep_inline.contains(id))
                .map(|(id, _)| {
                    req.fetches
                        .iter()
                        .find(|(other, _)| other == id)
                        .map_or(1, |(_, n)| *n)
                })
                .max()
                .unwrap_or(0);
            if compressed_here && protected > 0 {
                // What shipping this message whole gives up, every later turn.
                let forfeited = count_tokens(&original, &role)
                    .saturating_sub(count_tokens(&messages[index].content, &role));
                // Everything after this message, which a rewrite here makes the
                // provider read again, and the whole prompt it is a fraction
                // of. Counted on what the messages hold right now: the ones
                // already past have shipped in their final form, and the ones
                // still ahead can only shrink, so the ratio errs towards
                // admitting rather than refusing.
                let break_cost: u32 = messages[index + 1..]
                    .iter()
                    .map(|later| count_tokens(&later.content, &later.role))
                    .sum();
                let whole: u32 = messages
                    .iter()
                    .map(|each| count_tokens(&each.content, &each.role))
                    .sum();
                if keeping_it_inline_is_worth_it(
                    forfeited,
                    protected,
                    break_cost,
                    whole,
                    fetches,
                    req.cache_hit_per_mille,
                ) {
                    kept_inline_ids.extend(
                        refs.pairs()[inline_mark.pairs..]
                            .iter()
                            .filter(|(id, _)| req.keep_inline.contains(id))
                            .map(|(id, _)| *id),
                    );
                    refs.rollback(inline_mark);
                    // Keep fetched source bodies, not unrelated bodies that happen
                    // to share their message. Retry only after the existing cost
                    // checks approve promotion; use a scratch collector so this
                    // policy cannot leak into later messages or requests.
                    // The outer kind may be JSON or mixed text even when the
                    // fetched fragments are source bodies nested inside it.
                    let selective = compress_preserving_code(&original, &role, opts, req, refs)
                        .map(|(content, kinds)| {
                            messages[index].content = content;
                            applied = kinds;
                        })
                        .is_some();
                    if !selective {
                        messages[index].content = original;
                    }
                    compressed_here = selective;
                    kept_inline = true;
                } else {
                    // Said out loud, because the consequence is that the model
                    // may ask for this fragment again and spend another round
                    // doing it. The same reasoning put the ranged-retrieval
                    // salvage on `warn`: two paid runs narrowed nothing at all
                    // while it was a `debug`.
                    tracing::warn!(
                        forfeited,
                        protected,
                        break_cost,
                        whole,
                        fetches,
                        hit_per_mille = req.cache_hit_per_mille,
                        needed =
                            fetches_to_repay_a_break(break_cost, whole, req.cache_hit_per_mille),
                        budget = KEEP_INLINE_MAX_FORFEIT,
                        "keeping a fetched fragment inline would cost more than \
                         it protects; compressing it again"
                    );
                }
            }
        }

        seen.push(&messages[index].content.joined_text());

        if compressed_here {
            stats.compressed_messages += 1;
            let mut seen = BTreeSet::new();
            for kind in applied {
                if seen.insert(kind) {
                    *stats.by_type.entry(kind.as_str().to_string()).or_insert(0) += 1;
                }
            }
        } else {
            stats.skipped_messages += 1;
            let reason = if kept_inline {
                "retrieved_before".to_string()
            } else {
                decline_reason(&messages[index].content, &role, opts)
            };
            stats.skip(&reason, count_tokens(&messages[index].content, &role));
        }

        stats.tokens_out += count_tokens(&messages[index].content, &role);
        stats.chars_out += messages[index].content.text_len() as u64;
    }

    stats.protected_refs = protected_ref_ids.into_iter().collect();
    stats.kept_inline_refs = kept_inline_ids.into_iter().collect();
    stats.finish(opts.mode, refs)
}

/// Retry an approved promotion with fetched source bodies held inline. A
/// fallback route must not hide any of them again; if it does, keep the
/// original message, just as the whole-message promotion did.
fn compress_preserving_code(
    original: &MessageContent,
    role: &str,
    opts: &EngineOptions,
    req: &RequestOptions,
    refs: &mut RefCollector,
) -> Option<(MessageContent, Vec<Kind>)> {
    let mut candidate = original.clone();
    let mut scratch = refs.scratch();
    scratch.preserve_code(req.keep_inline);
    let mut applied = Vec::new();
    let mut changed = false;
    let mut compress = |text: &mut String| {
        let ctx = Context::for_role(role);
        let (out, kind) =
            compress_content(text, role, opts, None, &mut scratch, &ctx, &mut applied);
        if let Some(kind) = kind {
            *text = out.into_owned();
            applied.push(kind);
            changed = true;
        }
    };
    match &mut candidate {
        MessageContent::Text(text) => compress(text),
        MessageContent::Blocks(slots) => {
            for slot in slots {
                let lifted = slot.protection == Protection::WhileCached
                    && req.prefix_cache == PrefixCache::Cold;
                if !slot.protected || lifted {
                    compress(&mut slot.text);
                }
            }
        }
        MessageContent::Opaque => return None,
    }
    if !changed
        || scratch
            .pairs()
            .iter()
            .any(|(id, _)| req.keep_inline.contains(id))
    {
        return None;
    }
    refs.adopt(scratch);
    Some((candidate, applied))
}

fn texts_of(content: &MessageContent) -> Vec<&str> {
    match content {
        MessageContent::Text(t) => vec![t.as_str()],
        MessageContent::Blocks(slots) => slots.iter().map(|s| s.text.as_str()).collect(),
        MessageContent::Opaque => Vec::new(),
    }
}

fn count_tokens(content: &MessageContent, role: &str) -> u32 {
    match content {
        MessageContent::Text(t) => estimate_tokens(t, role),
        MessageContent::Blocks(slots) => slots.iter().map(|s| estimate_tokens(&s.text, role)).sum(),
        MessageContent::Opaque => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_payload(lines: usize) -> String {
        (0..lines)
            .map(|i| {
                format!(
                    "2026-01-02T03:04:{:02}Z INFO worker heartbeat seq={i}",
                    i % 60
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn run(messages: &mut [Message], opts: &EngineOptions, req: RequestOptions) -> CompressStats {
        let mut refs = opts.collector();
        compress_messages(messages, opts, &req, &mut refs)
    }

    #[test]
    fn the_models_own_turns_are_never_compressed() {
        let opts = EngineOptions::default();
        let payload = log_payload(200);
        let mut messages = vec![
            Message::text("system", payload.clone()),
            Message::text("assistant", payload.clone()),
            Message::text("user", payload.clone()),
        ];
        let stats = run(&mut messages, &opts, RequestOptions::default());
        match (&messages[0].content, &messages[1].content) {
            (MessageContent::Text(system), MessageContent::Text(assistant)) => {
                assert_eq!(system, &payload);
                assert_eq!(assistant, &payload);
            }
            _ => panic!("content shape changed"),
        }
        assert_eq!(stats.protected_by_reason.get("role:system"), Some(&1));
        assert_eq!(stats.protected_by_reason.get("role:assistant"), Some(&1));
        assert_eq!(stats.compressed_messages, 1, "only the user turn");
    }

    #[test]
    fn a_harness_tool_result_arriving_as_user_is_compressible() {
        // agent harnesses deliver terminal output as a user message, which is
        // why `user` is deliberately absent from the protected roles
        let opts = EngineOptions::default();
        let mut messages = vec![Message::text("user", log_payload(300))];
        let stats = run(&mut messages, &opts, RequestOptions::default());
        assert_eq!(stats.compressed_messages, 1);
    }

    #[test]
    fn the_frozen_prefix_comes_back_byte_identical() {
        let opts = EngineOptions::default();
        let payload = log_payload(200);
        let mut messages = vec![
            Message::text("user", payload.clone()),
            Message::text("user", payload.clone()),
        ];
        let stats = run(
            &mut messages,
            &opts,
            RequestOptions {
                frozen_message_count: 1,
                ..Default::default()
            },
        );
        match &messages[0].content {
            MessageContent::Text(t) => assert_eq!(t, &payload),
            _ => panic!(),
        }
        assert_eq!(stats.protected_by_reason.get("frozen"), Some(&1));
    }

    #[test]
    fn a_message_compresses_the_same_however_much_follows_it() {
        // The property the proxy's provider-cache story rests on, and the one
        // that decides whether the frozen floor may be lowered at all.
        //
        // A provider caches a *prefix of bytes*. If turn five and turn twenty
        // both send message three, they must send it identically or the cache
        // misses from that byte onward and everything after it is re-read at
        // full price. Compressing history is only safe because the engine's
        // output for a message does not depend on how much conversation
        // arrived after it.
        //
        // Asserted rather than assumed because cross-turn dedup reads the
        // messages around it: a rule that let a later message change how an
        // earlier one compresses would make every turn a cache miss, which is
        // the opposite of the saving this is for.
        let opts = EngineOptions::default();
        let payload = log_payload(200);

        let compress_with = |extra: usize| {
            let mut messages = vec![
                Message::text("user", "the opening turn".to_string()),
                Message::text("tool", payload.clone()),
            ];
            for i in 0..extra {
                messages.push(Message::text("tool", log_payload(120 + i)));
            }
            run(&mut messages, &opts, RequestOptions::default());
            match &messages[1].content {
                MessageContent::Text(t) => t.clone(),
                other => panic!("text in, text out, got {other:?}"),
            }
        };

        let alone = compress_with(0);
        // Non-vacuous: a message the engine declined to touch is trivially
        // stable, and would let this pass while saying nothing.
        assert!(
            alone.len() < payload.len(),
            "the message under test has to actually be compressed: {} vs {}",
            alone.len(),
            payload.len()
        );
        for extra in [1, 5, 20] {
            assert_eq!(
                compress_with(extra),
                alone,
                "message one must not change because {extra} more messages followed it"
            );
        }
    }

    #[test]
    fn stale_reads_are_the_one_transform_that_rewrites_what_came_before() {
        // The prefix-monotonic invariant, stated as a test because exactly one
        // thing in this engine breaks it.
        //
        // `crossturn` documents the rule: a message is matched only against
        // strictly earlier ones, so the output of `0..k` is byte-identical
        // whether or not `k+1` exists, and appending a turn never rewrites an
        // earlier one. That is what lets the proxy compress history and still
        // hit the provider's cache.
        //
        // Stale-read is the exception. A later *write* to the same path
        // replaces an earlier read with a marker, so appending a turn does
        // rewrite an earlier turn. Measured on a Harbor run with the floor
        // lowered: turn five sent a read and the provider cached it, turn six
        // added the edit, and 4,324 cached tokens were re-written at 1.25x.
        //
        // Asserted in both directions so that whoever makes stale-read causal
        // finds this test rather than the bill: the first half is a tripwire on
        // a known violation, and the second is the property the proxy relies on
        // when it turns the feature off.
        let payload = log_payload(200);
        let read = || Message {
            role: "tool".into(),
            content: MessageContent::Text(payload.clone()),
            meta: MessageMeta {
                tool_name: Some("read".into()),
                file_path: Some("src/a.rs".into()),
                ..Default::default()
            },
        };
        let edit = || Message {
            role: "tool".into(),
            content: MessageContent::Text("patched".into()),
            meta: MessageMeta {
                tool_name: Some("edit".into()),
                file_path: Some("src/a.rs".into()),
                ..Default::default()
            },
        };
        let sent = |opts: &EngineOptions, with_edit: bool| {
            let mut messages = vec![Message::text("user", "go".to_string()), read()];
            if with_edit {
                messages.push(edit());
            }
            run(&mut messages, opts, RequestOptions::default());
            match &messages[1].content {
                MessageContent::Text(t) => t.clone(),
                other => panic!("text in, text out, got {other:?}"),
            }
        };

        let on = EngineOptions::default();
        assert!(on.enable_stale_read, "the default this is about");
        assert_ne!(
            sent(&on, true),
            sent(&on, false),
            "with stale reads on, appending an edit rewrites the read before it \
             — if this now passes, the transform became causal and the proxy no \
             longer needs to switch it off"
        );

        let off = EngineOptions {
            enable_stale_read: false,
            ..EngineOptions::default()
        };
        assert_eq!(
            sent(&off, true),
            sent(&off, false),
            "with them off the prefix is monotonic: the read the provider has \
             already cached goes out unchanged"
        );
    }

    #[test]
    fn skips_are_weighted_by_tokens_as_well_as_counted() {
        // The count map alone cannot rank anything. A message skipped for
        // being below the floor and a whole declined payload are one row each
        // in `skipped_by_reason` and could hardly differ more in what they are
        // worth pursuing.
        let opts = EngineOptions::default();
        let payload = log_payload(200);
        let mut messages = vec![
            Message::text("tool", "tiny".to_string()),
            Message::text("assistant", payload.clone()),
            Message {
                role: "tool".into(),
                content: MessageContent::Opaque,
                meta: MessageMeta::default(),
            },
        ];
        let stats = run(&mut messages, &opts, RequestOptions::default());

        let counts = &stats.skipped_by_reason;
        let tokens = &stats.skipped_tokens_by_reason;
        assert_eq!(
            counts.keys().collect::<Vec<_>>(),
            tokens.keys().collect::<Vec<_>>(),
            "the two maps must partition the same skips"
        );
        assert_eq!(counts.get("below_min_chars"), Some(&1));
        assert_eq!(
            tokens.get("below_min_chars"),
            Some(&1),
            "\"tiny\" is one token, and a floor skip is cheap by construction"
        );
        assert!(
            tokens
                .get("protected:role:assistant")
                .is_some_and(|t| *t > 100),
            "the protected payload is where the mass is: {tokens:?}"
        );
        // The limitation, asserted rather than left to be discovered: the
        // engine holds no text for an opaque message, so it cannot say what
        // the provider billed for it. 0 here means unknown, not empty.
        assert_eq!(counts.get("opaque"), Some(&1));
        assert_eq!(tokens.get("opaque"), Some(&0));
    }

    #[test]
    fn a_cold_prefix_is_no_longer_frozen() {
        // The freeze exists to protect a cache read. Once the cache has lapsed
        // there is nothing to protect and the prefix is the cheapest text in
        // the request to remove, so the same request compresses where it
        // previously came back whole.
        let opts = EngineOptions::default();
        let payload = log_payload(200);
        let mut messages = vec![Message::text("user", payload.clone())];
        let stats = run(
            &mut messages,
            &opts,
            RequestOptions {
                frozen_message_count: 1,
                prefix_cache: PrefixCache::Cold,
                ..Default::default()
            },
        );
        match &messages[0].content {
            MessageContent::Text(t) => assert!(t.len() < payload.len(), "a cold prefix compresses"),
            _ => panic!(),
        }
        assert_eq!(stats.protected_by_reason.get("frozen"), None);
        assert_eq!(stats.frozen_messages, 0);
    }

    #[test]
    fn a_slot_pinned_only_by_a_cache_breakpoint_lifts_when_the_cache_is_gone() {
        // The half of `PrefixCache::Cold` that lives below the message. On
        // Anthropic one `cache_control` marker does both jobs — it puts the
        // message in the frozen count *and* pins its block — so lifting
        // `effective_frozen` alone changed nothing: the message thawed and the
        // slot stayed shut. Measured before this existed, the request went out
        // byte-identical on both turns.
        let opts = EngineOptions::default();
        let payload = log_payload(200);
        let slot = |protection| TextSlot {
            text: payload.clone(),
            protected: true,
            protection,
        };
        let pinned = |protection, prefix_cache| {
            let mut messages = vec![Message {
                role: "user".into(),
                content: MessageContent::Blocks(vec![slot(protection)]),
                meta: MessageMeta::default(),
            }];
            run(
                &mut messages,
                &opts,
                RequestOptions {
                    prefix_cache,
                    ..Default::default()
                },
            );
            match &messages[0].content {
                MessageContent::Blocks(slots) => slots[0].text.len(),
                _ => panic!("the shape survives"),
            }
        };

        assert!(
            pinned(Protection::WhileCached, PrefixCache::Cold) < payload.len(),
            "a breakpoint naming a cache that is gone protects nothing"
        );
        assert_eq!(
            pinned(Protection::WhileCached, PrefixCache::Warm),
            payload.len(),
            "while the cache is live the breakpoint is load-bearing and the \
             bytes must not move"
        );
        assert_eq!(
            pinned(Protection::Always, PrefixCache::Cold),
            payload.len(),
            "a signature or a citation is not about the cache: no prefix state \
             makes it safe to rewrite"
        );
    }

    #[test]
    fn warm_is_the_default_a_caller_gets() {
        // Guessing `Cold` on a live cache costs a re-read of everything after
        // the first rewritten byte; guessing `Warm` on a dead one leaves a
        // saving unclaimed. The cheap mistake is the default.
        assert_eq!(PrefixCache::default(), PrefixCache::Warm);
        let req = RequestOptions {
            frozen_message_count: 3,
            ..Default::default()
        };
        assert_eq!(req.effective_frozen(), 3);
    }

    #[test]
    fn a_later_message_points_at_what_an_earlier_one_showed() {
        // End to end: the same file content twice, and the second copy folds.
        let opts = EngineOptions::default();
        let body: String = (0..40)
            .map(|i| format!("    total_{i} = accumulate(rows, {i}, offset={})\n", i * 7))
            .collect();
        let mut messages = vec![
            Message::text("tool", body.clone()),
            Message::text("tool", body.clone()),
        ];
        run(&mut messages, &opts, RequestOptions::default());
        let second = match &messages[1].content {
            MessageContent::Text(t) => t.clone(),
            _ => panic!(),
        };
        assert!(
            second.contains("shown in full earlier"),
            "the second copy points back: {second}"
        );
    }

    #[test]
    fn the_first_message_keeps_the_bytes_the_second_points_at() {
        let opts = EngineOptions::default();
        let body: String = (0..40)
            .map(|i| format!("    total_{i} = accumulate(rows, {i}, offset={})\n", i * 7))
            .collect();
        let mut one = vec![Message::text("tool", body.clone())];
        run(&mut one, &opts, RequestOptions::default());
        let alone = match &one[0].content {
            MessageContent::Text(t) => t.clone(),
            _ => panic!(),
        };

        let mut two = vec![
            Message::text("tool", body.clone()),
            Message::text("tool", body.clone()),
        ];
        run(&mut two, &opts, RequestOptions::default());
        let first = match &two[0].content {
            MessageContent::Text(t) => t.clone(),
            _ => panic!(),
        };
        assert_eq!(
            alone, first,
            "adding a second message must not change the first — the cache prefix"
        );
    }

    #[test]
    fn a_cache_control_message_is_withheld() {
        let opts = EngineOptions::default();
        let payload = log_payload(200);
        let mut messages = vec![Message {
            role: "user".into(),
            content: MessageContent::Text(payload.clone()),
            meta: MessageMeta {
                cache_control: true,
                ..Default::default()
            },
        }];
        let stats = run(&mut messages, &opts, RequestOptions::default());
        assert_eq!(stats.protected_by_reason.get("flagged"), Some(&1));
        match &messages[0].content {
            MessageContent::Text(t) => assert_eq!(t, &payload),
            _ => panic!(),
        }
    }

    #[test]
    fn an_override_can_reenable_compression_for_the_assistant() {
        let opts = EngineOptions {
            protected_roles: vec![],
            ..Default::default()
        };
        let mut messages = vec![Message::text("assistant", log_payload(300))];
        let stats = run(&mut messages, &opts, RequestOptions::default());
        assert_eq!(stats.compressed_messages, 1);
        assert!(stats.protected_by_reason.is_empty());
    }

    #[test]
    fn opaque_content_passes_through_and_is_counted_as_skipped() {
        let opts = EngineOptions::default();
        let mut messages = vec![Message {
            role: "user".into(),
            content: MessageContent::Opaque,
            meta: MessageMeta::default(),
        }];
        let stats = run(&mut messages, &opts, RequestOptions::default());
        assert_eq!(stats.skipped_messages, 1);
        assert_eq!(stats.compressed_messages, 0);
    }

    #[test]
    fn block_content_is_compressed_per_slot_and_protected_slots_survive() {
        let opts = EngineOptions::default();
        let payload = log_payload(300);
        let mut messages = vec![Message {
            role: "user".into(),
            content: MessageContent::Blocks(vec![
                TextSlot {
                    text: payload.clone(),
                    protected: true,
                    protection: Protection::Always,
                },
                TextSlot::new(payload.clone()),
            ]),
            meta: MessageMeta::default(),
        }];
        let stats = run(&mut messages, &opts, RequestOptions::default());
        match &messages[0].content {
            MessageContent::Blocks(slots) => {
                assert_eq!(slots[0].text, payload, "a cache breakpoint keeps its bytes");
                assert!(slots[1].text.chars().count() < payload.chars().count());
            }
            _ => panic!(),
        }
        assert_eq!(stats.compressed_messages, 1);
    }

    #[test]
    fn a_handle_stranded_in_protected_content_is_reported() {
        let opts = EngineOptions::default();
        let handle = crate::refs::ref_for("elided fragment");
        let quoted = format!("earlier I saw [... 4 items omitted [ref:{handle}]] in the output");
        let mut messages = vec![Message::text("assistant", quoted)];
        let stats = run(&mut messages, &opts, RequestOptions::default());
        assert_eq!(stats.protected_refs, vec![handle]);
        assert_eq!(stats.protected_ref_messages, 1);
        assert!(stats.protected_ref_reasons.contains("role:assistant"));
    }

    #[test]
    fn a_read_a_later_edit_invalidated_is_replaced_with_a_retrievable_marker() {
        let opts = EngineOptions::default();
        let file = "x = 1\n".repeat(200);
        let mut messages = vec![
            Message {
                role: "tool".into(),
                content: MessageContent::Text(file.clone()),
                meta: MessageMeta {
                    tool_name: Some("read_file".into()),
                    file_path: Some("src/app.py".into()),
                    ..Default::default()
                },
            },
            Message {
                role: "tool".into(),
                content: MessageContent::Text("ok".into()),
                meta: MessageMeta {
                    tool_name: Some("write_file".into()),
                    file_path: Some("src/app.py".into()),
                    ..Default::default()
                },
            },
        ];
        let mut refs = opts.collector();
        let stats = compress_messages(&mut messages, &opts, &RequestOptions::default(), &mut refs);
        match &messages[0].content {
            MessageContent::Text(t) => {
                assert!(t.starts_with("[Read content stale: src/app.py"));
                assert!(t.contains("ref:k_"));
            }
            _ => panic!(),
        }
        assert_eq!(stats.by_type.get("stale_read"), Some(&1));
        assert!(!refs.is_empty());
    }

    #[test]
    fn the_accounting_adds_up() {
        let opts = EngineOptions::default();
        let mut messages = vec![
            Message::text("user", log_payload(300)),
            Message::text("assistant", "a short reply"),
            Message::text("user", "tiny"),
        ];
        let stats = run(&mut messages, &opts, RequestOptions::default());
        assert_eq!(
            stats.compressed_messages + stats.skipped_messages,
            messages.len() as u32
        );
        assert!(stats.tokens_out < stats.tokens_in);
        assert!(stats.ratio < 1.0 && stats.ratio > 0.0);
        assert_eq!(stats.mode, Mode::Recoverable);
    }
}

/// Naming the zero: which of the several reasons a run removed nothing.
#[cfg(test)]
mod outcome_tests {
    use super::*;

    fn stats(compressed: u32, skipped: u32, frozen: u32, other_protected: u32) -> CompressStats {
        let mut stats = CompressStats {
            compressed_messages: compressed,
            skipped_messages: skipped,
            frozen_messages: frozen,
            ..Default::default()
        };
        if frozen > 0 {
            stats
                .protected_by_reason
                .insert("frozen".to_string(), frozen);
        }
        if other_protected > 0 {
            stats
                .protected_by_reason
                .insert("role:assistant".to_string(), other_protected);
        }
        stats
    }

    #[test]
    fn a_run_that_removed_something_is_simply_compressed() {
        assert_eq!(stats(3, 20, 20, 0).outcome(), Outcome::Compressed);
    }

    /// The shape a wrapped Claude Code session produces: every message inside
    /// the provider-cached prefix, nothing touchable.
    #[test]
    fn a_wholly_pinned_prompt_is_prefix_frozen() {
        assert_eq!(stats(0, 6, 6, 0).outcome(), Outcome::PrefixFrozen);
    }

    #[test]
    fn a_wholly_role_protected_prompt_is_not_blamed_on_the_cache() {
        assert_eq!(stats(0, 4, 0, 4).outcome(), Outcome::AllProtected);
    }

    /// Eligible content that no compressor took is the one worth chasing, and
    /// it must not hide behind either protection reason.
    #[test]
    fn eligible_content_nothing_took_is_named_separately() {
        assert_eq!(stats(0, 9, 2, 3).outcome(), Outcome::NoCompressibleContent);
        assert_eq!(stats(0, 5, 0, 0).outcome(), Outcome::NoCompressibleContent);
    }

    #[test]
    fn every_outcome_has_a_stable_name() {
        for (outcome, name) in [
            (Outcome::Compressed, "compressed"),
            (Outcome::PrefixFrozen, "prefix_frozen"),
            (Outcome::AllProtected, "all_protected"),
            (Outcome::NoCompressibleContent, "no_compressible_content"),
        ] {
            assert_eq!(outcome.as_str(), name);
        }
    }
}

#[cfg(test)]
mod skip_reason_tests {
    use super::*;

    fn opts() -> EngineOptions {
        EngineOptions {
            mode: Mode::Recoverable,
            ..Default::default()
        }
    }

    #[cfg(feature = "code-aware")]
    #[test]
    fn fetched_shared_bodies_do_not_expose_an_unfetched_body() {
        // Exercise both source compressors and both message representations.
        for python in [true, false] {
            for (blocks, json_envelope) in
                [(false, false), (true, false), (false, true), (true, true)]
            {
                let source = |variant: &str| -> String {
                    (0..18)
                        .map(|function| {
                            let mut body = String::new();
                            for step in 0..26 {
                                let value = if function == 17 && step == 17 {
                                    format!("\"{variant}\"")
                                } else {
                                    (function * 26 + step).to_string()
                                };
                                body.push_str(&if python {
                                    format!("    step_{step} = compute(entries, {value})\n")
                                } else {
                                    format!("    const step_{step} = compute(entries, {value});\n")
                                });
                            }
                            if python {
                                format!("def reconcile_{function:02}(entries):\n{body}\n")
                            } else {
                                format!("function reconcile_{function:02}(entries) {{\n{body}}}\n")
                            }
                        })
                        .collect()
                };
                let message = |text: &str| {
                    let wrapped = if json_envelope {
                        serde_json::json!({"output": text, "exit_code": 0}).to_string()
                    } else {
                        text.to_string()
                    };
                    let text = wrapped.as_str();
                    let mut message = msg("tool", text);
                    if blocks {
                        let mut protected = TextSlot::new("protected context");
                        protected.protected = true;
                        message.content =
                            MessageContent::Blocks(vec![protected, TextSlot::new(text)]);
                    }
                    message
                };
                let mut warmup_handles = Vec::new();
                for variant in ["decoy-one", "decoy-two"] {
                    let mut messages = vec![message(&source(variant))];
                    let mut refs = RefCollector::new(1 << 20);
                    compress_messages(
                        &mut messages,
                        &opts(),
                        &RequestOptions::default(),
                        &mut refs,
                    );
                    assert_eq!(refs.len(), 18);
                    warmup_handles.push(refs.pairs().iter().map(|(id, _)| *id).collect::<Vec<_>>());
                }
                assert_eq!(&warmup_handles[0][..17], &warmup_handles[1][..17]);
                assert_ne!(warmup_handles[0][17], warmup_handles[1][17]);
                let keep = &warmup_handles[0][..8];
                let original = message(&source("unfetched-ledger-secret"));
                let mut messages = vec![original.clone()];
                let mut refs = RefCollector::new(1 << 20);
                let stats = compress_messages(
                    &mut messages,
                    &opts(),
                    &RequestOptions {
                        keep_inline: keep,
                        ..Default::default()
                    },
                    &mut refs,
                );
                let output = messages[0].content.joined_text();
                assert_eq!(stats.compressed_messages, 1);
                assert_eq!(stats.kept_inline_refs.len(), 8);
                assert_eq!(refs.len(), 10);
                assert_eq!(output.matches("body lines omitted").count(), 10);
                assert!(output.contains("step_0 = compute(entries, 0)"));
                assert!(!output.contains("unfetched-ledger-secret"));
                assert!(refs.pairs().iter().all(|(id, _)| !keep.contains(id)));
                assert!(
                    refs.pairs()
                        .iter()
                        .any(|(_, text)| text.contains("unfetched-ledger-secret"))
                );
                for (id, _) in refs.pairs() {
                    assert!(output.contains(&id.to_string()), "no orphan references");
                }
                if let MessageContent::Blocks(slots) = &messages[0].content {
                    assert_eq!(slots[0].text, "protected context");
                    assert!(slots[0].protected);
                }
                // Protection belongs to this retry, not the reusable collector.
                let mut fresh = vec![original.clone()];
                let later =
                    compress_messages(&mut fresh, &opts(), &RequestOptions::default(), &mut refs);
                assert_eq!(later.compressed_messages, 1);
                assert_eq!(
                    fresh[0]
                        .content
                        .joined_text()
                        .matches("body lines omitted")
                        .count(),
                    18
                );
                // Once every body is fetched, keeping the complete original is
                // still correct, and must not leave orphan references behind.
                let all: Vec<_> = refs.pairs().iter().map(|(id, _)| *id).collect();
                let mut fetched = vec![original.clone()];
                let mut final_refs = RefCollector::new(1 << 20);
                let final_stats = compress_messages(
                    &mut fetched,
                    &opts(),
                    &RequestOptions {
                        keep_inline: &all,
                        ..Default::default()
                    },
                    &mut final_refs,
                );
                assert_eq!(fetched[0].content, original.content);
                assert!(final_refs.is_empty());
                assert_eq!(
                    final_stats.skipped_by_reason.get("retrieved_before"),
                    Some(&1)
                );
            }
        }
    }

    fn msg(role: &str, text: &str) -> Message {
        Message {
            role: role.to_string(),
            content: MessageContent::Text(text.to_string()),
            meta: MessageMeta::default(),
        }
    }

    /// Eliding a fragment is a bet that nobody will ask for it. A retrieval
    /// settles that bet badly: the round answering it re-sends the whole prompt
    /// upstream, and the fetched bytes never reach the client's history, so the
    /// next turn asks again. Measured on a real run: the same three handles
    /// fetched on three consecutive turns, ~274,000 prompt tokens spent to
    /// deliver 3,815 tokens of fragment.
    #[test]
    fn content_already_fetched_back_is_kept_inline_from_then_on() {
        // Lines sharing a template but differing in their values, so the fold
        // cannot be reversed from a `[xN]` count and a handle is retained.
        let payload: String = (0..200)
            .map(|i| {
                format!(
                    "2026-01-02T03:04:{:02}Z INFO worker seq={i} value={i}\n",
                    i % 60
                )
            })
            .collect();
        let original = msg("tool", &payload);

        // Turn one: nothing has been retrieved, so it compresses and mints a
        // handle. Without this the rest of the test would prove nothing.
        let mut first = vec![original.clone()];
        let mut refs = RefCollector::new(1 << 20);
        let before = compress_messages(&mut first, &opts(), &RequestOptions::default(), &mut refs);
        assert_eq!(before.compressed_messages, 1, "the fixture must compress");
        assert!(!refs.pairs().is_empty(), "and must mint a handle");
        let fetched = refs.pairs()[0].0;

        // Turn two, same bytes, after the model fetched that handle back.
        let mut second = vec![original.clone()];
        let mut refs2 = RefCollector::new(1 << 20);
        let keep = [fetched];
        let after = compress_messages(
            &mut second,
            &opts(),
            &RequestOptions {
                keep_inline: &keep,
                ..Default::default()
            },
            &mut refs2,
        );
        assert_eq!(after.compressed_messages, 0, "it must not compress again");
        assert_eq!(
            second[0].content.joined_text(),
            original.content.joined_text(),
            "the message ships exactly as it arrived"
        );
        assert_eq!(
            after.skipped_by_reason.get("retrieved_before"),
            Some(&1),
            "and says why, rather than hiding in declined:*: {:?}",
            after.skipped_by_reason
        );
        assert!(
            refs2.pairs().is_empty(),
            "the rolled-back fragment must not be left in the store for a \
             marker that no longer points at it"
        );
    }

    /// Where the message sits decides whether keeping it inline is affordable,
    /// and the ratio rule above cannot see position at all.
    ///
    /// Measured 2026-10-08 on `claude-opus-5`: a 309-token fragment held inline
    /// at 15.9% of the prompt cost 46,379 tokens of cache -- $0.21 in forfeited
    /// reads and $0.29 to write it again, 17.6% of a $2.83 run -- to avoid
    /// about $0.04 of retrieve rounds. The recurring rule waved it through
    /// because 309 tokens comfortably justified what that one message gave up.
    /// The proxy's own promotion path already priced the break; this one did
    /// not, and the loss came through here.
    #[test]
    fn a_fetched_fragment_early_in_the_prompt_is_not_kept_inline() {
        let payload: String = (0..200)
            .map(|i| {
                format!(
                    "2026-01-02T03:04:{:02}Z INFO worker seq={i} value={i}\n",
                    i % 60
                )
            })
            .collect();
        let original = msg("tool", &payload);

        let mut first = vec![original.clone()];
        let mut refs = RefCollector::new(1 << 20);
        compress_messages(&mut first, &opts(), &RequestOptions::default(), &mut refs);
        let fetched = refs.pairs()[0].0;

        // The same fetched fragment, with a conversation after it. Everything
        // following has to be read again if this message is rewritten, and
        // there is far more of it than the fragment can repay.
        // Big enough to pass `protected * KEEP_INLINE_BREAK_PAYBACK_FETCHES`.
        // The field case was 309 tokens protected against 46,379 behind it, a
        // ratio of 150; this fixture's fragment is 4,514 tokens, so the tail
        // has to clear 36,112 rather than that. Measured here at ~67,000.
        let tail: String = (0..6_000)
            .map(|i| format!("a later turn carrying its own text, line {i}\n"))
            .collect();
        let mut later = vec![original.clone(), msg("user", &tail)];
        let keep = [fetched];
        let mut refs2 = RefCollector::new(1 << 20);
        let after = compress_messages(
            &mut later,
            &opts(),
            &RequestOptions {
                keep_inline: &keep,
                ..Default::default()
            },
            &mut refs2,
        );

        assert_eq!(
            after.skipped_by_reason.get("retrieved_before"),
            None,
            "a promotion this expensive must not be taken: {:?}",
            after.skipped_by_reason
        );
        assert!(
            later[0].content.joined_text().len() < original.content.joined_text().len(),
            "the message stays compressed, so the prefix in front of the tail \
             does not move"
        );
    }

    /// And the same fragment, once it has been asked for often enough.
    ///
    /// The unit tests above pin the rule; this one proves the count reaches it.
    /// `RequestOptions::fetches` is the only route by which the engine learns
    /// how often a handle has been demanded, and a rule fed a default of one
    /// demand would refuse everything early in a prompt for ever.
    #[test]
    fn a_fragment_asked_for_often_enough_is_kept_inline_despite_the_rewrite() {
        let payload: String = (0..200)
            .map(|i| {
                format!(
                    "2026-01-02T03:04:{:02}Z INFO worker seq={i} value={i}\n",
                    i % 60
                )
            })
            .collect();
        let original = msg("tool", &payload);

        let mut first = vec![original.clone()];
        let mut refs = RefCollector::new(1 << 20);
        compress_messages(&mut first, &opts(), &RequestOptions::default(), &mut refs);
        let fetched = refs.pairs()[0].0;

        let tail: String = (0..6_000)
            .map(|i| format!("a later turn carrying its own text, line {i}\n"))
            .collect();
        let mut later = vec![original.clone(), msg("user", &tail)];
        let keep = [fetched];
        // Demanded more times than the rewrite can cost, which is at most
        // the full multiple even at the very front of the prompt.
        let counts = [(fetched, CACHE_DISCOUNT)];
        let mut refs2 = RefCollector::new(1 << 20);
        let after = compress_messages(
            &mut later,
            &opts(),
            &RequestOptions {
                keep_inline: &keep,
                fetches: &counts,
                ..Default::default()
            },
            &mut refs2,
        );

        assert_eq!(
            after.skipped_by_reason.get("retrieved_before"),
            Some(&1),
            "a fragment this often demanded must be promoted: {:?}",
            after.skipped_by_reason
        );
        assert_eq!(
            later[0].content.joined_text(),
            original.content.joined_text(),
            "and ships whole"
        );
    }

    /// The guard's whole job is the ratio, so it is tested as a ratio.
    ///
    /// `break_cost` is 0 throughout: these cases pin the recurring bound, and a
    /// break would be a second reason to refuse that would hide which one fired.
    /// The break bound has its own test below.
    #[test]
    fn keeping_a_fragment_inline_is_refused_when_it_costs_far_more_than_it_saves() {
        // The measured healthy case: ~1,980 tokens forfeited to protect ~233,
        // a ratio of 8.5. It is the tightest of the four field observations
        // against a bound of 10, so it is the one that pins the floor: lower
        // the constant and this is what breaks first.
        assert!(keeping_it_inline_is_worth_it(1_980, 233, 0, 0, 0, 1000));
        // The three later runs that kept one, two and three messages inline,
        // at ratios of 3.0, 2.4 and 3.4. All comfortably inside.
        for (forfeited, protected) in [(1_720, 576), (3_958, 1_668), (5_675, 1_668)] {
            assert!(
                keeping_it_inline_is_worth_it(forfeited, protected, 0, 0, 0, 1000),
                "{forfeited}/{protected} was measured working and must not be refused"
            );
        }
        // The bound is the cache discount, solved from seven live runs at
        // $5.00 fresh against $0.50 cached per MTok. A decision past it pays
        // more to hold the fragment than to fetch it again.
        assert_eq!(
            KEEP_INLINE_MAX_FORFEIT, 10,
            "the threshold is the provider's cache discount, not a free choice; \
             changing it means the discount was re-measured"
        );
        // The pathological one: a 50,000-token log held inline for 233 tokens
        // adds more to every prompt than the retrieve round it avoids.
        assert!(!keeping_it_inline_is_worth_it(50_000, 233, 0, 0, 0, 1000));
        // Exactly at the budget is still worth it; one token past is not.
        assert!(keeping_it_inline_is_worth_it(
            233 * KEEP_INLINE_MAX_FORFEIT,
            233,
            0,
            0,
            0,
            1000
        ));
        assert!(!keeping_it_inline_is_worth_it(
            233 * KEEP_INLINE_MAX_FORFEIT + 1,
            233,
            0,
            0,
            0,
            1000
        ));
        // A message that gives up nothing is always worth keeping inline.
        assert!(keeping_it_inline_is_worth_it(0, 1, 0, 0, 0, 1000));
        // Nothing fetched, nothing to protect: the guard never applies.
        assert!(!keeping_it_inline_is_worth_it(0, 0, 0, 0, 0, 1000));
        // A protected fragment large enough to overflow the multiply must not
        // wrap into a refusal.
        assert!(keeping_it_inline_is_worth_it(
            u32::MAX,
            u32::MAX,
            0,
            0,
            0,
            1000
        ));
    }

    /// The one-off bound, held apart from the recurring one.
    ///
    /// What repays a prefix rewrite is how often the fragment would otherwise
    /// be fetched again, so this is tested in fetches — the quantity the old
    /// rule named and did not use. Both field cases are here, and they pull in
    /// opposite directions: the rule has to get both right or it is just a
    /// differently-wrong constant.
    #[test]
    fn a_promotion_is_priced_against_how_often_the_fragment_is_asked_for() {
        // Anthropic, 2026-10-08. A fragment 84% of the way back through the
        // prompt, asked for twice. Promoting cost 46,379 tokens of cache --
        // $0.50 -- to save about $0.04 of rounds. Must refuse.
        assert!(!keeping_it_inline_is_worth_it(
            600, 309, 8_400, 10_000, 2, 1000
        ));
        // OpenAI, 2026-10-08. The same shape of fragment, asked for four times
        // and refused seventeen times over; nine avoidable rounds at $0.35
        // against $0.25 to promote. At 40% back, four demands clear it.
        assert!(keeping_it_inline_is_worth_it(
            600, 309, 4_000, 10_000, 4, 1000
        ));
        // Position is the whole difference. The same four demands do not buy a
        // rewrite of nearly the entire prompt.
        assert!(!keeping_it_inline_is_worth_it(
            600, 309, 9_500, 10_000, 4, 1000
        ));

        // The tail has nothing behind it and stays free, whatever the demand.
        // Without this nothing would ever be promotable.
        assert!(keeping_it_inline_is_worth_it(600, 309, 0, 10_000, 0, 1000));

        // The front of the prompt is the full multiple: nine demands.
        assert!(!keeping_it_inline_is_worth_it(
            600, 309, 10_000, 10_000, 8, 1000
        ));
        assert!(keeping_it_inline_is_worth_it(
            600, 309, 10_000, 10_000, 9, 1000
        ));
        assert_eq!(
            CACHE_DISCOUNT, 10,
            "the discount is the measured $5.00 against $0.50, not a free \
             choice; changing it means the rates were re-measured"
        );

        // The recurring bound still applies on top: a decision can be worth it
        // on demand and still give up far too much on every later turn.
        assert!(!keeping_it_inline_is_worth_it(
            50_000, 309, 0, 10_000, 1_000, 1000
        ));

        // A break larger than the prompt it is part of cannot ask for more than
        // the full multiple -- the engine's two sums are taken at different
        // moments and the ratio must not run away.
        assert!(keeping_it_inline_is_worth_it(
            0,
            309,
            u32::MAX,
            10_000,
            9,
            1000
        ));
    }

    /// The bar follows the measured cache hit rate, because that is what sets
    /// the price of the round a promotion avoids.
    ///
    /// Held to the arithmetic rather than to behaviour: a constant here is a
    /// constant that is right for one run, which this rule has now been twice.
    #[test]
    fn the_bar_is_computed_from_the_hit_rate_not_assumed() {
        // At the front of the prompt the ratio is 1, so this reads the multiple
        // straight off. The three rates these runs actually saw.
        assert_eq!(fetches_to_repay_a_break(1_000, 1_000, 913), 5);
        assert_eq!(fetches_to_repay_a_break(1_000, 1_000, 961), 6);
        assert_eq!(fetches_to_repay_a_break(1_000, 1_000, 975), 7);
        // A prompt entirely served from cache is the old fixed value, and the
        // default: a round is then at its cheapest, so the bar is at its
        // highest. That is the reading an unmeasured caller gets.
        assert_eq!(fetches_to_repay_a_break(1_000, 1_000, 1_000), 9);
        assert_eq!(RequestOptions::default().cache_hit_per_mille, 1_000);

        // Position scales it: the tail is free however the cache behaves.
        assert_eq!(fetches_to_repay_a_break(0, 1_000, 961), 0);
        assert_eq!(fetches_to_repay_a_break(500, 1_000, 1_000), 4);

        // The run that showed the assumption was wrong: 96.1% cached, a break
        // 83.1% of the way back. The rule asked 7 where the economics said 5.5,
        // and now asks 5.
        assert_eq!(fetches_to_repay_a_break(831, 1_000, 961), 5);

        // Degenerate inputs must not divide by zero or run away.
        assert_eq!(fetches_to_repay_a_break(1_000, 0, 961), 0);
        assert_eq!(fetches_to_repay_a_break(u32::MAX, 1_000, 1_000), 9);
        assert_eq!(fetches_to_repay_a_break(1_000, 1_000, u32::MAX), 9);
    }

    /// The control. An unrelated handle must change nothing, or the test above
    /// would pass for a message that simply stopped compressing.
    #[test]
    fn a_handle_from_another_conversation_does_not_keep_anything_inline() {
        // Lines sharing a template but differing in their values, so the fold
        // cannot be reversed from a `[xN]` count and a handle is retained.
        let payload: String = (0..200)
            .map(|i| {
                format!(
                    "2026-01-02T03:04:{:02}Z INFO worker seq={i} value={i}\n",
                    i % 60
                )
            })
            .collect();
        let mut messages = vec![msg("tool", &payload)];
        let mut refs = RefCollector::new(1 << 20);
        let elsewhere = [crate::refs::ref_for("a fragment this run never saw")];
        let stats = compress_messages(
            &mut messages,
            &opts(),
            &RequestOptions {
                keep_inline: &elsewhere,
                ..Default::default()
            },
            &mut refs,
        );
        assert_eq!(stats.compressed_messages, 1);
        assert!(!stats.skipped_by_reason.contains_key("retrieved_before"));
    }

    /// The property the whole breakdown rests on: every skip is accounted for
    /// exactly once, so the map can be read as a partition of `skipped_messages`
    /// rather than a sample of it.
    #[test]
    fn every_skipped_message_is_accounted_for_exactly_once() {
        let mut refs = RefCollector::disabled();
        let messages = vec![
            msg("assistant", "a protected role, whatever it says"),
            msg("user", "short"),
            msg("tool", &"unique line\n".repeat(3)),
            Message {
                role: "tool".to_string(),
                content: MessageContent::Opaque,
                meta: MessageMeta::default(),
            },
        ];
        let mut messages = messages;
        let stats = compress_messages(
            &mut messages,
            &opts(),
            &RequestOptions::default(),
            &mut refs,
        );

        let counted: u32 = stats.skipped_by_reason.values().sum();
        assert_eq!(
            counted, stats.skipped_messages,
            "every skip must carry exactly one reason: {:?}",
            stats.skipped_by_reason
        );
        assert!(stats.skipped_by_reason.contains_key("opaque"));
    }

    /// The key that answers the question this was built for. A payload the
    /// compressors looked at and declined names the type they declined it as,
    /// which `by_type` cannot report because it only counts successes.
    #[test]
    fn a_declined_payload_names_the_type_it_was_declined_as() {
        let mut refs = RefCollector::disabled();
        // Distinct lines, nothing to fold, comfortably over the size gate.
        let text: String = (0..80)
            .map(|i| format!("unrepeatable sentence number {i} about nothing in particular\n"))
            .collect();
        let mut messages = vec![msg("tool", &text)];
        let stats = compress_messages(
            &mut messages,
            &opts(),
            &RequestOptions::default(),
            &mut refs,
        );

        if stats.skipped_messages > 0 {
            let declined: Vec<&String> = stats
                .skipped_by_reason
                .keys()
                .filter(|k| k.starts_with("declined:"))
                .collect();
            assert!(
                !declined.is_empty(),
                "a declined payload over the size gate must name its type, got {:?}",
                stats.skipped_by_reason
            );
        }
    }

    #[test]
    fn a_payload_under_the_size_gate_says_so_rather_than_naming_a_type() {
        // Nothing ran on it, so naming a type would describe a classification
        // the engine never performed.
        let mut refs = RefCollector::disabled();
        let mut messages = vec![msg("tool", "tiny")];
        let stats = compress_messages(
            &mut messages,
            &opts(),
            &RequestOptions::default(),
            &mut refs,
        );
        assert_eq!(stats.skipped_by_reason.get("below_min_chars"), Some(&1));
    }
}
