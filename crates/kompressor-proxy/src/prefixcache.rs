//! Telling a live provider cache from a lapsed one.
//!
//! The engine already knows what to do with the answer: a frozen prefix is
//! protected while the provider is serving it at cache rates, and
//! `RequestOptions::effective_frozen` drops the freeze to zero once that
//! cache is gone. What it has never had is a caller able to tell the
//! difference, so `PrefixCache::Warm` was the only value ever constructed and
//! the cold path was dead code.
//!
//! # The asymmetry that decides everything here
//!
//! The two mistakes do not cost the same, and this module is shaped entirely
//! around that.
//!
//! * **Wrongly `Cold`** — the prefix *is* cached, the engine rewrites inside
//!   it, and the provider re-reads everything from the first changed byte at
//!   full price. On the workloads measured here the billed prompt is 90–100%
//!   cache reads, so this turns the cheapest part of a request into the most
//!   expensive one.
//! * **Wrongly `Warm`** — a saving goes unclaimed. Nothing breaks and nothing
//!   costs more than it did before this module existed.
//!
//! So this never *infers* cold. It reports cold only on evidence the proxy has
//! already seen with its own eyes: the provider told us, for this exact prefix,
//! that it read nothing from cache.
//!
//! # What identifies a conversation
//!
//! The learned opening of the conversation, fingerprinted inside its resolved
//! tenant, upstream, API-family and model namespace. Not a session header:
//! those are client-specific (`x-claude-code-session-id`, `prompt_cache_key`,
//! and whatever the next client invents), and a proxy that only works for
//! clients it has heard of is the failure the agent catalogue is written
//! against. The opening identifies the conversation while the namespace keeps
//! independent callers and provider/model routes from sharing its optimization
//! state.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use kompressor_engine::gate::GatePin;
use kompressor_engine::messages::{CachePricing, PrefixCache};
use kompressor_engine::refs::RefId;
use kompressor_wire::ApiFamily;

/// How many distinct prefixes to remember.
///
/// Bounded because a busy proxy sees unboundedly many conversations and this
/// is an optimisation, not a ledger: forgetting one costs a `Warm` answer,
/// which is the safe one. Evicting the oldest keeps the working set of an
/// active session while letting yesterday's fall out.
pub const REMEMBER: usize = 4096;

/// How many opening messages the identity chain covers.
///
/// Bounds two things at once: the hashing done per request, and how much
/// boilerplate two conversations may share before they are told apart. A
/// conversation is recognised by what it OPENED with, so this never needs to
/// reach the end of a long history.
const IDENTITY_DEPTH: usize = 64;

/// What a conversation's opening prefix maps to.
#[derive(Clone, Copy, Debug)]
struct Identity {
    id: u64,
    at: Instant,
}

/// Conversation identities, learned rather than derived.
///
/// Keyed by `(length, chain hash at that length)`: the resolved namespace and
/// exact content of the first `length` messages. `lengths` refcounts which
/// lengths are registered, so a lookup probes only those rather than every
/// depth.
#[derive(Debug, Default)]
struct Identities {
    by_prefix: HashMap<(usize, u64), Identity>,
    lengths: BTreeMap<usize, usize>,
    /// The longest history each conversation has sent, as its identity
    /// chain. What [`PrefixTracker::identify`] checks a match against: a
    /// request that departs from it is another conversation that opened the
    /// same way.
    history: HashMap<u64, (Vec<u64>, Instant)>,
    /// How many conversations have been told apart by [`Self::reopening`];
    /// mixed into the identity each one gets, since its opening hash is
    /// already the identity of the conversation it opened like.
    reopened: u64,
}

/// How far from the end of a conversation's history a request may differ and
/// still be that conversation.
///
/// A later turn appends, so a departure is almost always another conversation.
/// Measured on the 89-task gate run, comparing message text the way the chain
/// does: one request in 958 rewrote its own history, two messages from the
/// end. Three attempts of one Harbor task, by contrast, open identically and
/// part after their second message, which merged them into one conversation
/// that shared a cache-lifetime flag, a freeze watermark and a fetch history.
const TAIL_REWRITE: usize = 2;

impl Identities {
    /// Note `chain` as `id`'s history when it extends, or rewrites the tail
    /// of, the longest one seen; a shorter replay of it changes nothing.
    fn extend(&mut self, id: u64, chain: &[u64]) {
        if self.history.len() >= REMEMBER
            && !self.history.contains_key(&id)
            && let Some(&oldest) = self
                .history
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(id, _)| id)
        {
            self.history.remove(&oldest);
        }
        let entry = self
            .history
            .entry(id)
            .or_insert_with(|| (Vec::new(), Instant::now()));
        if chain.len() >= entry.0.len() {
            entry.0 = chain.to_vec();
        }
        entry.1 = Instant::now();
    }

    /// Where `chain` parts from `id`'s history, when it parts too early to be
    /// a rewrite of its tail. `None` for an extension, a replay of an earlier
    /// turn, a tail rewrite, or a conversation with no history on record.
    fn departure(&self, id: u64, chain: &[u64]) -> Option<usize> {
        let (history, _) = self.history.get(&id)?;
        let common = history
            .iter()
            .zip(chain)
            .take_while(|(a, b)| a == b)
            .count();
        (common < chain.len() && common + TAIL_REWRITE < history.len()).then_some(common)
    }

    /// Where to re-anchor `id` when `chain` replays its history from the
    /// start but stops well short of it: another conversation that opened
    /// the same way and has not parted yet.
    ///
    /// A later turn appends, and a retry or tail rewrite reaches at most
    /// [`TAIL_REWRITE`] messages back, so a request that is only the opening
    /// of a long history is not that conversation's next turn. Measured on a
    /// 164-minute, 18-trial Harbor run: every one of the 16 requests that
    /// replayed a strict prefix of a known history was the first or second
    /// request of a fresh attempt at the same task, none a replay inside a
    /// running session. Filed under the earlier attempt, each one read as a
    /// cache loss of everything that attempt had cached (seven false
    /// warnings, 129k tokens), switched `cache_ttl = "adaptive"` to one-hour
    /// writes on the strength of the earlier attempt's idle gap, and moved
    /// its keep-warm target.
    ///
    /// The depth returned keeps [`TAIL_REWRITE`] in hand, so the earlier
    /// conversation is still recognised after rewriting its tail.
    fn reopening(&self, id: u64, chain: &[u64]) -> Option<(usize, u64)> {
        let (history, _) = self.history.get(&id)?;
        let replays = chain.len() <= history.len() && history[..chain.len()] == *chain;
        let depth = history.len() - TAIL_REWRITE.min(history.len());
        (replays && chain.len() < depth).then(|| (depth, history[depth - 1]))
    }

    fn insert(&mut self, len: usize, hash: u64, id: u64) {
        if self.by_prefix.len() >= REMEMBER
            && let Some((&key, _)) = self.by_prefix.iter().min_by_key(|(_, entry)| entry.at)
        {
            self.by_prefix.remove(&key);
            if let Some(count) = self.lengths.get_mut(&key.0) {
                *count -= 1;
                if *count == 0 {
                    self.lengths.remove(&key.0);
                }
            }
        }
        if self
            .by_prefix
            .insert(
                (len, hash),
                Identity {
                    id,
                    at: Instant::now(),
                },
            )
            .is_none()
        {
            *self.lengths.entry(len).or_insert(0) += 1;
        }
    }
}

/// What the last request on one prefix turned out to cost.
#[derive(Clone, Copy, Debug)]
struct Seen {
    at: Instant,
    /// Cache-read tokens the provider reported. `None` when the response
    /// carried no usage at all, which is not evidence of anything.
    cache_read: Option<u32>,
    /// Whether this prefix has ever been served from cache.
    ///
    /// Without it, zero reads cannot be told apart from a prefix that was never
    /// cached in the first place — and the first turn of every conversation
    /// reads zero by construction. Sticky once true: a prefix that was warm
    /// stays capable of having lapsed.
    was_warm: bool,
    /// How far the provider's cache reached when this proxy first saw the
    /// conversation — see [`PrefixTracker::floor`]. `None` until asked.
    floor: Option<usize>,
    /// Retrieval rounds the turn that filed this entry spent.
    ///
    /// Kept only to suppress a false alarm. A retrieve turn sums usage across
    /// its two upstream calls, so its `cache_read` is inflated and the *next*
    /// turn appears to lose cache. That is accounting, not churn — it has been
    /// misread as churn twice — so a drop following a retrieve turn is not
    /// reported.
    rounds: u32,
    /// Whether this conversation has ever sat idle past the provider's
    /// five-minute cache TTL. Sticky: an agent that waited on a ten-minute
    /// test run once is the kind that does it again. What `CacheTtl::Adaptive`
    /// reads.
    idled_past_ttl: bool,
    /// The most messages a request under this identity has carried — what
    /// [`PrefixTracker::long_ttl`] reads to tell a conversation coming back
    /// from a pause from a new one that opens with the same bytes.
    most_messages: usize,
    /// Whether the response behind `cache_read` also wrote to the cache.
    ///
    /// A zero read that came with a write is a miss the provider has already
    /// repaired: it re-created the entry from the bytes this proxy just sent,
    /// so the next turn is served from cache again — and lifting the freeze
    /// would rewrite exactly the bytes it is about to read. Only a zero read
    /// with no write leaves nothing cached to protect. Anthropic reports
    /// writes; OpenAI does not, so this stays false there and its zero reads
    /// keep meaning what they did.
    rewritten: bool,
    /// How much of the conversation's first request the provider held in its
    /// cache once it had answered: what it read plus what it wrote. See
    /// [`PrefixTracker::opened_with`]. `None` until that answer arrives.
    opened_with: Option<u32>,
    /// Whether a turn of this conversation has gone upstream compressed.
    /// Sticky. What `Stage::AfterCompressed` asks; nothing decides on it.
    compressed: bool,
}

/// Which conversation, under which model, last sent one history.
#[derive(Clone, Debug)]
struct Lineage {
    fingerprint: u64,
    model: String,
    at: Instant,
}

/// Remembers, per frozen prefix, whether the provider last served it from cache.
#[derive(Debug)]
pub struct PrefixTracker {
    /// Billed prompt tokens and the part of them served from cache, summed
    /// over every request this process has seen usage for.
    ///
    /// The engine prices a prefix rewrite against what a retrieve round costs,
    /// and a round is the whole prompt at the *blended* rate — mostly cached,
    /// but paying the fresh rate on whatever is not. That blend is a property
    /// of the traffic, not of the code, and assuming it away is what made the
    /// first version of the rule ask for 35% more demands than the economics
    /// required. Measured rather than assumed, and process-wide rather than
    /// per-conversation: it is a rate, and the two conversations in a run are
    /// the same workload against the same provider.
    ///
    /// Cache reads, the whole billed prompt, and the part of it the provider
    /// billed as a cache *write* — which only Anthropic reports, and which
    /// prices a rewrite above a plain miss.
    billed: Mutex<(u64, u64, u64)>,
    seen: Mutex<HashMap<u64, Seen>>,
    /// Openings already seen, so a later turn is recognised as the same
    /// conversation rather than hashed into a new one.
    ids: Mutex<Identities>,
    /// The conversation and model that last sent each history, keyed by that
    /// history's identity chain WITHOUT the model in scope. What
    /// [`Self::floor_for`] reads to recognise one conversation carried on
    /// under another model. Bounded like `seen`.
    lineages: Mutex<HashMap<u64, Lineage>>,
    /// Handles the model has fetched back, per conversation.
    ///
    /// Bounded twice over: at most [`RETRIEVED_PER_CONVERSATION`] handles for
    /// one conversation and [`RETRIEVED_CONVERSATIONS`] conversations in all,
    /// because this is keyed by something a caller controls and an unbounded
    /// map keyed by caller input is a memory leak with extra steps.
    retrieved: Mutex<HashMap<u64, HashMap<RefId, Fetched>>>,
    /// Client requests seen per conversation, so a fetch can be placed in time.
    /// Bounded like `retrieved`, and for the same reason.
    turns: Mutex<HashMap<u64, u64>>,
    /// The whole billed prompt of each conversation's last answered request,
    /// per upstream call. What the net-cost gate prices a retrieve round at.
    /// Bounded like `retrieved`.
    prompts: Mutex<HashMap<u64, u32>>,
    /// The net-cost gate's pins, per conversation, with when each
    /// conversation last pinned anything. Bounded like `retrieved`, and per
    /// conversation by [`GATE_PINS_PER_CONVERSATION`].
    gate_pins: Mutex<HashMap<u64, PinnedGate>>,
    /// Extra idle time per conversation, set only by
    /// [`PrefixTracker::age_for_test`]. Empty outside tests.
    aged: Mutex<HashMap<u64, Duration>>,
    /// How far this tracker's clock runs ahead of the machine's, in
    /// milliseconds. Moved only by [`PrefixTracker::advance_clock_for_test`];
    /// zero outside tests.
    ahead_ms: std::sync::atomic::AtomicU64,
    /// Age past which a prefix is assumed lapsed. Zero disables the age rule
    /// and leaves only observed evidence, which is the shipped default: a TTL
    /// is a guess about someone else's eviction policy, and guessing `Cold` is
    /// the expensive mistake.
    ttl: Duration,
}

/// One handle the model has fetched back, and whether it has earned its place
/// inline yet.
///
/// # Why one retrieval is not enough
///
/// The cross-turn loop this guards against has a signature: the *same* handle
/// fetched in *different* requests, because the server-side round that answers
/// a retrieval never reaches the client's history, so the next turn elides the
/// same bytes again and the model asks again.
///
/// A single retrieval does not match that signature, and acting on it is a bet
/// that the fragment will be wanted again. Measured on a Harbor run
/// (2026-10-02, 39 requests): all 11 fetched handles were fetched in exactly
/// one request each, and keeping them inline anyway cost 503,355 tokens —
/// **24% of everything the engine looked at** — for nothing. Waiting for a
/// second request costs one extra round on a handle that really is looping,
/// and costs nothing at all on the common case.
#[derive(Clone, Debug)]
struct Fetched {
    /// The request the first fetch happened in. A fetch from any *other*
    /// request is the second data point, and the one that proves a loop.
    first_request: String,
    /// Whether the engine should now keep this fragment's content inline.
    promoted: bool,
    /// How many *distinct requests* have fetched this handle.
    ///
    /// The engine prices a promotion against the prefix it rewrites, and that
    /// trade turns on how often the fragment would otherwise be fetched again.
    /// That number used to be unobservable: a promoted fragment is never asked
    /// for again, so nothing reveals what it would have cost. **Refusing a
    /// promotion is what makes it observable** — a handle the model still
    /// wants announces itself, once per round, until it is promoted.
    ///
    /// Counted per request and not per retrieval, for the same reason
    /// `promoted` is: a model narrowing twice inside one request is one
    /// demand, not two.
    fetches: u32,
    /// Whether the engine has already shipped this fragment's message whole.
    ///
    /// Once it has, the decision is not re-taken. The bar it was weighed
    /// against moves — the prompt grows behind the message, and the measured
    /// cache hit rate converges as a run proceeds — so a fragment promoted on
    /// one turn can be refused on the next, re-eliding a message the provider
    /// has cached whole and losing the prefix a second time. Measured
    /// 2026-10-08: promoted while the running rate read 82%, demoted at 94%,
    /// 8,192 tokens. The rewrite is paid once, so the decision is taken once.
    inlined: bool,
    /// The conversation turns that fetched this handle, most recent last and
    /// at most `DEMAND_WINDOW` of them. What [`PrefixTracker::demand`]
    /// counts.
    recent: Vec<u64>,
}

/// Did the provider serve less of this prefix than it did last turn?
///
/// Within one conversation the prefix only grows, so the reported cache read
/// should never fall. When it does, the provider served less of it than last
/// turn. That is an observation, not an attribution or a price: expiry,
/// eviction, a changed request structure (tools, system, breakpoints) and our
/// own rewrite all read the same here, and what the missing part costs depends
/// on whether it was re-written to cache, at which TTL. `cache-loss-diff.py`
/// and the cache report are what attribute it.
///
/// A retrieve turn is excluded on both sides. It sums usage across its two
/// upstream calls, so its own figure is inflated and the turn after it appears
/// to lose cache — accounting, not churn, and misread as churn twice before it
/// was understood.
fn is_a_cache_loss(before: u32, now: u32, prior_rounds: u32, rounds: u32) -> bool {
    now < before && prior_rounds == 0 && rounds == 0
}

/// Most handles one conversation may hold inline. A conversation that has
/// fetched back this many fragments is not being helped by compression.
pub const RETRIEVED_PER_CONVERSATION: usize = 64;
/// Most conversations the retrieved map tracks at once.
pub const RETRIEVED_CONVERSATIONS: usize = 1024;
/// One conversation's net-cost gate pins, with when it last pinned anything.
type PinnedGate = (Instant, BTreeMap<u64, GatePin>);

/// Most messages one conversation may hold a net-cost gate pin for. Past it a
/// new message is decided against the current prompt on every turn, which can
/// move its bytes; a conversation with this many handle-bearing tool results
/// has not been seen.
const GATE_PINS_PER_CONVERSATION: usize = 4_096;
/// A provider's prices for one model, in millionths of its uncached input
/// price: a cache read, and whatever a token the cache does not hold is billed
/// as on the way in — a cache write where the provider charges for one,
/// uncached input where it does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CachePrices {
    pub read_ppm: u32,
    pub write_ppm: u32,
}

impl CachePrices {
    /// The published prices for `model` on `family`'s provider, with
    /// `long_ttl` when this request asks for one-hour cache entries.
    ///
    /// Read prices that differ from the usual tenth are named: Claude Opus 5.5
    /// reads at $0.20 against $4.00 input, Claude Sonnet 5.5 at $0.10 against
    /// $2.00, Claude Fable 5.1 and Mythos 5.1 at $0.25 against $10.00. Anthropic
    /// writes at 1.25x input for five-minute entries and 2x for one-hour ones.
    /// OpenAI caches without a write charge, so a miss there is uncached input.
    ///
    /// A model routed as `anthropic/<id>` is priced as `<id>`: the archive's
    /// stats carry both spellings of the same models.
    pub fn for_model(family: ApiFamily, model: Option<&str>, long_ttl: bool) -> CachePrices {
        if family != ApiFamily::AnthropicMessages {
            return CachePrices {
                read_ppm: 100_000,
                write_ppm: 1_000_000,
            };
        }
        let model = model.unwrap_or_default();
        let model = model.strip_prefix("anthropic/").unwrap_or(model);
        let read_ppm = if model.starts_with("claude-opus-5-5")
            || model.starts_with("claude-sonnet-5-5")
        {
            50_000
        } else if model.starts_with("claude-fable-5-1") || model.starts_with("claude-mythos-5-1") {
            25_000
        } else {
            100_000
        };
        CachePrices {
            read_ppm,
            write_ppm: if long_ttl { 2_000_000 } else { 1_250_000 },
        }
    }
}

/// Anthropic's default cache lifetime, which `CacheTtl::Adaptive` watches for.
const PROVIDER_CACHE_TTL: Duration = Duration::from_secs(300);
/// How many of a conversation's latest requests count as current demand.
///
/// Replayed over every promotion decision in two Harbor runs (93 decisions,
/// 18 fragment groups): no decision taken five or more requests after the
/// fragment's last fetch was ever followed by another fetch (0 of 45), against
/// 11 of 37 taken right after one. Windows of three to six requests decided
/// all 18 groups identically; four is the middle of that range.
const DEMAND_WINDOW: u32 = 4;

impl PrefixTracker {
    pub fn new(ttl: Duration) -> PrefixTracker {
        PrefixTracker {
            billed: Mutex::new((0, 0, 0)),
            seen: Mutex::new(HashMap::new()),
            ids: Mutex::new(Identities::default()),
            lineages: Mutex::new(HashMap::new()),
            retrieved: Mutex::new(HashMap::new()),
            turns: Mutex::new(HashMap::new()),
            prompts: Mutex::new(HashMap::new()),
            gate_pins: Mutex::new(HashMap::new()),
            aged: Mutex::new(HashMap::new()),
            ahead_ms: std::sync::atomic::AtomicU64::new(0),
            ttl,
        }
    }

    /// The time every idle measurement here is taken against: the machine's
    /// clock, plus whatever a replay has moved this tracker's ahead by.
    fn now(&self) -> Instant {
        Instant::now()
            + Duration::from_millis(self.ahead_ms.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// How long ago `at` — a time this tracker stamped — was, on its clock.
    fn since(&self, at: Instant) -> Duration {
        self.now().saturating_duration_since(at)
    }

    /// Move this tracker's clock forward by `by`, for every conversation.
    ///
    /// For a replay, which plays hours of captured traffic in seconds: the
    /// provider it simulates expires entries on the capture's clock, and a
    /// proxy left on the machine's would never see the pauses that expired
    /// them — so the adaptive TTL would never switch and no lapse would ever
    /// be predicted, whatever the settings under test. Nothing in the proxy
    /// calls it.
    #[doc(hidden)]
    pub fn advance_clock_for_test(&self, by: Duration) {
        self.ahead_ms.fetch_add(
            u64::try_from(by.as_millis()).unwrap_or(u64::MAX),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Is the prefix this request froze still being served from cache?
    ///
    /// `Warm` whenever there is no evidence — a prefix never seen before, a
    /// previous response that reported no usage, a poisoned lock.
    pub fn state(&self, fingerprint: u64) -> PrefixCache {
        let Ok(seen) = self.seen.lock() else {
            return PrefixCache::Warm;
        };
        let Some(entry) = seen.get(&fingerprint) else {
            // First time. The provider may well have this cached from a
            // request that did not come through us.
            return PrefixCache::Warm;
        };
        // Zero reads mean "lapsed" only for a prefix that was once served from
        // cache. On a conversation's FIRST turn the provider reads nothing
        // because nothing is cached yet — it is busy *creating* the entry — and
        // reading that as a lapse inverts the feature: the next turn lifts the
        // whole freeze and rewrites the prefix that was just written.
        //
        // Measured on a Claude Code subscription session. Turn one reported
        // `cache_read = 0` alongside `cache_creation = 22,363`, the tracker
        // said `Cold`, turn three ran with `frozen_messages = 0` and the
        // provider served only 14,176 of the 22,363 it held — 8,187 tokens
        // re-read at full price. Turn four went back to `frozen_messages = 2`
        // the moment a nonzero read was observed, which is what showed this to
        // be the per-turn cache state rather than the monotonic watermark.
        if !entry.was_warm {
            return PrefixCache::Warm;
        }
        // The provider's own answer, and the only signal that is evidence
        // rather than inference: it was asked to serve this prefix and read
        // nothing from cache — and did not cache it again either. A zero read
        // beside a write is a lapse already repaired; see `Seen::rewritten`.
        if entry.cache_read == Some(0) && !entry.rewritten {
            return PrefixCache::Cold;
        }
        if !self.ttl.is_zero() && self.since(entry.at) > self.ttl {
            return PrefixCache::Cold;
        }
        PrefixCache::Warm
    }

    /// Record what the provider reported for this prefix.
    ///
    /// `prompt_tokens` is the *whole* billed prompt, cached part included. On
    /// OpenAI that is the reported `prompt_tokens`; on Anthropic the reported
    /// `input_tokens` is only the uncached remainder, and the caller must add
    /// the cache reads and writes back. Passed raw, it made every Anthropic
    /// run read as 1000 — 41,116 cached over 8 "prompt" tokens — so the engine
    /// priced every promotion at the most demanding bar there is.
    pub fn record(
        &self,
        fingerprint: u64,
        cache_read: Option<u32>,
        prompt_tokens: Option<u32>,
        rounds: u32,
    ) {
        // Both or neither: a prompt total without its cached part would read as
        // a cache miss and drag the measured rate down.
        if let (Some(read), Some(prompt)) = (cache_read, prompt_tokens)
            && prompt > 0
            && let Ok(mut billed) = self.billed.lock()
        {
            billed.0 = billed.0.saturating_add(u64::from(read));
            billed.1 = billed.1.saturating_add(u64::from(prompt));
        }
        let Ok(mut seen) = self.seen.lock() else {
            return;
        };
        if seen.len() >= REMEMBER && !seen.contains_key(&fingerprint) {
            // Oldest out. A scan rather than a real LRU: this runs once per
            // request at the cap, over at most `REMEMBER` entries, against a
            // network round trip.
            if let Some(oldest) = seen
                .iter()
                .min_by_key(|(_, entry)| entry.at)
                .map(|(key, _)| *key)
            {
                seen.remove(&oldest);
            }
        }
        // `None` means the response carried no usage, which is not evidence —
        // and must not *erase* evidence either. One request can finish more
        // than once here (an abandoned stream, an error path), and the later
        // call carries no usage: left to overwrite, it wiped the `Some(0)` the
        // real response had just recorded and the next turn saw nothing known.
        // Measured: the cold path never fired because of exactly this.
        //
        // The timestamp still advances, because the prefix *was* used just now
        // and the age rule should say so.
        let known =
            cache_read.or_else(|| seen.get(&fingerprint).and_then(|prior| prior.cache_read));
        // Sticky: one served read is enough to establish that this prefix can
        // be cached at all, which is what makes a later zero meaningful.
        let was_warm = known.is_some_and(|read| read > 0)
            || seen.get(&fingerprint).is_some_and(|prior| prior.was_warm);
        // The watermark is set at request time and must survive the response
        // that follows it; this insert would otherwise wipe it every turn.
        let floor = seen.get(&fingerprint).and_then(|prior| prior.floor);
        let idled_past_ttl = seen
            .get(&fingerprint)
            .is_some_and(|prior| prior.idled_past_ttl);
        let most_messages = seen
            .get(&fingerprint)
            .map_or(0, |prior| prior.most_messages);
        // Like `known`: a usage-less finish says nothing about writes either.
        let rewritten =
            cache_read.is_none() && seen.get(&fingerprint).is_some_and(|prior| prior.rewritten);
        let opened_with = seen.get(&fingerprint).and_then(|prior| prior.opened_with);
        let compressed = seen.get(&fingerprint).is_some_and(|prior| prior.compressed);

        // Within one conversation the prefix only grows, so the cache read the
        // provider reports should never fall. When it does, something outside
        // this proxy dropped the prefix, and the next turn pays full price for
        // everything after the first missing byte.
        //
        // Said out loud with its context, because the alternative is
        // reconstructing it from a stats dump afterwards. Observed three times
        // across sixteen runs and growing: -9,216 on a build that predates the
        // retrieve work, -14,336 later, and once a *total* loss — 29,056 to
        // zero on a prefix that had grown by 117 tokens, recovering completely
        // on the very next turn. That shape is provider-side eviction rather
        // than a rewrite of ours, which would not recover in one step; this
        // line exists so the next occurrence carries its own evidence.
        if let Some(prior) = seen.get(&fingerprint)
            && let (Some(before), Some(now)) = (prior.cache_read, known)
            && is_a_cache_loss(before, now, prior.rounds, rounds)
        {
            tracing::warn!(
                conversation = format!("{fingerprint:016x}"),
                was = before,
                now,
                lost = before - now,
                since_previous_ms = self.since(prior.at).as_millis() as u64,
                "the provider served less of this prefix from cache than last turn \
                         (observed, not attributed: expiry, eviction, a changed request \
                         and a rewrite all read the same here)"
            );
        }
        seen.insert(
            fingerprint,
            Seen {
                at: self.now(),
                cache_read: known,
                floor,
                was_warm,
                rounds,
                idled_past_ttl,
                most_messages,
                rewritten,
                opened_with,
                compressed,
            },
        );
    }

    /// Record how much of the conversation's first request the provider held
    /// in its cache once it had answered it: `cached` is what it read plus
    /// what it wrote. Called beside [`PrefixTracker::record`], and only for
    /// the request that opened the conversation; the first figure filed
    /// stands.
    pub fn record_opening(&self, fingerprint: u64, cached: u32) {
        if let Ok(mut seen) = self.seen.lock()
            && let Some(entry) = seen.get_mut(&fingerprint)
            && entry.opened_with.is_none()
        {
            entry.opened_with = Some(cached);
        }
    }

    /// How many tokens of the conversation's first request the provider held
    /// in its cache after answering it. `None` for a conversation not seen
    /// before, or whose first answer has not arrived.
    ///
    /// That much is not this conversation's alone. Another conversation that
    /// opens with the same bytes — a second run of the same task, with the
    /// same instructions, tools and first message — reads and renews the
    /// same entry, so the provider can go on serving it after everything
    /// this conversation wrote later has lapsed. A read no longer than this
    /// is therefore no sign that this conversation's cache was alive.
    pub fn opened_with(&self, fingerprint: u64) -> Option<u32> {
        let seen = self.seen.lock().ok()?;
        seen.get(&fingerprint)?.opened_with
    }

    /// Record that the request just filed by [`PrefixTracker::record`] went
    /// upstream compressed. Called beside it.
    pub fn record_compressed(&self, fingerprint: u64) {
        if let Ok(mut seen) = self.seen.lock()
            && let Some(entry) = seen.get_mut(&fingerprint)
        {
            entry.compressed = true;
        }
    }

    /// Whether an earlier turn of this conversation went upstream compressed.
    pub fn was_compressed(&self, fingerprint: u64) -> bool {
        self.seen
            .lock()
            .map(|seen| seen.get(&fingerprint).is_some_and(|entry| entry.compressed))
            .unwrap_or(false)
    }

    /// Record that the response just filed by [`PrefixTracker::record`] also
    /// wrote this prefix to the cache. Called beside it, for the same usage.
    pub fn record_rewritten(&self, fingerprint: u64) {
        if let Ok(mut seen) = self.seen.lock()
            && let Some(entry) = seen.get_mut(&fingerprint)
        {
            entry.rewritten = true;
        }
    }
}

impl PrefixTracker {
    /// Record that `ids` were fetched back in `conversation`.
    ///
    /// Called by the retrieve loop after a hit. From this point the engine
    /// stops eliding that content for this conversation: see
    /// [`RequestOptions::keep_inline`](kompressor_engine::messages::RequestOptions::keep_inline)
    /// for why a retrieval is the signal that eliding it was a bad trade.
    pub fn record_retrieved(&self, conversation: u64, request_id: &str, ids: &[RefId]) {
        if ids.is_empty() {
            return;
        }
        let Ok(mut map) = self.retrieved.lock() else {
            return;
        };
        // Evicting a whole conversation only costs it its inline-keeping, so
        // an arbitrary victim is acceptable where a leak is not.
        if map.len() >= RETRIEVED_CONVERSATIONS
            && !map.contains_key(&conversation)
            && let Some(&victim) = map.keys().next()
        {
            map.remove(&victim);
        }
        let turn = self.turn(conversation);
        let held = map.entry(conversation).or_default();
        for id in ids {
            match held.get_mut(id) {
                // Seen before, in a different request: that is the loop, and
                // from here the engine stops eliding it.
                Some(fetched) => {
                    if fetched.first_request != request_id {
                        fetched.promoted = true;
                        fetched.fetches = fetched.fetches.saturating_add(1);
                    }
                    if fetched.recent.last() != Some(&turn) {
                        fetched.recent.push(turn);
                        if fetched.recent.len() > DEMAND_WINDOW as usize {
                            fetched.recent.remove(0);
                        }
                    }
                }
                // First sight. Recorded, not acted on: a model narrowing twice
                // inside one request is not a loop, and charging the whole
                // fragment to every later turn for it is how the first version
                // of this spent 24% of the engine's input for nothing.
                None => {
                    if held.len() < RETRIEVED_PER_CONVERSATION {
                        held.insert(
                            *id,
                            Fetched {
                                first_request: request_id.to_string(),
                                promoted: false,
                                fetches: 1,
                                inlined: false,
                                recent: vec![turn],
                            },
                        );
                    }
                }
            }
        }
    }

    /// Promote `ids` because this turn appears to be asking about them.
    ///
    /// The other route to promotion is evidence after the fact — the same
    /// handle fetched in two different requests. This one is evidence before
    /// it: the marker is in the request the model is answering now, and the
    /// text of that turn is about the fragment behind it. It exists because a
    /// relayed websocket has no retrieve loop, so a marker the model cannot
    /// resolve is content it has simply lost.
    ///
    /// Promotes on sight rather than on a second sighting, because there will
    /// be no second sighting: nothing on that transport can ask.
    pub fn record_wanted(&self, conversation: u64, ids: &[RefId]) {
        if ids.is_empty() {
            return;
        }
        let Ok(mut map) = self.retrieved.lock() else {
            return;
        };
        if map.len() >= RETRIEVED_CONVERSATIONS
            && !map.contains_key(&conversation)
            && let Some(&victim) = map.keys().next()
        {
            map.remove(&victim);
        }
        let held = map.entry(conversation).or_default();
        for id in ids {
            match held.get_mut(id) {
                Some(fetched) => fetched.promoted = true,
                None => {
                    if held.len() < RETRIEVED_PER_CONVERSATION {
                        held.insert(
                            *id,
                            Fetched {
                                // No request owns this: it was not fetched.
                                first_request: String::new(),
                                promoted: true,
                                // Nor counted as a demand. This route promotes
                                // because the transport cannot ask, so there is
                                // no fetch to count and the engine must not
                                // price it as though a round had been spent.
                                fetches: 0,
                                inlined: false,
                                recent: Vec::new(),
                            },
                        );
                    }
                }
            }
        }
    }

    /// Handles already fetched back in `conversation`, for the engine to keep
    /// inline. Empty for a conversation that has never retrieved anything,
    /// which is almost all of them.
    pub fn retrieved(&self, conversation: Option<u64>) -> Vec<RefId> {
        self.retrieved_with_counts(conversation)
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    /// What share of billed prompt tokens this provider has served from cache,
    /// in parts per thousand.
    ///
    /// 1000 — everything cached — until there is evidence, and that default is
    /// the conservative one: it makes a retrieve round look as cheap as it can
    /// be, so the engine demands the most before rewriting a prefix. Guessing
    /// the other way would promote on thin evidence, which is the mistake that
    /// cost 46,379 tokens of cache in one turn.
    ///
    /// The rate is what was measured, for reporting. Pricing a promotion needs
    /// more than it — the cost of a write, and what this model charges for a
    /// read — and [`PrefixTracker::promotion_pricing`] carries both.
    pub fn cache_hit_per_mille(&self) -> u32 {
        let Ok(billed) = self.billed.lock() else {
            return 1000;
        };
        let (read, prompt, _) = *billed;
        if prompt == 0 {
            return 1000;
        }
        (read.min(prompt) * 1000 / prompt) as u32
    }

    /// What a promotion trades at `prices`, on the traffic this process has
    /// billed so far: rewriting one token, and one prompt token of a retrieve
    /// round in the measured blend of reads, writes and uncached input.
    ///
    /// Before any usage the round is priced as all reads, the cheapest a round
    /// can be and so the most demanding bar — the same conservative reading
    /// the hit rate has always defaulted to.
    pub fn promotion_pricing(&self, prices: CachePrices) -> CachePricing {
        let (read, prompt, written) = self.billed.lock().map(|billed| *billed).unwrap_or_default();
        let round = if prompt == 0 {
            u64::from(prices.read_ppm)
        } else {
            let read = read.min(prompt);
            let written = written.min(prompt - read);
            let fresh = prompt - read - written;
            (read * u64::from(prices.read_ppm)
                + written * u64::from(prices.write_ppm)
                + fresh * 1_000_000)
                / prompt
        };
        CachePricing {
            rewrite_ppm: prices.write_ppm.saturating_sub(prices.read_ppm),
            round_ppm: u32::try_from(round).unwrap_or(u32::MAX).max(1),
        }
    }

    /// Record the part of a billed prompt the provider charged as a cache
    /// write. Called beside [`PrefixTracker::record`], for the same usage, and
    /// only when that call counted it.
    pub fn record_cache_writes(&self, written: u32) {
        if let Ok(mut billed) = self.billed.lock() {
            billed.2 = billed.2.saturating_add(u64::from(written));
        }
    }

    /// Whether this request should ask for one-hour cache entries under
    /// `CacheTtl::Adaptive`: the conversation has gone quiet past the
    /// provider's five-minute TTL, on this request or an earlier one.
    ///
    /// Measured from the previous request's end, which undercounts the
    /// start-to-start gap the provider times entries by. Noticing a gap late
    /// costs one expiry; inventing one costs a doubled write price on every
    /// later turn.
    ///
    /// The request that finds the gap already answers `true`. Its prefix has
    /// expired, so it rewrites it whatever it asks for, and asking for an hour
    /// costs 2x on that one write. Waiting a request instead costs the whole
    /// prefix twice: a request asking for an hour does not read the five-minute
    /// entry its predecessor wrote. Measured on the 89-task tuned run, all 11
    /// conversations that switched one request late rewrote the prefix again
    /// 2-43 s after the first rewrite, 208K tokens at the one-hour price.
    ///
    /// Only a request that carries more messages than any before it under this
    /// identity can find the gap. One that does not is a new agent opening
    /// with the same bytes, not this conversation coming back: Claude Code
    /// opens every trial of a task with an identical two-message request, so
    /// those share an identity however far apart they start. Measured on the
    /// 2026-11-09 keep-warm run: a crack-7z trial opening 36 minutes after the
    /// previous one was switched to an hour, and because one-hour requests do
    /// not read five-minute entries it missed the 13,690-token system and
    /// tools prefix every task had kept warm, then wrote 17,401 tokens twice
    /// at the one-hour price.
    pub fn long_ttl(&self, fingerprint: u64, messages: usize) -> bool {
        let Ok(mut seen) = self.seen.lock() else {
            return false;
        };
        let Some(entry) = seen.get_mut(&fingerprint) else {
            return false;
        };
        let continues = messages > entry.most_messages;
        entry.most_messages = entry.most_messages.max(messages);
        if continues && self.since(entry.at) > PROVIDER_CACHE_TTL {
            entry.idled_past_ttl = true;
        }
        entry.idled_past_ttl
    }

    /// How long the conversation has been idle — since its last response or
    /// its last keep-warm read — whether the provider has ever served it
    /// from cache, and whether a request of `messages` messages continues it:
    /// carries more than any before it. `None` for a conversation not seen
    /// before.
    ///
    /// These are the inputs of [`Self::predicted_cold`], read as it reads
    /// them. Read-only, and for the record only: what the next response
    /// reports is scored against them to learn how well a lapse can be
    /// predicted on the request that finds it. Nothing decides what to send
    /// from this.
    pub fn idle(&self, fingerprint: u64, messages: usize) -> Option<(Duration, bool, bool)> {
        let seen = self.seen.lock().ok()?;
        let entry = seen.get(&fingerprint)?;
        Some((
            self.since(entry.at),
            entry.was_warm,
            messages > entry.most_messages,
        ))
    }

    /// Whether this request is arriving after the conversation's cache entry
    /// must have expired: the provider has served it from cache before, the
    /// request continues it, and it has been idle — since its last response or
    /// keep-warm read — for longer than `lifetime`.
    ///
    /// An inference, where [`Self::state`] reports evidence. It is the only
    /// way to act on the turn that *finds* a lapse: the provider's own answer
    /// arrives with that turn's response, after its prefix has been written
    /// again. `lifetime` is the caller's — what the conversation's last
    /// request asked for, plus a margin — because a wrong `true` rewrites a
    /// live prefix. Read-only.
    ///
    /// `messages` must exceed what the conversation has carried: a request
    /// that does not is another conversation opening with the same bytes, as
    /// in [`Self::long_ttl`].
    pub fn predicted_cold(&self, fingerprint: u64, messages: usize, lifetime: Duration) -> bool {
        let Ok(seen) = self.seen.lock() else {
            return false;
        };
        let aged = self
            .aged
            .lock()
            .ok()
            .and_then(|aged| aged.get(&fingerprint).copied())
            .unwrap_or_default();
        seen.get(&fingerprint).is_some_and(|entry| {
            entry.was_warm
                && messages > entry.most_messages
                && self.since(entry.at) + aged > lifetime
        })
    }

    /// Have [`Self::predicted_cold`] read this conversation as idle for `by`
    /// longer than it has been. Zero undoes it.
    ///
    /// For tests, which cannot wait out a five-minute cache lifetime and
    /// cannot move an `Instant` backwards on a machine that booted a minute
    /// ago. Nothing in the proxy calls it.
    #[doc(hidden)]
    pub fn age_for_test(&self, fingerprint: u64, by: Duration) {
        if let Ok(mut aged) = self.aged.lock() {
            if by.is_zero() {
                aged.remove(&fingerprint);
            } else {
                aged.insert(fingerprint, by);
            }
        }
    }

    /// Record how many messages this conversation's request carried, for
    /// [`Self::predicted_cold`] to compare the next one with.
    pub fn note_messages(&self, fingerprint: u64, messages: usize) {
        if let Ok(mut seen) = self.seen.lock()
            && let Some(entry) = seen.get_mut(&fingerprint)
        {
            entry.most_messages = entry.most_messages.max(messages);
        }
    }

    /// Lower the conversation's freeze to nothing, for good.
    ///
    /// Called once a recompaction of the whole conversation has gone upstream:
    /// the provider now holds every message in its compressed form, so
    /// freezing any of them again would send the client's originals and miss.
    /// The floor may fall and never rises, so this holds from here on.
    pub fn lower_floor(&self, fingerprint: u64) {
        if let Ok(mut seen) = self.seen.lock()
            && let Some(entry) = seen.get_mut(&fingerprint)
        {
            entry.floor = Some(0);
        }
    }

    /// A keep-warm ping just read this conversation's prefix from cache.
    ///
    /// The prefix was used, so the clock that decides whether the conversation
    /// sat idle past the provider's TTL starts again. Without this a pause the
    /// pings bridged would still flip `CacheTtl::Adaptive` to one-hour entries,
    /// paying for the very lapse they prevented.
    pub fn kept_warm(&self, fingerprint: u64) {
        if let Ok(mut seen) = self.seen.lock()
            && let Some(entry) = seen.get_mut(&fingerprint)
        {
            entry.at = self.now();
        }
    }

    /// File the whole billed prompt of a conversation's answered request.
    ///
    /// A retrieve turn reports usage summed over its rounds, so the figure is
    /// divided by the upstream calls it took: what one round re-read, which is
    /// what the next fetch will cost again.
    pub fn record_prompt(&self, conversation: u64, prompt_tokens: u32, rounds: u32) {
        let Ok(mut prompts) = self.prompts.lock() else {
            return;
        };
        if prompts.len() >= RETRIEVED_CONVERSATIONS
            && !prompts.contains_key(&conversation)
            && let Some(evict) = prompts.keys().next().copied()
        {
            prompts.remove(&evict);
        }
        prompts.insert(conversation, prompt_tokens / rounds.saturating_add(1));
    }

    /// The prompt [`Self::record_prompt`] last filed for `conversation`.
    pub fn last_prompt(&self, conversation: u64) -> Option<u32> {
        self.prompts.lock().ok()?.get(&conversation).copied()
    }

    /// The net-cost gate's pins for `conversation`, to hand back to the engine.
    pub fn gate_pins(&self, conversation: Option<u64>) -> BTreeMap<u64, GatePin> {
        conversation
            .and_then(|key| {
                let pins = self.gate_pins.lock().ok()?;
                pins.get(&key).map(|(_, held)| held.clone())
            })
            .unwrap_or_default()
    }

    /// Pin what the gate decided for the first time. A pin already held is
    /// never replaced: the first decision is the one whose bytes went out.
    pub fn record_gate_pins(&self, conversation: Option<u64>, pins: &[(u64, GatePin)]) {
        let (Some(conversation), false) = (conversation, pins.is_empty()) else {
            return;
        };
        let now = self.now();
        let Ok(mut map) = self.gate_pins.lock() else {
            return;
        };
        if map.len() >= RETRIEVED_CONVERSATIONS
            && !map.contains_key(&conversation)
            && let Some(oldest) = map
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(key, _)| *key)
        {
            map.remove(&oldest);
        }
        let (at, held) = map
            .entry(conversation)
            .or_insert_with(|| (now, BTreeMap::new()));
        *at = now;
        for (key, pin) in pins {
            if held.len() >= GATE_PINS_PER_CONVERSATION {
                break;
            }
            held.entry(*key).or_insert(*pin);
        }
    }

    /// Pin fragments the engine has shipped whole, so the decision is not
    /// re-taken against a bar that has since moved.
    pub fn record_inlined(&self, conversation: Option<u64>, ids: &[RefId]) {
        if ids.is_empty() {
            return;
        }
        let Some(conversation) = conversation else {
            return;
        };
        let Ok(mut map) = self.retrieved.lock() else {
            return;
        };
        let Some(held) = map.get_mut(&conversation) else {
            return;
        };
        for id in ids {
            if let Some(fetched) = held.get_mut(id) {
                fetched.inlined = true;
            }
        }
    }

    /// Start a client request in `conversation` and return what the engine
    /// should keep inline for it, each handle with its **current** demand: the
    /// requests among the last `DEMAND_WINDOW` that fetched it.
    ///
    /// A lifetime count is the wrong predictor. A fragment fetched in four
    /// consecutive requests is being worked with; one fetched at requests 4, 9,
    /// 12 and 16 reaches the same count and is browsed. Priced on the lifetime
    /// count, the second is promoted and its rewrite — 69% of the prompt, beyond
    /// the cache lookback — buys two rounds that cost a fraction of it. A
    /// pinned promotion keeps `u32::MAX`, as it always has.
    pub fn demand(&self, conversation: Option<u64>) -> Vec<(RefId, u32)> {
        let Some(conversation) = conversation else {
            return Vec::new();
        };
        let now = self.advance_turn(conversation);
        let Ok(map) = self.retrieved.lock() else {
            return Vec::new();
        };
        map.get(&conversation)
            .map(|held| {
                held.iter()
                    .filter(|(_, fetched)| fetched.promoted)
                    .map(|(id, fetched)| {
                        let current = if fetched.inlined {
                            u32::MAX
                        } else {
                            let since = now.saturating_sub(u64::from(DEMAND_WINDOW));
                            fetched
                                .recent
                                .iter()
                                .filter(|&&turn| turn >= since && turn < now)
                                .count() as u32
                        };
                        (*id, current)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The turn `conversation` is on.
    fn turn(&self, conversation: u64) -> u64 {
        self.turns
            .lock()
            .ok()
            .and_then(|turns| turns.get(&conversation).copied())
            .unwrap_or(0)
    }

    /// Count one more client request in `conversation`, returning its turn.
    fn advance_turn(&self, conversation: u64) -> u64 {
        let Ok(mut turns) = self.turns.lock() else {
            return 0;
        };
        if turns.len() >= RETRIEVED_CONVERSATIONS
            && !turns.contains_key(&conversation)
            && let Some(&victim) = turns.keys().next()
        {
            turns.remove(&victim);
        }
        let turn = turns.entry(conversation).or_insert(0);
        *turn += 1;
        *turn
    }

    /// The same handles, each with how many distinct requests have asked for
    /// it. The engine weighs a promotion's one-off prefix rewrite against this.
    ///
    /// A fragment already shipped whole reports [`u32::MAX`] demands, which
    /// clears any bar: the prefix rewrite it would have paid for is already
    /// paid, and re-eliding it now would pay for a second one.
    pub fn retrieved_with_counts(&self, conversation: Option<u64>) -> Vec<(RefId, u32)> {
        let Some(conversation) = conversation else {
            return Vec::new();
        };
        let Ok(map) = self.retrieved.lock() else {
            return Vec::new();
        };
        map.get(&conversation)
            .map(|held| {
                held.iter()
                    .filter(|(_, fetched)| fetched.promoted)
                    .map(|(id, fetched)| {
                        (
                            *id,
                            if fetched.inlined {
                                u32::MAX
                            } else {
                                fetched.fetches
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// How far the provider's cache reached when this proxy first saw the
    /// conversation — the highest message index it must not touch.
    ///
    /// # Why the client's own marker is the wrong answer
    ///
    /// Claude Code puts a `cache_control` breakpoint on its **final** message,
    /// so the frozen floor derived from it is `max(i) + 1` — the whole
    /// conversation, including the messages that arrived on this very turn.
    /// Those are not cached. They are about to be. Measured across a 20-turn
    /// Harbor run, `frozen + opaque == messages_total` on every single turn:
    /// the floor described where the cache *would be after the request*, not
    /// where it was, and the engine was handed nothing it was allowed to move.
    ///
    /// The bytes between the previous turn's boundary and the new marker are
    /// in no cache entry at all, so compressing them cannot invalidate
    /// anything, and the prefix in front of them is untouched so every existing
    /// hit still lands.
    ///
    /// # Why it is a watermark and not "wherever the cache reached last turn"
    ///
    /// Because what the provider cached is *this proxy's output*, not the
    /// client's input. Compress message seven on turn five and the cache holds
    /// the compressed form; freeze it on turn six and the client's original
    /// bytes go out instead, missing the cache from that byte on and re-reading
    /// the rest at full price. So once the engine is allowed to touch an index
    /// it must keep being allowed to, and the floor may only ever fall.
    ///
    /// Held at the first `observed` value, which is what was genuinely cached
    /// before this proxy had written anything. A session the proxy sees from
    /// its first turn yields a floor of zero; one it joins mid-conversation
    /// keeps whatever the client had already established.
    ///
    /// Byte-stability across turns is what makes this safe, and it is a real
    /// property of the engine rather than an assumption:
    /// `a_message_compresses_the_same_however_much_follows_it` in
    /// `kompressor_engine::messages`, and handles are content-addressed, so the
    /// same message compresses to the same bytes on every turn.
    ///
    /// Which conversation is this, given the chain over its opening messages?
    ///
    /// A conversation cannot be identified by hashing a FIXED SELECTION of
    /// early messages, because agent harnesses open every conversation the
    /// same way. Hashing the first message put four unrelated Harbor tasks
    /// under one key; adding the first `user` message helped and was still not
    /// enough — measured on a 20-task run, 637 requests collapsed into five
    /// keys, and those same five keys reappeared in a later run with different
    /// tasks. A key that repeats across runs is naming the CLIENT.
    ///
    /// So the opening is LEARNED instead of derived. The first time a request
    /// is seen whose opening matches nothing known, its whole message list
    /// becomes that conversation's registered prefix. Every later turn extends
    /// that prefix — appending a turn never rewrites an earlier one — so it
    /// still matches, and two conversations are told apart at the first
    /// message where they actually differ rather than at a depth guessed in
    /// advance.
    ///
    /// Deepest match wins, so a conversation that opened inside another's
    /// preamble is still recognised as itself.
    ///
    /// Two conversations that open identically match one entry, and are told
    /// apart at the first request that departs from the history the entry's
    /// conversation has sent: that request is filed as a new conversation,
    /// registered at the message where the two part. Before that, a request
    /// that replays only the start of a much longer history is the later one
    /// opening (see `Identities::reopening`): it gets an identity of its own
    /// and the opening's key, and the earlier conversation is re-registered
    /// deeper in its own history. Only when the earlier history is itself no
    /// more than `TAIL_REWRITE` messages past the opening are the two still
    /// filed together until they part.
    ///
    /// `None` only for an empty chain. A poisoned lock degrades to the
    /// first-sight hash, which is stable for as long as the lock stays broken.
    pub fn identify(&self, chain: &[u64]) -> Option<u64> {
        let newest = *chain.last()?;
        let Ok(mut ids) = self.ids.lock() else {
            return Some(newest);
        };
        let found = ids
            .lengths
            .keys()
            .rev()
            .copied()
            .filter(|&len| len <= chain.len())
            .find_map(|len| ids.by_prefix.get(&(len, chain[len - 1])).map(|e| e.id));
        if let Some(id) = found {
            if let Some((depth, anchor)) = ids.reopening(id, chain) {
                // Another conversation on the same opening. The earlier one
                // is registered deeper, at a message only its own later
                // turns reach, and gives up the shallower key to this one.
                ids.insert(depth, anchor, id);
                ids.reopened += 1;
                let fresh = newest ^ ids.reopened.wrapping_mul(0x9E37_79B9_7F4A_7C15);
                ids.insert(chain.len(), newest, fresh);
                ids.extend(fresh, chain);
                return Some(fresh);
            }
            let Some(at) = ids.departure(id, chain) else {
                ids.extend(id, chain);
                return Some(id);
            };
            // The same opening, then elsewhere: a conversation of its own,
            // registered at the first message where the two differ so that
            // its later turns match it rather than the one it opened like.
            let fork = chain[at];
            ids.insert(at + 1, fork, fork);
            ids.extend(fork, chain);
            return Some(fork);
        }
        ids.insert(chain.len(), newest, newest);
        ids.extend(newest, chain);
        Some(newest)
    }

    /// The floor [`Self::floor`] holds for this conversation, without filing
    /// one. `None` until a request has set it, and on a poisoned lock.
    pub fn held_floor(&self, fingerprint: u64) -> Option<usize> {
        self.seen
            .lock()
            .ok()?
            .get(&fingerprint)
            .and_then(|entry| entry.floor)
    }

    /// A poisoned lock answers with `observed`, which is the current behaviour
    /// and cannot be wrong.
    pub fn floor(&self, fingerprint: u64, observed: usize) -> usize {
        let Ok(mut seen) = self.seen.lock() else {
            return observed;
        };
        if let Some(entry) = seen.get_mut(&fingerprint) {
            // `min`, never `max`: an eviction that lost the watermark would
            // otherwise let the floor climb back up and strand the bytes
            // already cached in compressed form.
            let held = entry.floor.map_or(observed, |held| held.min(observed));
            entry.floor = Some(held);
            return held;
        }
        // The cap is enforced here as well as in `record`. This runs at
        // request time, before any response, so by the time `record` sees the
        // fingerprint it is already in the table — its own "not yet held"
        // test was never true again and nothing was ever evicted: one entry
        // leaked per conversation, for the life of the process.
        if seen.len() >= REMEMBER
            && let Some(oldest) = seen
                .iter()
                .min_by_key(|(_, entry)| entry.at)
                .map(|(key, _)| *key)
        {
            seen.remove(&oldest);
        }
        seen.insert(
            fingerprint,
            Seen {
                at: self.now(),
                cache_read: None,
                floor: Some(observed),
                was_warm: false,
                // Filed at request time, before any response: no round has
                // happened yet, and nothing has been read from cache to drop.
                rounds: 0,
                idled_past_ttl: false,
                most_messages: 0,
                rewritten: false,
                opened_with: None,
                compressed: false,
            },
        );
        observed
    }

    /// [`Self::floor`], except that a conversation seen for the first time
    /// under a new model, carrying a history this proxy already sent under
    /// another one, starts from that conversation's floor instead of the
    /// client's marker.
    ///
    /// `lineage` is the request's identity chain with the model left out of
    /// scope — the same history under any model hashes alike — and `model`
    /// the model it asks for.
    ///
    /// The marker is the right first floor for a conversation the proxy
    /// joins: the client may have cached it before anything came through
    /// here. It is the wrong one for a conversation the proxy has carried all
    /// along, moved to another model mid-flight. The provider caches per
    /// model, so nothing is cached under the new one yet and no floor can cost
    /// a read; freezing to the marker only sends whole, for the rest of the
    /// session, whatever the earlier model's turns had compressed.
    ///
    /// Measured on 2026-11-10 (`11-03-18`): Opus 5.5's safety classifier
    /// refused the biology tasks and Claude Code carried on under
    /// `claude-opus-5` (`model_refusal_fallback`, 76 of 192 requests). The
    /// first request under the fallback was the refused one again, its tool
    /// result already in it and marked, so the floor froze all four messages;
    /// every later turn kept that result whole, and dna-insert compressed
    /// 3.8–5.2% per attempt where the requests before the switch compressed
    /// about 30%. Starting from the sibling's floor, the result compresses to
    /// the same bytes the earlier model was sent.
    ///
    /// Only a sibling under a DIFFERENT model is taken: two attempts at one
    /// Harbor task replay each other's opening under the same model, and
    /// there the marker rule stands.
    pub fn floor_for(
        &self,
        fingerprint: u64,
        observed: usize,
        lineage: Option<u64>,
        model: &str,
    ) -> usize {
        let sibling = lineage.and_then(|key| {
            let mut lineages = self.lineages.lock().ok()?;
            let before = lineages.get(&key).cloned();
            if lineages.len() >= REMEMBER
                && !lineages.contains_key(&key)
                && let Some(oldest) = lineages
                    .iter()
                    .min_by_key(|(_, entry)| entry.at)
                    .map(|(key, _)| *key)
            {
                lineages.remove(&oldest);
            }
            lineages.insert(
                key,
                Lineage {
                    fingerprint,
                    model: model.to_string(),
                    at: Instant::now(),
                },
            );
            before.filter(|prior| prior.fingerprint != fingerprint && prior.model != model)
        });
        let first_sight = self
            .seen
            .lock()
            .map(|seen| !seen.contains_key(&fingerprint))
            .unwrap_or(false);
        let seeded = match sibling {
            Some(prior) if first_sight => self
                .held_floor(prior.fingerprint)
                .map_or(observed, |held| held.min(observed)),
            _ => observed,
        };
        self.floor(fingerprint, seeded)
    }

    /// Has the provider ever served this prefix from cache?
    ///
    /// The question a discard has to ask before it throws a rewrite away.
    /// Discarding forwards the client's ORIGINAL bytes, so the provider caches
    /// those — and a later turn that does compress the same messages sends
    /// different bytes and misses from there on. The flip is raw-to-compressed,
    /// so suppressing the reverse would not help; what has to stop is caching
    /// raw bytes for messages a later turn will rewrite.
    ///
    /// Before anything is cached a discard is free, and the guard it protects
    /// is worth keeping: a body that grew is a body we should never have
    /// touched. Once there is a cached prefix, a marginally larger body costs a
    /// few hundred tokens and a missed prefix costs the re-read of everything
    /// after the first changed byte, which on this traffic is four orders of
    /// magnitude more.
    ///
    /// Measured: a 39-request Opus run discarded turns four and five, compressed
    /// from turn six, and lost exactly `cw[4] + cw[5]` = 8,299 cached tokens at
    /// that transition — 11.1% of everything it wrote.
    pub fn has_been_cached(&self, fingerprint: u64) -> bool {
        self.seen
            .lock()
            .map(|seen| seen.get(&fingerprint).is_some_and(|entry| entry.was_warm))
            .unwrap_or(false)
    }

    /// How many distinct conversations are remembered.
    ///
    /// The observable that says whether the key is stable: an agent loop is
    /// one conversation, so a run that files a new entry every turn has a
    /// fingerprint that moves with the prefix rather than identifying the
    /// conversation. Also the number to watch if the bound is ever reached.
    pub fn tracked(&self) -> usize {
        self.seen.lock().map(|seen| seen.len()).unwrap_or(0)
    }

    /// Of the conversations remembered, how many hold a frozen prefix and
    /// how many have been moved to one-hour entries: what a restart forgets,
    /// for the Maintenance page.
    pub fn held(&self) -> (usize, usize, usize) {
        self.seen
            .lock()
            .map(|seen| {
                (
                    seen.len(),
                    seen.values()
                        .filter(|entry| entry.floor.is_some_and(|floor| floor > 0))
                        .count(),
                    seen.values().filter(|entry| entry.idled_past_ttl).count(),
                )
            })
            .unwrap_or_default()
    }
}

/// A stable identifier for the *conversation* whose prefix is frozen.
///
/// The head, not the whole prefix. That distinction is the whole function.
///
/// The first version hashed every frozen message, which sounds like the honest
/// key — it is what the provider caches on — and is useless in practice,
/// because on agent traffic the frozen prefix grows every single turn. Measured
/// on a Claude Code run: `frozen_messages` went 0, 3, 4, 5, 7 … 26 across 24
/// requests, **23 distinct values**, so every turn asked about a key no turn
/// had ever filed evidence under. The tracker was correct and permanently
/// empty.
///
/// The provider's cache does not work that way either: a longer prefix still
/// hits the cache for its common head, so "is this conversation's prefix warm"
/// is the question worth asking, and the head is what identifies a
/// conversation while the prefix moves.
///
/// Exactly the first message, never the first *n*: a count that varied with
/// how much happened to be frozen would reintroduce the same instability in
/// smaller form — turn one hashing one message and turn five hashing two are
/// different keys for one conversation.
///
/// Two conversations whose whole head is identical share a key. That is
/// tolerable in a way the reverse is not: they almost certainly share the
/// cached prefix as well, both answers concern "is a conversation starting
/// this way being served from cache", and anything unknown still resolves to
/// [`PrefixCache::Warm`].
///
/// Every part of `head` is hashed. An earlier version took this same slice
/// and hashed only `head.first()`, which made the parameter a trap: the
/// caller could pass as much as it liked and change nothing. The test that
/// was supposed to guard stability passed 1, 3 and 6 parts and asserted they
/// agreed — true no matter what the tail held, so it proved only that the
/// tail was ignored. Choosing a head that does not move between turns is the
/// CALLER's job (see `handler::conversation_head`); this function's job is to
/// distinguish the heads it is given.
///
/// FNV-1a rather than `DefaultHasher`, whose per-process seed would make two
/// proxies — or one restarted mid-session — disagree about which conversation
/// a request belongs to, with the answer depending on which process replied.
///
/// `None` only when there are no messages at all. An earlier version was fed
/// the *frozen* head and so answered `None` for every OpenAI request, whose
/// floor is always 0 — which silently made the conversation identity, and the
/// unique-bytes accounting that rests on it, Anthropic-only.
/// The rolling chain over a conversation's opening messages.
///
/// `chain[i]` identifies messages `0..=i` exactly, so a longer conversation
/// that opens the same way shares the same leading entries — which is what
/// lets [`PrefixTracker::identify`] recognise turn twenty as the conversation
/// that turn one started. Capped at `IDENTITY_DEPTH`.
pub fn identity_chain(messages: &[&str]) -> Vec<u64> {
    identity_chain_in_scope(messages, &[])
}

/// The key [`PrefixTracker::floor_for`] files a history under: its whole
/// identity chain in the conversation's scope minus the model, so the same
/// history asked of another model hashes alike while another tenant, provider
/// or API family never does. Named arguments rather than a slice of the
/// identity scope, so that a component added to that scope cannot quietly
/// take the model's place here.
pub fn lineage_key(messages: &[&str], tenant: &str, provider: &str, family: &str) -> Option<u64> {
    identity_chain_in_scope(messages, &[tenant, provider, family])
        .last()
        .copied()
}

/// A stable identity chain for messages inside one tenant and provider/model
/// namespace. Namespace components are hashed before conversation messages,
/// with a distinct separator so user-controlled text cannot move a request
/// into another scope.
pub fn identity_chain_in_scope(messages: &[&str], scope: &[&str]) -> Vec<u64> {
    let mut out = Vec::with_capacity(messages.len().min(IDENTITY_DEPTH));
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in scope {
        hash ^= 0xFE;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    for part in messages.iter().take(IDENTITY_DEPTH) {
        // The same separator `fingerprint` uses, and for the same reason:
        // `0xFF` cannot occur in a `&str`, so two different message splits
        // cannot hash alike.
        hash ^= 0xFF;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        out.push(hash);
    }
    out
}

pub fn fingerprint(head: &[&str]) -> Option<u64> {
    if head.is_empty() {
        return None;
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (index, part) in head.iter().enumerate() {
        // Separated, so that `["ab"]` and `["a", "b"]` are different keys.
        // `0xFF` never occurs in a `&str`, which is valid UTF-8, so the
        // encoding is unambiguous for every input this can be given.
        if index > 0 {
            hash ^= 0xFF;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    Some(hash)
}

/// Everything a [`PrefixTracker`] holds that a restart should not forget, in
/// the state file's terms (`crate::persist`): ages in place of instants,
/// lists in place of maps, and no message text.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct TrackerRecord {
    /// Cache reads, billed prompt and cache writes, summed: the measured
    /// blend a promotion is priced against.
    billed: [u64; 3],
    conversations: Vec<SeenRecord>,
    openings: Vec<OpeningRecord>,
    histories: Vec<HistoryRecord>,
    reopened: u64,
    lineages: Vec<LineageRecord>,
    retrieved: Vec<RetrievedRecord>,
    /// `[conversation, client requests seen]`.
    turns: Vec<[u64; 2]>,
    /// The net-cost gate's pins. Without them a restart re-decides every
    /// message against the prompt as it is now, and a message whose decision
    /// flips changes bytes the provider has cached.
    gate_pins: Vec<GatePinsRecord>,
    /// `[conversation, prompt tokens]`: what the gate prices a new message's
    /// fetch against until the conversation's next response is filed.
    prompts: Vec<[u64; 2]>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct GatePinsRecord {
    conversation: u64,
    age_ms: u64,
    /// `(read key, pin)`, read keys being hashes of the message, not its text.
    pins: Vec<(u64, GatePin)>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct SeenRecord {
    conversation: u64,
    age_ms: u64,
    cache_read: Option<u32>,
    was_warm: bool,
    floor: Option<usize>,
    rounds: u32,
    idled_past_ttl: bool,
    most_messages: usize,
    rewritten: bool,
    opened_with: Option<u32>,
    /// Defaulted so a file written before the field existed still loads.
    #[serde(default)]
    compressed: bool,
}

/// One registered opening: the chain hash over its first `len` messages.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct OpeningRecord {
    len: usize,
    hash: u64,
    conversation: u64,
    age_ms: u64,
}

/// A conversation's longest history, as identity-chain hashes.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct HistoryRecord {
    conversation: u64,
    chain: Vec<u64>,
    age_ms: u64,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct LineageRecord {
    key: u64,
    conversation: u64,
    model: String,
    age_ms: u64,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct RetrievedRecord {
    conversation: u64,
    handles: Vec<FetchedRecord>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct FetchedRecord {
    /// The handle's twelve characters, without `k_`.
    handle: String,
    first_request: String,
    promoted: bool,
    fetches: u32,
    inlined: bool,
    recent: Vec<u64>,
}

/// Keep the `cap` newest of `items`, by `age`.
pub(crate) fn newest<T>(mut items: Vec<T>, cap: usize, age: impl Fn(&T) -> u64) -> Vec<T> {
    if items.len() > cap {
        items.sort_by_key(|item| age(item));
        items.truncate(cap);
    }
    items
}

impl PrefixTracker {
    /// What this tracker holds, for the state file. Conversations in `skip`
    /// are left out of the per-conversation cache evidence and watermark, so
    /// the next process treats them as new.
    pub(crate) fn snapshot(
        &self,
        clock: &crate::persist::Clock,
        skip: &std::collections::HashSet<u64>,
    ) -> TrackerRecord {
        let billed = self
            .billed
            .lock()
            .map(|billed| [billed.0, billed.1, billed.2])
            .unwrap_or_default();
        let conversations = self
            .seen
            .lock()
            .map(|seen| {
                seen.iter()
                    .filter(|(key, _)| !skip.contains(key))
                    .map(|(key, entry)| SeenRecord {
                        conversation: *key,
                        age_ms: clock.age(entry.at),
                        cache_read: entry.cache_read,
                        was_warm: entry.was_warm,
                        floor: entry.floor,
                        rounds: entry.rounds,
                        idled_past_ttl: entry.idled_past_ttl,
                        most_messages: entry.most_messages,
                        rewritten: entry.rewritten,
                        opened_with: entry.opened_with,
                        compressed: entry.compressed,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let (openings, histories, reopened) = self
            .ids
            .lock()
            .map(|ids| {
                (
                    ids.by_prefix
                        .iter()
                        .map(|(&(len, hash), entry)| OpeningRecord {
                            len,
                            hash,
                            conversation: entry.id,
                            age_ms: clock.age(entry.at),
                        })
                        .collect(),
                    ids.history
                        .iter()
                        .map(|(&conversation, (chain, at))| HistoryRecord {
                            conversation,
                            chain: chain.clone(),
                            age_ms: clock.age(*at),
                        })
                        .collect(),
                    ids.reopened,
                )
            })
            .unwrap_or_default();
        let lineages = self
            .lineages
            .lock()
            .map(|lineages| {
                lineages
                    .iter()
                    .filter(|(_, lineage)| lineage.model.len() <= crate::persist::MAX_TEXT_BYTES)
                    .map(|(&key, lineage)| LineageRecord {
                        key,
                        conversation: lineage.fingerprint,
                        model: lineage.model.clone(),
                        age_ms: clock.age(lineage.at),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let retrieved = self
            .retrieved
            .lock()
            .map(|map| {
                map.iter()
                    .map(|(&conversation, held)| RetrievedRecord {
                        conversation,
                        handles: held
                            .iter()
                            .map(|(id, fetched)| FetchedRecord {
                                handle: id.body().to_string(),
                                first_request: shortened(&fetched.first_request),
                                promoted: fetched.promoted,
                                fetches: fetched.fetches,
                                inlined: fetched.inlined,
                                recent: fetched.recent.clone(),
                            })
                            .collect(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let turns = self
            .turns
            .lock()
            .map(|turns| turns.iter().map(|(&key, &turn)| [key, turn]).collect())
            .unwrap_or_default();
        let gate_pins = self
            .gate_pins
            .lock()
            .map(|map| {
                map.iter()
                    .map(|(&conversation, (at, held))| GatePinsRecord {
                        conversation,
                        age_ms: clock.age(*at),
                        pins: held.iter().map(|(&key, &pin)| (key, pin)).collect(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let prompts = self
            .prompts
            .lock()
            .map(|prompts| {
                prompts
                    .iter()
                    .map(|(&key, &tokens)| [key, u64::from(tokens)])
                    .collect()
            })
            .unwrap_or_default();
        TrackerRecord {
            billed,
            conversations,
            openings,
            histories,
            reopened,
            lineages,
            retrieved,
            turns,
            gate_pins,
            prompts,
        }
    }

    /// Replace what this tracker holds with `record`, each table cut to its
    /// cap, newest first. Returns how many conversations it now remembers.
    pub(crate) fn restore(&self, record: TrackerRecord, clock: &crate::persist::Clock) -> usize {
        if let Ok(mut billed) = self.billed.lock() {
            *billed = (record.billed[0], record.billed[1], record.billed[2]);
        }
        let conversations = newest(record.conversations, REMEMBER, |entry| entry.age_ms);
        let remembered = conversations.len();
        if let Ok(mut seen) = self.seen.lock() {
            *seen = conversations
                .into_iter()
                .map(|entry| {
                    (
                        entry.conversation,
                        Seen {
                            at: clock.at(entry.age_ms),
                            cache_read: entry.cache_read,
                            was_warm: entry.was_warm,
                            floor: entry.floor,
                            rounds: entry.rounds,
                            idled_past_ttl: entry.idled_past_ttl,
                            most_messages: entry.most_messages,
                            rewritten: entry.rewritten,
                            opened_with: entry.opened_with,
                            compressed: entry.compressed,
                        },
                    )
                })
                .collect();
        }
        if let Ok(mut ids) = self.ids.lock() {
            let mut restored = Identities {
                reopened: record.reopened,
                ..Identities::default()
            };
            let openings = record
                .openings
                .into_iter()
                .filter(|opening| (1..=IDENTITY_DEPTH).contains(&opening.len))
                .collect();
            for opening in newest(openings, REMEMBER, |opening| opening.age_ms) {
                let fresh = restored
                    .by_prefix
                    .insert(
                        (opening.len, opening.hash),
                        Identity {
                            id: opening.conversation,
                            at: clock.at(opening.age_ms),
                        },
                    )
                    .is_none();
                if fresh {
                    *restored.lengths.entry(opening.len).or_insert(0) += 1;
                }
            }
            for mut history in newest(record.histories, REMEMBER, |history| history.age_ms) {
                history.chain.truncate(IDENTITY_DEPTH);
                restored.history.insert(
                    history.conversation,
                    (history.chain, clock.at(history.age_ms)),
                );
            }
            *ids = restored;
        }
        if let Ok(mut lineages) = self.lineages.lock() {
            *lineages = newest(record.lineages, REMEMBER, |lineage| lineage.age_ms)
                .into_iter()
                .filter(|lineage| lineage.model.len() <= crate::persist::MAX_TEXT_BYTES)
                .map(|lineage| {
                    (
                        lineage.key,
                        Lineage {
                            fingerprint: lineage.conversation,
                            model: lineage.model,
                            at: clock.at(lineage.age_ms),
                        },
                    )
                })
                .collect();
        }
        if let Ok(mut map) = self.retrieved.lock() {
            map.clear();
            for conversation in record.retrieved.into_iter().take(RETRIEVED_CONVERSATIONS) {
                let held: HashMap<RefId, Fetched> = conversation
                    .handles
                    .into_iter()
                    .filter_map(|fetched| {
                        let id = RefId::parse(&fetched.handle)?;
                        let mut recent = fetched.recent;
                        let surplus = recent.len().saturating_sub(DEMAND_WINDOW as usize);
                        recent.drain(..surplus);
                        Some((
                            id,
                            Fetched {
                                first_request: shortened(&fetched.first_request),
                                promoted: fetched.promoted,
                                fetches: fetched.fetches,
                                inlined: fetched.inlined,
                                recent,
                            },
                        ))
                    })
                    .take(RETRIEVED_PER_CONVERSATION)
                    .collect();
                map.insert(conversation.conversation, held);
            }
        }
        if let Ok(mut turns) = self.turns.lock() {
            *turns = record
                .turns
                .into_iter()
                .take(RETRIEVED_CONVERSATIONS)
                .map(|[key, turn]| (key, turn))
                .collect();
        }
        if let Ok(mut map) = self.gate_pins.lock() {
            *map = newest(record.gate_pins, RETRIEVED_CONVERSATIONS, |pins| {
                pins.age_ms
            })
            .into_iter()
            .map(|pins| {
                let held = pins
                    .pins
                    .into_iter()
                    .take(GATE_PINS_PER_CONVERSATION)
                    .collect();
                (pins.conversation, (clock.at(pins.age_ms), held))
            })
            .collect();
        }
        if let Ok(mut prompts) = self.prompts.lock() {
            *prompts = record
                .prompts
                .into_iter()
                .take(RETRIEVED_CONVERSATIONS)
                .map(|[key, tokens]| (key, u32::try_from(tokens).unwrap_or(u32::MAX)))
                .collect();
        }
        remembered
    }
}

/// A request id as the state file keeps it: as it is when short, else a
/// hash of it. Only ever compared for equality with another request's id,
/// and after a restart every request is another one.
fn shortened(request_id: &str) -> String {
    if request_id.len() <= crate::persist::MAX_TEXT_BYTES {
        request_id.to_string()
    } else {
        format!(
            "fnv:{:016x}",
            fingerprint(&[request_id]).unwrap_or_default()
        )
    }
}

#[cfg(test)]
mod tests {
    /// The net-cost gate's pins and the prompt it prices against survive the
    /// state file: without them a restart re-decides a message against the
    /// prompt as it is then, and a flipped decision moves cached bytes.
    #[test]
    fn gate_pins_and_the_last_prompt_survive_a_restart() {
        let before = PrefixTracker::new(Duration::from_secs(0));
        let pin = GatePin {
            prefix_tokens: 120_000,
            write_ppm: 2_000_000,
            read_ppm: 100_000,
        };
        before.record_gate_pins(Some(7), &[(11, pin), (12, GatePin::default())]);
        before.record_prompt(7, 90_000, 0);
        let clock = crate::persist::Clock::writing();
        let record = before.snapshot(&clock, &std::collections::HashSet::new());
        let json = serde_json::to_string(&record).unwrap();
        let after = PrefixTracker::new(Duration::from_secs(0));
        after.restore(serde_json::from_str(&json).unwrap(), &clock);
        assert_eq!(after.gate_pins(Some(7)), before.gate_pins(Some(7)));
        assert_eq!(after.gate_pins(Some(7)).get(&11), Some(&pin));
        assert_eq!(after.last_prompt(7), Some(90_000));
        // A file written before these tables existed still loads.
        let old: TrackerRecord = serde_json::from_str("{}").unwrap();
        after.restore(old, &clock);
        assert!(after.gate_pins(Some(7)).is_empty());
    }

    /// The measured cache hit rate the engine prices a prefix rewrite against.
    #[test]
    fn the_hit_rate_is_measured_and_unknown_reads_as_fully_cached() {
        let tracker = PrefixTracker::new(Duration::from_secs(0));
        // Nothing seen yet. 1000 is not a guess that the cache is perfect -- it
        // is the reading under which a retrieve round is cheapest and so the
        // bar for rewriting a prefix is highest. Erring towards refusing is the
        // cheap mistake; the other way cost 46,379 tokens in one turn.
        assert_eq!(tracker.cache_hit_per_mille(), 1000);

        tracker.record(1, Some(961), Some(1_000), 0);
        assert_eq!(tracker.cache_hit_per_mille(), 961);

        // It is a rate over the traffic, so a second request moves it rather
        // than replacing it.
        tracker.record(2, Some(0), Some(1_000), 0);
        assert_eq!(tracker.cache_hit_per_mille(), 480);

        // A response that carried no usage is not evidence of a cache miss. The
        // same NULL-is-not-zero distinction the loss check turns on: counted as
        // a miss, a run of usage-less responses would drive the rate to zero
        // and promote on a single demand.
        tracker.record(3, None, Some(1_000_000), 0);
        tracker.record(4, Some(500), None, 0);
        assert_eq!(tracker.cache_hit_per_mille(), 480);
    }

    /// A promotion is priced at the provider's real rates, on the traffic's
    /// measured blend of reads, writes and uncached input.
    #[test]
    fn a_promotion_is_priced_at_the_models_rates_and_the_ttl_it_asks_for() {
        use kompressor_engine::messages::CachePricing;
        let tracker = PrefixTracker::new(Duration::from_secs(0));
        let opus =
            CachePrices::for_model(ApiFamily::AnthropicMessages, Some("claude-opus-5-5"), false);
        assert_eq!(
            tracker.promotion_pricing(opus).round_ppm,
            50_000,
            "nothing billed yet: a round priced as all reads, the most demanding bar"
        );

        // afull89's totals, per ten thousand: 92.15% reads, 7.27% writes.
        tracker.record(1, Some(9_215), Some(10_000), 0);
        tracker.record_cache_writes(727);
        assert_eq!(
            tracker.cache_hit_per_mille(),
            921,
            "the rate reports what was measured"
        );

        let five = tracker.promotion_pricing(opus);
        assert_eq!(
            five,
            CachePricing {
                rewrite_ppm: 1_200_000,
                round_ppm: 142_750
            }
        );
        let hour = tracker.promotion_pricing(CachePrices::for_model(
            ApiFamily::AnthropicMessages,
            Some("claude-opus-5-5"),
            true,
        ));
        assert_eq!(
            hour,
            CachePricing {
                rewrite_ppm: 1_950_000,
                round_ppm: 197_275
            }
        );
        // Demands needed for a rewrite of the whole prompt: the hit-rate
        // reading said 5; Opus 5.5 at five minutes says 8, at an hour 9.
        let needed = |p: CachePricing| p.rewrite_ppm / p.round_ppm;
        assert_eq!(needed(CachePricing::from_hit_rate(921)), 5);
        assert_eq!(needed(five), 8);
        assert_eq!(needed(hour), 9);

        // A provider without write charges: a miss is uncached input, which is
        // the shape the hit-rate reading always assumed.
        let openai = tracker.promotion_pricing(CachePrices::for_model(
            ApiFamily::OpenAiResponses,
            Some("gpt-5.5"),
            false,
        ));
        assert_eq!(openai.rewrite_ppm, 900_000);
    }

    #[test]
    fn cache_read_prices_follow_the_model() {
        let read = |model| {
            CachePrices::for_model(ApiFamily::AnthropicMessages, Some(model), false).read_ppm
        };
        assert_eq!(read("claude-opus-5-5"), 50_000);
        assert_eq!(read("claude-sonnet-5-5"), 50_000);
        assert_eq!(read("anthropic/claude-sonnet-5-5"), 50_000);
        assert_eq!(read("claude-fable-5-1"), 25_000);
        assert_eq!(read("claude-mythos-5-1"), 25_000);
        assert_eq!(read("claude-opus-4-8"), 100_000);
        assert_eq!(read("claude-opus-5"), 100_000);
        assert_eq!(read("claude-sonnet-4-5"), 100_000);
        assert_eq!(
            CachePrices::for_model(ApiFamily::AnthropicMessages, None, true).write_ppm,
            2_000_000
        );
    }

    /// The cache report (`/v1/stats/cache`) prices rows with its own copy of
    /// this table, because the statistics crate cannot depend on the proxy.
    /// The two must not drift.
    #[test]
    fn the_cache_report_prices_reads_and_writes_as_the_proxy_does() {
        for family in [
            ApiFamily::AnthropicMessages,
            ApiFamily::OpenAiChat,
            ApiFamily::OpenAiResponses,
        ] {
            for model in [
                None,
                Some("claude-opus-5-5"),
                Some("anthropic/claude-sonnet-5-5"),
                Some("claude-fable-5-1"),
                Some("claude-mythos-5-1"),
                Some("claude-haiku-5-5"),
                Some("claude-opus-4-8"),
                Some("gpt-5.5"),
            ] {
                let report = kompressor_stats::cache::rates(family.as_str(), model);
                let short = CachePrices::for_model(family, model, false);
                let long = CachePrices::for_model(family, model, true);
                let ppm = |rate: f64| (rate * 1_000_000.0).round() as u32;
                assert_eq!(ppm(report.read), short.read_ppm, "{family:?} {model:?}");
                assert_eq!(
                    ppm(report.write_5m),
                    short.write_ppm,
                    "{family:?} {model:?}"
                );
                assert_eq!(ppm(report.write_1h), long.write_ppm, "{family:?} {model:?}");
            }
        }
    }

    /// `kompressor stats cache` and `stats conversations` price rows with the
    /// statistics crate's `breakdown::prices`, another copy of this table for
    /// the same reason. Same check.
    #[test]
    fn the_stats_cli_prices_reads_and_writes_as_the_proxy_does() {
        for family in [
            ApiFamily::AnthropicMessages,
            ApiFamily::OpenAiChat,
            ApiFamily::OpenAiResponses,
        ] {
            for model in [
                None,
                Some("claude-opus-5-5"),
                Some("anthropic/claude-sonnet-5-5"),
                Some("claude-fable-5-1"),
                Some("claude-mythos-5-1"),
                Some("claude-haiku-5-5"),
                Some("claude-opus-4-8"),
                Some("gpt-5.5"),
            ] {
                let cli = kompressor_stats::breakdown::prices(family.as_str(), model);
                let short = CachePrices::for_model(family, model, false);
                let long = CachePrices::for_model(family, model, true);
                let ppm = |rate: f64| (rate * 1_000_000.0).round() as u32;
                assert_eq!(ppm(cli.read), short.read_ppm, "{family:?} {model:?}");
                assert_eq!(ppm(cli.write_5m), short.write_ppm, "{family:?} {model:?}");
                assert_eq!(ppm(cli.write_1h), long.write_ppm, "{family:?} {model:?}");
            }
        }
    }

    /// Demand is what the last few requests asked for, not what the
    /// conversation ever did.
    #[test]
    fn demand_counts_only_fetches_inside_the_window() {
        let tracker = PrefixTracker::new(Duration::from_secs(0));
        let id = kompressor_engine::refs::ref_for("a fetched body");
        tracker.demand(Some(7));
        tracker.record_retrieved(7, "turn-1", &[id]);
        tracker.demand(Some(7));
        tracker.record_retrieved(7, "turn-2", &[id]);
        assert_eq!(
            tracker.demand(Some(7)),
            vec![(id, 2)],
            "two of the last requests fetched it"
        );
        for _ in 0..3 {
            tracker.demand(Some(7));
        }
        assert_eq!(
            tracker.demand(Some(7)),
            vec![(id, 0)],
            "four requests without a fetch: still eligible, no longer in demand"
        );
        assert_eq!(
            tracker.retrieved_with_counts(Some(7)),
            vec![(id, 2)],
            "the lifetime count is untouched"
        );
        tracker.record_inlined(Some(7), &[id]);
        assert_eq!(
            tracker.demand(Some(7)),
            vec![(id, u32::MAX)],
            "a pinned promotion stays pinned"
        );
        assert!(tracker.demand(None).is_empty());
    }

    /// `CacheTtl::Adaptive` moves a conversation to one-hour entries on the
    /// request that finds it idle past five minutes, not the one after.
    #[test]
    fn a_conversation_asks_for_an_hour_once_it_idles_past_the_ttl() {
        let tracker = PrefixTracker::new(Duration::from_secs(0));
        assert!(
            !tracker.long_ttl(9, 2),
            "a conversation never seen asks for nothing"
        );
        tracker.floor(9, 1);
        assert!(
            !tracker.long_ttl(9, 2),
            "a busy conversation keeps the default"
        );

        let idle = |tracker: &PrefixTracker| {
            let mut seen = tracker.seen.lock().unwrap();
            let entry = seen.get_mut(&9).unwrap();
            entry.at = Instant::now() - Duration::from_secs(301);
        };
        idle(&tracker);
        assert!(
            tracker.long_ttl(9, 4),
            "the request that finds the gap rewrites the expired prefix once, at an hour"
        );
        assert!(
            tracker.long_ttl(9, 4),
            "every request after it asks for an hour"
        );
        tracker.record(9, Some(100), Some(1_000), 0);
        assert!(
            tracker.long_ttl(9, 6),
            "and the flag survives the response being recorded"
        );
    }

    /// A replay moves the tracker's clock, and everything that measures idle
    /// time moves with it: what is recorded, the adaptive TTL, a predicted
    /// lapse. A ping read during the pause starts the clock again.
    #[test]
    fn what_the_first_request_left_cached_is_filed_once_and_kept() {
        let tracker = PrefixTracker::new(Duration::from_secs(0));
        assert_eq!(tracker.opened_with(9), None, "never seen");
        tracker.record_opening(9, 18_300);
        assert_eq!(
            tracker.opened_with(9),
            None,
            "nothing recorded to hang it on"
        );

        tracker.record(9, Some(14_482), Some(18_300), 0);
        assert_eq!(tracker.opened_with(9), None, "not until it is filed");
        tracker.record_opening(9, 18_300);
        tracker.record_opening(9, 40_000);
        assert_eq!(
            tracker.opened_with(9),
            Some(18_300),
            "the first figure stands"
        );

        // Later responses, with usage or without, leave it as it was.
        tracker.record(9, Some(30_000), Some(31_000), 0);
        tracker.record(9, None, None, 0);
        assert_eq!(tracker.opened_with(9), Some(18_300));
        assert_eq!(tracker.opened_with(10), None, "another conversation");
    }

    #[test]
    fn a_clock_moved_forward_is_idle_time_for_every_conversation() {
        let tracker = PrefixTracker::new(Duration::from_secs(0));
        for conversation in [9, 10] {
            tracker.floor(conversation, 1);
            tracker.record(conversation, Some(100), Some(1_000), 0);
            tracker.note_messages(conversation, 2);
        }
        let idle_s = |conversation| tracker.idle(conversation, 4).unwrap().0.as_secs();
        assert_eq!(idle_s(9), 0);
        let lifetime = Duration::from_secs(300);
        assert!(!tracker.predicted_cold(9, 4, lifetime));

        tracker.advance_clock_for_test(Duration::from_secs(200));
        assert_eq!((idle_s(9), idle_s(10)), (200, 200));
        assert!(
            !tracker.predicted_cold(9, 4, lifetime),
            "inside the lifetime"
        );

        // A ping reads conversation 10 two hundred seconds in.
        tracker.kept_warm(10);
        tracker.advance_clock_for_test(Duration::from_secs(200));
        assert_eq!((idle_s(9), idle_s(10)), (400, 200));
        assert!(tracker.predicted_cold(9, 4, lifetime));
        assert!(!tracker.predicted_cold(10, 4, lifetime), "kept warm");
        assert!(
            tracker.long_ttl(9, 4),
            "the pause the adaptive TTL watches for"
        );
        assert!(!tracker.long_ttl(10, 4));

        // The answer to the request that found the gap starts it again.
        tracker.record(9, Some(100), Some(1_000), 0);
        assert_eq!(idle_s(9), 0);
        assert!(!tracker.predicted_cold(9, 6, lifetime));
    }

    /// What is recorded for scoring is what `predicted_cold` reads: whether
    /// the request carries more messages than any before it.
    #[test]
    fn the_recorded_continuation_is_the_one_a_prediction_tests() {
        let tracker = PrefixTracker::new(Duration::from_secs(0));
        assert_eq!(tracker.idle(9, 1), None, "never seen");
        tracker.floor(9, 1);
        tracker.record(9, Some(100), Some(1_000), 0);
        tracker.note_messages(9, 3);
        tracker.advance_clock_for_test(Duration::from_secs(400));
        let lifetime = Duration::from_secs(300);
        for (messages, continues) in [(2, false), (3, false), (4, true)] {
            let (_, was_warm, recorded) = tracker.idle(9, messages).unwrap();
            assert!(was_warm);
            assert_eq!(recorded, continues, "{messages} messages");
            assert_eq!(tracker.predicted_cold(9, messages, lifetime), continues);
        }
        // Cold or not, never predicted of a conversation never served from cache.
        tracker.floor(11, 1);
        tracker.note_messages(11, 1);
        assert_eq!(
            tracker.idle(11, 5).map(|(_, warm, more)| (warm, more)),
            Some((false, true))
        );
        assert!(!tracker.predicted_cold(11, 5, lifetime));
    }

    /// A pause a keep-warm ping bridged is not a pause: the cache was read
    /// during it, so `CacheTtl::Adaptive` must not pay for one-hour entries.
    #[test]
    fn a_pause_kept_warm_does_not_switch_to_an_hour() {
        let tracker = PrefixTracker::new(Duration::from_secs(0));
        tracker.floor(9, 1);
        let idle = |tracker: &PrefixTracker| {
            let mut seen = tracker.seen.lock().unwrap();
            seen.get_mut(&9).unwrap().at = Instant::now() - Duration::from_secs(301);
        };
        idle(&tracker);
        tracker.kept_warm(9);
        assert!(!tracker.long_ttl(9, 4), "the ping read the prefix just now");

        idle(&tracker);
        assert!(
            tracker.long_ttl(9, 6),
            "without one, the same pause switches"
        );
        tracker.kept_warm(9);
        assert!(
            tracker.long_ttl(9, 8),
            "and a ping later does not take the switch back"
        );

        tracker.kept_warm(42);
        assert!(
            !tracker.long_ttl(42, 2),
            "a conversation never seen is not created by a ping"
        );
    }

    /// A new agent that opens with the same bytes as one seen long ago is not
    /// that conversation coming back from a pause, and keeps the default TTL:
    /// one-hour requests do not read the five-minute entries every other
    /// conversation has kept warm. The 2026-11-09 keep-warm run: a crack-7z
    /// trial opening 36 minutes after the previous one missed the 13,690-token
    /// shared prefix that way.
    #[test]
    fn a_repeated_opening_after_a_pause_keeps_the_default_ttl() {
        let tracker = PrefixTracker::new(Duration::from_secs(0));
        tracker.floor(9, 1);
        assert!(!tracker.long_ttl(9, 2), "the first trial opens");
        assert!(!tracker.long_ttl(9, 2), "its concurrent twin");
        let idle = |tracker: &PrefixTracker| {
            let mut seen = tracker.seen.lock().unwrap();
            seen.get_mut(&9).unwrap().at = Instant::now() - Duration::from_secs(2_155);
        };
        idle(&tracker);
        assert!(
            !tracker.long_ttl(9, 2),
            "a later trial opening with the same two messages is not a resumption"
        );
        assert!(
            !tracker.long_ttl(9, 1),
            "nor is a shorter request under the same identity"
        );

        idle(&tracker);
        assert!(
            tracker.long_ttl(9, 3),
            "a request that carries the conversation further still finds the gap"
        );
    }

    /// A promotion is taken once, because it is paid for once.
    ///
    /// The bar it is weighed against moves: the prompt grows behind the
    /// message, and the measured hit rate converges over a run. Re-deciding
    /// each turn therefore lets a fragment be promoted and then demoted, and
    /// the demotion rewrites the prefix a second time. Measured 2026-10-08,
    /// 8,192 tokens: promoted while the running rate read 82%, re-elided once
    /// it reached 94%.
    #[test]
    fn a_fragment_already_shipped_whole_is_never_weighed_again() {
        let tracker = PrefixTracker::new(Duration::from_secs(0));
        let id = kompressor_engine::refs::ref_for("a fragment");
        let other = kompressor_engine::refs::ref_for("another fragment");

        // Two requests asking for it is what promotes it in the first place.
        tracker.record_retrieved(7, "req-1", &[id, other]);
        tracker.record_retrieved(7, "req-2", &[id, other]);
        let before = tracker.retrieved_with_counts(Some(7));
        assert_eq!(
            before.iter().find(|(h, _)| *h == id).map(|(_, n)| *n),
            Some(2)
        );

        // Once the engine has shipped it whole, it reports demands no bar can
        // refuse -- not a larger count, which a rising bar could still overtake.
        tracker.record_inlined(Some(7), &[id]);
        let after = tracker.retrieved_with_counts(Some(7));
        assert_eq!(
            after.iter().find(|(h, _)| *h == id).map(|(_, n)| *n),
            Some(u32::MAX)
        );
        // And only that one: pinning is per fragment, not per conversation.
        assert_eq!(
            after.iter().find(|(h, _)| *h == other).map(|(_, n)| *n),
            Some(2)
        );

        // A conversation with nothing to pin against must not panic or invent
        // an entry for a handle it has never seen.
        tracker.record_inlined(None, &[id]);
        tracker.record_inlined(Some(99), &[id]);
        assert!(tracker.retrieved_with_counts(Some(99)).is_empty());
    }

    /// The cache-loss predicate, which decides whether a turn gets a warning.
    #[test]
    fn a_prefix_serving_less_than_last_turn_is_reported_unless_a_round_explains_it() {
        // Within one conversation the prefix only grows, so a smaller cache
        // read means something outside this proxy dropped it.
        assert!(
            is_a_cache_loss(29_056, 0, 0, 0),
            "the total loss seen in the field"
        );
        assert!(
            is_a_cache_loss(28_032, 18_816, 0, 0),
            "and the partial ones"
        );

        // Growth and standing still are the normal cases.
        assert!(!is_a_cache_loss(28_032, 29_056, 0, 0));
        assert!(!is_a_cache_loss(29_056, 29_056, 0, 0));

        // A retrieve turn sums usage across its two upstream calls, so its own
        // figure is inflated and the turn after it appears to lose cache. That
        // is accounting, not churn, and it has been misread as churn twice.
        assert!(
            !is_a_cache_loss(59_860, 30_890, 1, 0),
            "the turn after a retrieve round must not be reported"
        );
        assert!(
            !is_a_cache_loss(29_056, 0, 0, 2),
            "nor a retrieve round itself"
        );
    }

    /// One fetch is not a loop. Acting on it charges every later turn for a
    /// fragment the model may never want again — measured at 24% of the
    /// engine's whole input, for nothing.
    #[test]
    fn a_single_fetch_keeps_nothing_inline() {
        let tracker = PrefixTracker::new(Duration::ZERO);
        let id = kompressor_engine::refs::ref_for("fragment");
        tracker.record_retrieved(1, "req-a", &[id]);
        assert!(tracker.retrieved(Some(1)).is_empty());
    }

    /// The loop's signature: the same handle asked for again in a *later*
    /// request, because the round that answered the first never reached the
    /// client's history.
    #[test]
    fn a_second_fetch_in_another_request_is_what_keeps_it_inline() {
        let tracker = PrefixTracker::new(Duration::ZERO);
        let id = kompressor_engine::refs::ref_for("fragment");
        tracker.record_retrieved(1, "req-a", &[id]);
        tracker.record_retrieved(1, "req-b", &[id]);
        assert_eq!(tracker.retrieved(Some(1)), vec![id]);
    }

    /// A model narrowing twice inside one turn is not looping; it is reading.
    /// `keep_inline` could not help it anyway, because it applies from the
    /// next request onward.
    #[test]
    fn two_fetches_inside_one_request_are_not_a_loop() {
        let tracker = PrefixTracker::new(Duration::ZERO);
        let id = kompressor_engine::refs::ref_for("fragment");
        tracker.record_retrieved(1, "req-a", &[id]);
        tracker.record_retrieved(1, "req-a", &[id]);
        assert!(
            tracker.retrieved(Some(1)).is_empty(),
            "two reads of one fragment in one turn must not charge every later turn for it"
        );
    }

    /// A promotion belongs to the conversation that earned it.
    #[test]
    fn a_promoted_handle_is_held_for_that_conversation_only() {
        let tracker = PrefixTracker::new(Duration::ZERO);
        let a = kompressor_engine::refs::ref_for("fragment a");
        tracker.record_retrieved(1, "req-a", &[a]);
        tracker.record_retrieved(1, "req-b", &[a]);
        // Another conversation fetched the same bytes, but only once.
        tracker.record_retrieved(2, "req-c", &[a]);

        assert_eq!(tracker.retrieved(Some(1)), vec![a]);
        assert!(tracker.retrieved(Some(2)).is_empty());
        assert!(tracker.retrieved(Some(3)).is_empty());
        assert!(tracker.retrieved(None).is_empty());
    }

    /// The map is keyed by something a caller controls, so it has to be bounded
    /// in both directions or it is a leak with extra steps.
    #[test]
    fn the_retrieved_map_is_bounded_per_conversation_and_overall() {
        let tracker = PrefixTracker::new(Duration::ZERO);
        for i in 0..(RETRIEVED_PER_CONVERSATION + 20) {
            let id = kompressor_engine::refs::ref_for(&format!("f{i}"));
            tracker.record_retrieved(1, "req-a", &[id]);
            tracker.record_retrieved(1, "req-b", &[id]);
        }
        assert_eq!(tracker.retrieved(Some(1)).len(), RETRIEVED_PER_CONVERSATION);

        for conversation in 0..(RETRIEVED_CONVERSATIONS as u64 + 50) {
            tracker.record_retrieved(
                conversation + 1000,
                "req-a",
                &[kompressor_engine::refs::ref_for("shared")],
            );
        }
        let held = tracker.retrieved.lock().unwrap().len();
        assert!(
            held <= RETRIEVED_CONVERSATIONS,
            "the map grew to {held} conversations"
        );
    }

    use super::*;

    #[test]
    fn an_unseen_prefix_is_warm() {
        // No evidence is not evidence of absence: the provider may hold this
        // from a request that never came through this proxy.
        let tracker = PrefixTracker::new(Duration::ZERO);
        assert_eq!(tracker.state(1), PrefixCache::Warm);
    }

    #[test]
    fn a_prefix_the_provider_read_nothing_for_is_cold_once_it_was_warm() {
        // The one signal that is observation rather than inference — but it
        // only means "lapsed" for a prefix that was demonstrably cached before.
        let tracker = PrefixTracker::new(Duration::ZERO);
        tracker.record(1, Some(41_116), None, 0);
        tracker.record(1, Some(0), None, 0);
        assert_eq!(tracker.state(1), PrefixCache::Cold);
    }

    #[test]
    fn a_first_turn_reading_nothing_is_new_rather_than_cold() {
        // The bug this guards, and it fired on every conversation. The first
        // request of a session reads zero from cache because nothing is cached
        // yet — the provider is busy *creating* the entry — and reading that as
        // a lapse inverts the whole feature: the next turn lifts the freeze and
        // rewrites the prefix that was just written.
        //
        // Measured on a Claude Code subscription session: turn one reported
        // `cache_read = 0` with `cache_creation = 22,363`, turn three then ran
        // with `frozen_messages = 0`, and the provider served only 14,176 of
        // the 22,363 it held. 8,187 tokens re-read at full price, once per
        // conversation.
        let tracker = PrefixTracker::new(Duration::ZERO);
        tracker.record(1, Some(0), None, 0);
        assert_eq!(
            tracker.state(1),
            PrefixCache::Warm,
            "nothing was cached yet, so nothing has lapsed"
        );
        // And once it has been served, a zero means what it always meant.
        tracker.record(1, Some(900), None, 0);
        tracker.record(1, Some(0), None, 0);
        assert_eq!(tracker.state(1), PrefixCache::Cold);
    }

    #[test]
    fn a_prefix_the_provider_served_from_cache_stays_warm() {
        let tracker = PrefixTracker::new(Duration::ZERO);
        tracker.record(1, Some(41_116), None, 0);
        assert_eq!(tracker.state(1), PrefixCache::Warm);
    }

    #[test]
    fn a_response_with_no_usage_does_not_erase_what_is_known() {
        // The bug this guards. One request can reach `finish` more than once —
        // an abandoned stream, an error path — and the later call carries no
        // usage. Overwriting with it threw away the provider's own answer and
        // left the next turn with nothing to go on.
        let tracker = PrefixTracker::new(Duration::ZERO);
        tracker.record(1, Some(41_116), None, 0);
        tracker.record(1, Some(0), None, 0);
        tracker.record(1, None, None, 0);
        assert_eq!(
            tracker.state(1),
            PrefixCache::Cold,
            "a usage-less finish must not erase the provider's answer"
        );
    }

    #[test]
    fn a_compressed_turn_is_remembered_for_the_rest_of_the_conversation() {
        let tracker = PrefixTracker::new(Duration::ZERO);
        // Nothing filed yet: nothing to mark.
        tracker.record_compressed(1);
        assert!(!tracker.was_compressed(1));
        tracker.record(1, Some(0), None, 0);
        assert!(!tracker.was_compressed(1));
        tracker.record_compressed(1);
        assert!(tracker.was_compressed(1));
        // Later turns, with usage and without, keep it.
        tracker.record(1, Some(9_000), None, 0);
        tracker.record(1, None, None, 0);
        assert!(tracker.was_compressed(1));
        assert!(!tracker.was_compressed(2));
    }

    #[test]
    fn a_miss_the_provider_wrote_back_is_not_cold() {
        // Warm, then a turn that read nothing but re-created the entry: the
        // provider now holds exactly the bytes that turn sent. Calling the next
        // turn `Cold` lifts the freeze, rewrites those bytes and pays for the
        // write a second time.
        let tracker = PrefixTracker::new(Duration::ZERO);
        tracker.record(1, Some(41_116), None, 0);
        tracker.record(1, Some(0), None, 0);
        tracker.record_rewritten(1);
        assert_eq!(tracker.state(1), PrefixCache::Warm);
        // A usage-less finish of the same request keeps that answer.
        tracker.record(1, None, None, 0);
        assert_eq!(tracker.state(1), PrefixCache::Warm);
        // A later zero read with no write is a lapse nothing has repaired.
        tracker.record(1, Some(0), None, 0);
        assert_eq!(tracker.state(1), PrefixCache::Cold);
    }

    #[test]
    fn a_response_with_no_usage_is_not_evidence() {
        // A streamed response the client abandoned, or a provider that omits
        // usage: neither says anything about the cache, and reading silence as
        // "cold" would rewrite a live prefix.
        let tracker = PrefixTracker::new(Duration::ZERO);
        tracker.record(1, None, None, 0);
        assert_eq!(tracker.state(1), PrefixCache::Warm);
    }

    #[test]
    fn the_age_rule_is_off_unless_a_ttl_is_configured() {
        let tracker = PrefixTracker::new(Duration::ZERO);
        tracker.record(1, Some(999), None, 0);
        // However long ago; zero means "do not guess".
        assert_eq!(tracker.state(1), PrefixCache::Warm);
    }

    #[test]
    fn a_configured_ttl_expires_a_prefix_by_age() {
        let tracker = PrefixTracker::new(Duration::from_nanos(1));
        // Warm, so there is something for age to have expired.
        tracker.record(1, Some(999), None, 0);
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(tracker.state(1), PrefixCache::Cold);
    }

    #[test]
    fn every_part_of_the_head_is_hashed() {
        // This replaces a test that passed 1, 3 and 6 parts and asserted they
        // agreed. It held only because the tail was DISCARDED, so it proved
        // the opposite of what it claimed: a caller could add as much as it
        // liked to the head and the key would not move. Stability across
        // turns is now guarded where it is actually established, over the
        // messages the head is chosen from — see
        // `handler::the_head_does_not_grow_with_the_conversation` and the
        // end-to-end `every_turn_of_one_conversation_files_the_same_identity`.
        let one = fingerprint(&["the opening turn"]);
        let two = fingerprint(&["the opening turn", "and the first thing asked"]);
        assert_ne!(one, two, "a second part must reach the hash");
        assert_ne!(
            fingerprint(&["the opening turn", "this task"]),
            fingerprint(&["the opening turn", "that task"]),
            "two conversations behind one preamble must not collide"
        );
    }

    #[test]
    fn the_same_text_split_differently_is_a_different_identity() {
        // Without a separator the parts would run together and a preamble
        // ending in one character would be indistinguishable from a task
        // starting with it.
        assert_ne!(fingerprint(&["ab"]), fingerprint(&["a", "b"]));
    }

    #[test]
    fn different_conversations_get_different_identities() {
        assert_ne!(
            fingerprint(&["one conversation"]),
            fingerprint(&["another"])
        );
    }

    #[test]
    fn identical_openings_in_different_scopes_get_different_identities() {
        let tracker = PrefixTracker::new(Duration::ZERO);
        let identify =
            |scope: &[&str]| tracker.identify(&identity_chain_in_scope(&["same opening"], scope));

        let base = identify(&["tenant-a", "openai", "gpt-5"]);
        assert_eq!(identify(&["tenant-a", "openai", "gpt-5"]), base);
        assert_ne!(identify(&["tenant-b", "openai", "gpt-5"]), base);
        assert_ne!(identify(&["tenant-a", "anthropic", "gpt-5"]), base);
        assert_ne!(identify(&["tenant-a", "openai", "gpt-4"]), base);
    }

    #[test]
    fn retrieval_promotions_do_not_cross_scopes_with_identical_openings() {
        let tracker = PrefixTracker::new(Duration::ZERO);
        let identify = |tenant: &str| {
            tracker
                .identify(&identity_chain_in_scope(
                    &["same opening"],
                    &[tenant, "openai", "gpt-5"],
                ))
                .unwrap()
        };
        let tenant_a = identify("tenant-a");
        let tenant_b = identify("tenant-b");
        let id = kompressor_engine::refs::ref_for("fragment");

        assert_eq!(tracker.floor(tenant_a, 8), 8);
        assert_eq!(tracker.floor(tenant_b, 2), 2);
        assert_eq!(tracker.floor(tenant_a, 3), 3);
        assert_eq!(tracker.floor(tenant_b, 6), 2);

        tracker.record_retrieved(tenant_a, "req-a", &[id]);
        tracker.record_retrieved(tenant_a, "req-b", &[id]);

        assert_eq!(tracker.retrieved(Some(tenant_a)), vec![id]);
        assert!(tracker.retrieved(Some(tenant_b)).is_empty());
    }

    #[test]
    fn nothing_frozen_has_no_fingerprint() {
        assert_eq!(fingerprint(&[]), None);
    }

    #[test]
    fn the_floor_is_held_at_what_was_cached_before_we_wrote_anything() {
        // An agent advances its cache breakpoint every turn, so the marker
        // count climbs: 1, 3, 5 … The watermark must not climb with it, or the
        // engine is re-frozen out of the very bytes it compressed last turn —
        // and those bytes are in the provider's cache in *compressed* form, so
        // sending the client's originals instead would miss from there on.
        let tracker = PrefixTracker::new(Duration::ZERO);
        assert_eq!(tracker.floor(1, 1), 1, "the first sight defines it");
        assert_eq!(tracker.floor(1, 3), 1);
        assert_eq!(tracker.floor(1, 5), 1);
        assert_eq!(tracker.floor(1, 99), 1);
    }

    #[test]
    fn a_floor_may_fall_but_never_rise() {
        // A client that retracts a breakpoint has un-cached that prefix; there
        // is nothing there to protect any more and the engine may have it.
        let tracker = PrefixTracker::new(Duration::ZERO);
        assert_eq!(tracker.floor(1, 8), 8);
        assert_eq!(tracker.floor(1, 2), 2, "a retracted breakpoint frees it");
        assert_eq!(tracker.floor(1, 7), 2, "and it does not come back");
    }

    #[test]
    fn a_conversation_seen_from_its_first_turn_freezes_nothing() {
        // The case worth the whole feature: the proxy was there before the
        // provider cached anything, so no prefix is load-bearing and every
        // message is the engine's to compress.
        let tracker = PrefixTracker::new(Duration::ZERO);
        assert_eq!(tracker.floor(1, 0), 0);
        assert_eq!(tracker.floor(1, 12), 0);
    }

    #[test]
    fn the_floor_survives_the_response_that_follows_it() {
        // The watermark is set at request time and `record` runs after; an
        // insert that rebuilt the entry would wipe it every single turn, and
        // the floor would be redefined by whatever the next turn marked.
        let tracker = PrefixTracker::new(Duration::ZERO);
        assert_eq!(tracker.floor(1, 2), 2);
        tracker.record(1, Some(41_116), None, 0);
        assert_eq!(tracker.floor(1, 9), 2, "the response must not reset it");
    }

    #[test]
    fn a_history_moved_to_another_model_starts_from_its_floor() {
        let tracker = PrefixTracker::new(Duration::ZERO);
        assert_eq!(tracker.floor_for(1, 1, Some(10), "opus-5-5"), 1);
        assert_eq!(tracker.floor_for(1, 2, Some(20), "opus-5-5"), 1);
        assert_eq!(
            tracker.floor_for(2, 2, Some(20), "opus-5"),
            1,
            "the same history under a fallback model is the same conversation"
        );
        assert_eq!(tracker.floor_for(2, 5, Some(30), "opus-5"), 1);
    }

    #[test]
    fn the_same_history_under_the_same_model_keeps_the_marker() {
        // Two attempts at one Harbor task: one opening, one model, two
        // conversations. The second may have been cached by the client
        // before it came through here, so first sight still means the marker.
        let tracker = PrefixTracker::new(Duration::ZERO);
        assert_eq!(tracker.floor_for(1, 0, Some(10), "opus-5-5"), 0);
        assert_eq!(tracker.floor_for(2, 2, Some(10), "opus-5-5"), 2);
    }

    #[test]
    fn a_known_conversation_is_not_seeded_from_a_sibling() {
        // The seed is for first sight only. A conversation with a floor of
        // its own keeps it: lowering it would rewrite bytes already cached.
        let tracker = PrefixTracker::new(Duration::ZERO);
        assert_eq!(tracker.floor_for(1, 0, Some(10), "opus-5-5"), 0);
        assert_eq!(tracker.floor_for(2, 3, Some(20), "opus-5"), 3);
        assert_eq!(tracker.floor_for(2, 3, Some(10), "opus-5"), 3);
    }

    #[test]
    fn a_seed_never_freezes_more_than_the_client_marked() {
        // The sibling's floor is an upper bound, not a value: a fallback
        // request that marks less than it may still have the rest.
        let tracker = PrefixTracker::new(Duration::ZERO);
        assert_eq!(tracker.floor_for(1, 3, Some(10), "opus-5-5"), 3);
        assert_eq!(tracker.floor_for(2, 1, Some(10), "opus-5"), 1);
    }

    #[test]
    fn going_back_to_the_first_model_keeps_its_own_floor() {
        // Back from the fallback: that conversation is known, so it is not
        // first sight and nothing is seeded from the fallback's floor.
        let tracker = PrefixTracker::new(Duration::ZERO);
        assert_eq!(tracker.floor_for(1, 2, Some(10), "opus-5-5"), 2);
        assert_eq!(tracker.floor_for(2, 0, Some(20), "opus-5"), 0);
        assert_eq!(tracker.floor_for(1, 4, Some(20), "opus-5-5"), 2);
    }

    #[test]
    fn the_lineage_table_is_bounded() {
        // Keyed by caller-controlled history, so it must not grow with it.
        let tracker = PrefixTracker::new(Duration::ZERO);
        for key in 0..(REMEMBER as u64 + 64) {
            tracker.floor_for(key, 0, Some(key), "opus-5-5");
        }
        let held = tracker.lineages.lock().unwrap().len();
        assert!(held <= REMEMBER, "the table must stay bounded, held {held}");
    }

    #[test]
    fn the_lineage_key_ignores_the_model_and_nothing_else() {
        let history = ["open", "result"];
        let key = lineage_key(&history, "tenant", "anthropic", "anthropic");
        assert!(key.is_some());
        assert_ne!(
            key,
            lineage_key(&history, "other", "anthropic", "anthropic"),
            "another tenant's history must never seed this one"
        );
        assert_ne!(key, lineage_key(&history, "tenant", "openai", "anthropic"));
        assert_ne!(
            key,
            lineage_key(&history, "tenant", "anthropic", "openai_chat")
        );
        assert_ne!(
            key,
            lineage_key(&["open"], "tenant", "anthropic", "anthropic")
        );
        assert_eq!(lineage_key(&[], "tenant", "anthropic", "anthropic"), None);
    }

    #[test]
    fn no_lineage_is_the_marker_rule() {
        let tracker = PrefixTracker::new(Duration::ZERO);
        assert_eq!(tracker.floor_for(1, 4, None, "opus-5-5"), 4);
        assert_eq!(tracker.floor_for(1, 9, None, "opus-5-5"), 4);
    }

    #[test]
    fn floors_are_per_conversation() {
        let tracker = PrefixTracker::new(Duration::ZERO);
        assert_eq!(tracker.floor(1, 3), 3);
        assert_eq!(tracker.floor(2, 7), 7);
        assert_eq!(tracker.floor(1, 9), 3);
    }

    /// The order the proxy actually calls in: `floor` when the request
    /// arrives, `record` when the response does. `floor` files the entry, so
    /// the bound has to hold there — `record` alone never sees a new key.
    #[test]
    fn the_table_is_bounded_when_the_floor_is_asked_first() {
        let tracker = PrefixTracker::new(Duration::ZERO);
        for key in 0..(REMEMBER as u64 + 64) {
            tracker.floor(key, 3);
            tracker.record(key, Some(1), None, 0);
        }
        let held = tracker.seen.lock().unwrap().len();
        assert!(held <= REMEMBER, "the table must stay bounded, held {held}");
        // The newest conversation is still tracked, with its floor.
        assert_eq!(tracker.floor(REMEMBER as u64 + 63, 9), 3);
        assert_eq!(tracker.state(REMEMBER as u64 + 63), PrefixCache::Warm);
    }

    /// The same through `floor_for`, which is what the handler calls.
    #[test]
    fn the_table_is_bounded_through_floor_for() {
        let tracker = PrefixTracker::new(Duration::ZERO);
        for key in 0..(REMEMBER as u64 + 64) {
            tracker.floor_for(key, 1, Some(key ^ 0xabcd), "m");
        }
        assert!(tracker.seen.lock().unwrap().len() <= REMEMBER);
        assert!(tracker.lineages.lock().unwrap().len() <= REMEMBER);
    }

    #[test]
    fn the_table_is_bounded_and_keeps_the_newest() {
        let tracker = PrefixTracker::new(Duration::ZERO);
        for key in 0..(REMEMBER as u64 + 64) {
            // Warm first, then zero: a bare zero is a new prefix, not a cold
            // one, so it would no longer answer `Cold` below.
            tracker.record(key, Some(1), None, 0);
            tracker.record(key, Some(0), None, 0);
        }
        let held = tracker.seen.lock().unwrap().len();
        assert!(held <= REMEMBER, "the table must stay bounded, held {held}");
        // The most recent insert survived; an early one did not.
        assert_eq!(tracker.state(REMEMBER as u64 + 63), PrefixCache::Cold);
        assert_eq!(tracker.state(0), PrefixCache::Warm);
    }
}
