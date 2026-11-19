//! What a conversation's cold turns decided, kept so every later turn sends
//! the same bytes.
//!
//! A request that arrives after the provider's cache entry has lapsed is
//! written to the cache again at the write price whatever it contains. That
//! makes it the one turn on which rewriting the whole conversation is free,
//! and `proxy.cold_recompact` uses it: the freeze is lifted for good
//! ([`crate::prefixcache::PrefixTracker::lower_floor`]) and the read lifecycle
//! is allowed to decide.
//!
//! The first half needs nothing remembered. Ordinary compression is a function
//! of a message alone, so a conversation with no freeze compresses to the same
//! bytes on every turn. The second half does. Replacing a read as stale needs
//! a *later* write, so left to itself the lifecycle would decide again on the
//! next, warm turn — and replace a read the provider has just cached whole.
//! So the decision is taken on the cold turn, recorded here by
//! [`kompressor_engine::messages::read_key`], and replayed: on a warm turn the
//! engine replaces exactly the reads this ledger names and looks no further.
//!
//! # What is not kept
//!
//! The ledger lives in this process, beside the freeze watermark it depends
//! on. A restart loses both, and the conversation's next turn then freezes and
//! sends messages the provider holds compressed: one lost prefix, the price a
//! restart already has under `frozen_prefix = "first_seen"`. Nothing is
//! unrecoverable — a replaced read's content is retained behind its handle
//! like any other fragment.
//!
//! # A wrong prediction
//!
//! `cold_recompact = "predicted"` acts on an inference, and a wrong one
//! rewrites a prefix the provider was still holding. Each is counted when the
//! provider's answer shows it — a recompacted turn that read from cache after
//! all — and [`MAX_WRONG`] within [`WINDOW`] switch prediction off for the
//! life of the process.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kompressor_engine::read_lifecycle::ReadDecision;

/// Conversations remembered, the same bound the prefix tracker keeps.
const REMEMBER: usize = 4096;
/// Wrong predictions tolerated inside [`WINDOW`].
pub const MAX_WRONG: usize = 3;
/// The window they are counted over.
pub const WINDOW: Duration = Duration::from_secs(3600);

/// Why a turn was recompacted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cold {
    /// The provider's last answer read nothing and wrote nothing.
    Observed,
    /// The request arrived after the cache lifetime its predecessor asked for.
    Predicted,
}

impl Cold {
    pub fn as_str(self) -> &'static str {
        match self {
            Cold::Observed => "observed",
            Cold::Predicted => "predicted",
        }
    }
}

#[derive(Default)]
struct Ledger {
    at: Option<Instant>,
    /// What was decided about each read, by `read_key`.
    reads: BTreeMap<u64, ReadDecision>,
    /// How many cold turns have been recompacted.
    epoch: u32,
    /// A recompaction has gone upstream and the provider has not yet answered
    /// it with usage. Until it does, what it reported before says nothing
    /// about the prefix it now holds.
    unsettled: bool,
}

/// Every conversation's ledger, and the record of predictions gone wrong.
#[derive(Default)]
pub struct Recompact {
    ledgers: Mutex<HashMap<u64, Ledger>>,
    wrong: Mutex<VecDeque<Instant>>,
    disabled: AtomicBool,
    wrong_total: AtomicU64,
}

impl Recompact {
    /// What this conversation has decided about its reads. Empty, not absent,
    /// for one that has decided nothing: the engine is in ledger mode either
    /// way.
    pub(crate) fn reads(&self, conversation: u64) -> BTreeMap<u64, ReadDecision> {
        let Ok(mut ledgers) = self.ledgers.lock() else {
            return BTreeMap::new();
        };
        match ledgers.get_mut(&conversation) {
            Some(ledger) => {
                // Refreshed on every use. The freeze this ledger goes with is
                // lowered for as long as the conversation is remembered; a
                // ledger evicted while the conversation is still under way
                // would leave every read it rewrote to go out whole again,
                // inside a live prefix.
                ledger.at = Some(Instant::now());
                ledger.reads.clone()
            }
            None => BTreeMap::new(),
        }
    }

    /// How many reads the ledger would hold after `decided` joined it.
    pub(crate) fn would_hold(&self, conversation: u64, decided: &[(u64, ReadDecision)]) -> usize {
        let Ok(ledgers) = self.ledgers.lock() else {
            return decided.len();
        };
        let held = ledgers.get(&conversation);
        held.map_or(0, |ledger| ledger.reads.len())
            + decided
                .iter()
                .filter(|(key, _)| held.is_none_or(|ledger| !ledger.reads.contains_key(key)))
                .count()
    }

    /// Whether the conversation's last recompaction is still unanswered.
    ///
    /// While it is, the conversation is not cold again whatever the tracker's
    /// older evidence says: the turn after a recompaction whose response
    /// carried no usage — a client that hung up, an upstream error — would
    /// otherwise be recompacted a second time, deciding afresh over messages
    /// the provider may just have cached. A second request in flight beside
    /// the first is held off the same way.
    pub(crate) fn unsettled(&self, conversation: u64) -> bool {
        self.ledgers
            .lock()
            .ok()
            .is_some_and(|ledgers| ledgers.get(&conversation).is_some_and(|l| l.unsettled))
    }

    /// The provider has answered this conversation with usage: what the
    /// tracker now holds describes the recompacted prefix.
    pub(crate) fn settled(&self, conversation: u64) {
        if let Ok(mut ledgers) = self.ledgers.lock()
            && let Some(ledger) = ledgers.get_mut(&conversation)
        {
            ledger.unsettled = false;
        }
    }

    /// Record a recompaction that went upstream: what it decided joins the
    /// ledger. Returns how many reads the ledger now holds.
    ///
    /// A decision is never taken back and only ever strengthened, so this
    /// adds, or replaces an entry with a stronger one. `decided` is the
    /// engine's whole answer for the turn, replays included.
    pub(crate) fn commit(&self, conversation: u64, decided: &[(u64, ReadDecision)]) -> usize {
        let Ok(mut ledgers) = self.ledgers.lock() else {
            return 0;
        };
        if ledgers.len() >= REMEMBER
            && !ledgers.contains_key(&conversation)
            && let Some(oldest) = ledgers
                .iter()
                .min_by_key(|(_, ledger)| ledger.at)
                .map(|(key, _)| *key)
        {
            ledgers.remove(&oldest);
        }
        let ledger = ledgers.entry(conversation).or_default();
        ledger.at = Some(Instant::now());
        for (key, decision) in decided {
            ledger
                .reads
                .entry(*key)
                .and_modify(|held| *held = (*held).max(*decision))
                .or_insert(*decision);
        }
        ledger.epoch += 1;
        ledger.unsettled = true;
        ledger.reads.len()
    }

    /// How many cold turns of this conversation have been recompacted.
    pub fn epoch(&self, conversation: u64) -> u32 {
        self.ledgers
            .lock()
            .ok()
            .and_then(|ledgers| ledgers.get(&conversation).map(|ledger| ledger.epoch))
            .unwrap_or(0)
    }

    /// Whether a lapse may still be predicted.
    pub fn predicting(&self) -> bool {
        !self.disabled.load(Ordering::Relaxed)
    }

    /// Predictions the provider's answer has contradicted, in all.
    pub fn wrong_predictions(&self) -> u64 {
        self.wrong_total.load(Ordering::Relaxed)
    }

    /// A predicted lapse turned out to be a live cache. Returns whether
    /// prediction is still on.
    pub(crate) fn wrong_prediction(&self) -> bool {
        self.wrong_at(Instant::now())
    }

    fn wrong_at(&self, now: Instant) -> bool {
        self.wrong_total.fetch_add(1, Ordering::Relaxed);
        let Ok(mut wrong) = self.wrong.lock() else {
            return self.predicting();
        };
        wrong.push_back(now);
        while wrong
            .front()
            .is_some_and(|at| now.duration_since(*at) > WINDOW)
        {
            wrong.pop_front();
        }
        if wrong.len() >= MAX_WRONG && !self.disabled.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                wrong = wrong.len(),
                window_s = WINDOW.as_secs(),
                "predicted cache lapses keep reading from cache; cold_recompact \
                 will act on the provider's own answer only, until restart"
            );
        }
        self.predicting()
    }

    /// Conversations holding a ledger. For tests.
    pub fn tracked(&self) -> usize {
        self.ledgers.lock().map_or(0, |ledgers| ledgers.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ledger_only_grows_and_counts_its_cold_turns() {
        use ReadDecision::{Matured, Stale, Superseded};
        let state = Recompact::default();
        assert!(state.reads(1).is_empty());
        assert_eq!(state.epoch(1), 0);
        assert_eq!(state.commit(1, &[(10, Stale), (20, Matured)]), 2);
        // The next cold turn replays both, finds the matured one has since
        // been read again in full, and decides one more.
        assert_eq!(
            state.commit(1, &[(10, Stale), (20, Superseded), (30, Matured)]),
            3
        );
        // One that decided nothing new forgets nothing, and a weaker answer
        // does not weaken what is held.
        assert_eq!(state.commit(1, &[(20, Matured)]), 3);
        assert_eq!(
            state.reads(1).into_iter().collect::<Vec<_>>(),
            [(10, Stale), (20, Superseded), (30, Matured)]
        );
        assert_eq!(state.epoch(1), 3);
        assert!(
            state.reads(2).is_empty(),
            "another conversation's is its own"
        );
    }

    #[test]
    fn a_recompaction_is_unsettled_until_the_provider_answers_it() {
        let state = Recompact::default();
        assert!(!state.unsettled(1), "nothing has been recompacted");
        state.commit(1, &[(10, ReadDecision::Stale)]);
        assert!(state.unsettled(1));
        assert!(!state.unsettled(2));
        state.settled(1);
        assert!(!state.unsettled(1));
        state.settled(9);
        assert_eq!(
            state.tracked(),
            1,
            "settling an unknown conversation records nothing"
        );
    }

    #[test]
    fn what_a_ledger_would_hold_counts_each_read_once() {
        use ReadDecision::{Matured, Stale};
        let state = Recompact::default();
        assert_eq!(state.would_hold(1, &[(10, Stale), (20, Matured)]), 2);
        state.commit(1, &[(10, Stale), (20, Matured)]);
        assert_eq!(state.would_hold(1, &[(10, Stale), (30, Stale)]), 3);
        assert_eq!(state.would_hold(1, &[]), 2);
    }

    /// At the cap the ledger idle longest goes, not the one committed first.
    #[test]
    fn a_ledger_still_in_use_is_not_the_one_evicted() {
        let state = Recompact::default();
        state.commit(0, &[(1, ReadDecision::Stale)]);
        for conversation in 1..REMEMBER as u64 {
            state.commit(conversation, &[(1, ReadDecision::Stale)]);
        }
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(state.reads(0).len(), 1, "a warm turn replays it");
        state.commit(u64::MAX, &[(1, ReadDecision::Stale)]);
        assert_eq!(state.reads(0).len(), 1, "and it is still there");
    }

    #[test]
    fn three_wrong_predictions_in_an_hour_end_prediction() {
        let state = Recompact::default();
        let start = Instant::now();
        assert!(state.wrong_at(start));
        // Old enough to have left the window by the time the third arrives.
        assert!(state.wrong_at(start + WINDOW / 2));
        assert!(state.wrong_at(start + WINDOW + Duration::from_secs(1)));
        assert!(state.predicting(), "only two fall inside any one hour");
        assert!(!state.wrong_at(start + WINDOW + Duration::from_secs(2)));
        assert!(!state.predicting());
        assert_eq!(state.wrong_predictions(), 4);
    }

    #[test]
    fn the_table_stays_bounded() {
        let state = Recompact::default();
        for conversation in 0..(REMEMBER as u64 + 16) {
            state.commit(conversation, &[(1, ReadDecision::Stale)]);
        }
        assert_eq!(state.tracked(), REMEMBER);
    }
}
