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
//! on. A restart loses both unless `proxy.state_path` keeps them
//! ([`crate::persist`], which writes and loads the two together), and the
//! conversation's next turn then freezes and sends messages the provider
//! holds compressed: one lost prefix, the price a restart has under
//! `frozen_prefix = "first_seen"` without the file. Nothing is unrecoverable
//! — a replaced read's content is retained behind its handle like any other
//! fragment.
//!
//! # One recompaction at a time
//!
//! Deciding and sending are apart: the decision is taken as a request
//! arrives, the body is compressed across an `await`, and only a body certain
//! to go upstream commits. Two requests of one conversation arriving together
//! would both find it cold and both decide. So the decision *reserves* the
//! conversation (`Recompact::reserve`) under the same lock that reads its
//! state, and the reservation is held — released if the request ends up
//! sending something else, kept if it commits — until the provider answers a
//! request that was sent after the recompaction. Requests are told apart by a
//! ticket taken as each arrives (`Recompact::ticket`): an answer to one that
//! arrived before the recompacting request describes the prefix as it was,
//! settles nothing, and is not evidence about the prefix as it now is
//! (`Recompact::predates`).
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
    /// A request is recompacting this conversation, or has, and the provider
    /// has not yet answered it or a later request with usage. Until it does,
    /// what it reported before says nothing about the prefix it now holds.
    hold: Option<Hold>,
    /// The ticket of the last recompaction that went upstream. A request
    /// with a lower one was prepared against the prefix before it.
    recompacted_at: u64,
}

/// One request's claim on a conversation's recompaction.
#[derive(Clone, Copy)]
struct Hold {
    /// The ticket of the request that reserved it.
    ticket: u64,
    /// Whether its recompaction went upstream. Until it has, the claim is a
    /// reservation, and is dropped if the request sends something else.
    committed: bool,
}

/// A conversation reserved for one request's recompaction.
///
/// Dropped without [`Self::commit`] — the request declined, failed to encode
/// its rewrite, or was abandoned by its client mid-compression — it lets go
/// of the conversation, so no path out of a request leaves one reserved for
/// a recompaction that never went upstream.
pub(crate) struct Reservation {
    state: std::sync::Arc<Recompact>,
    conversation: u64,
    ticket: u64,
    committed: bool,
}

impl Reservation {
    /// The recompaction went upstream: what it decided joins the ledger, and
    /// the conversation stays held until the provider answers. Returns how
    /// many reads the ledger now holds.
    pub(crate) fn commit(mut self, decided: &[(u64, ReadDecision)]) -> usize {
        self.committed = true;
        self.state.commit(self.conversation, self.ticket, decided)
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.committed {
            self.state.release(self.conversation, self.ticket);
        }
    }
}

/// Every conversation's ledger, and the record of predictions gone wrong.
#[derive(Default)]
pub struct Recompact {
    ledgers: Mutex<HashMap<u64, Ledger>>,
    wrong: Mutex<VecDeque<Instant>>,
    disabled: AtomicBool,
    wrong_total: AtomicU64,
    /// The last ticket handed out.
    tickets: AtomicU64,
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

    /// A ticket for one arriving request, higher than every one before it.
    /// Taken before the request decides anything, so that tickets order
    /// requests by what they could have known.
    pub(crate) fn ticket(&self) -> u64 {
        self.tickets.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Whether a recompaction of the conversation is being prepared or is
    /// still unanswered.
    ///
    /// While one is, the conversation is not cold again whatever the tracker's
    /// older evidence says: the turn after a recompaction whose response
    /// carried no usage — a client that hung up, an upstream error — would
    /// otherwise be recompacted a second time, deciding afresh over messages
    /// the provider may just have cached. A second request in flight beside
    /// the first is held off the same way, from the moment the first decides.
    pub(crate) fn unsettled(&self, conversation: u64) -> bool {
        self.ledgers
            .lock()
            .ok()
            .is_some_and(|ledgers| ledgers.get(&conversation).is_some_and(|l| l.hold.is_some()))
    }

    /// Claim the conversation for the request holding `ticket`, which has
    /// found it cold and is about to recompact it. `None` when another
    /// request already has: checked and taken under one lock, so of two
    /// requests arriving together exactly one recompacts.
    pub(crate) fn reserve(
        self: &std::sync::Arc<Self>,
        conversation: u64,
        ticket: u64,
    ) -> Option<Reservation> {
        let mut ledgers = self.ledgers.lock().ok()?;
        Self::make_room(&mut ledgers, conversation);
        let ledger = ledgers.entry(conversation).or_default();
        if ledger.hold.is_some() {
            return None;
        }
        ledger.at = Some(Instant::now());
        ledger.hold = Some(Hold {
            ticket,
            committed: false,
        });
        Some(Reservation {
            state: self.clone(),
            conversation,
            ticket,
            committed: false,
        })
    }

    /// Let go of a reservation whose recompaction did not go upstream.
    fn release(&self, conversation: u64, ticket: u64) {
        let Ok(mut ledgers) = self.ledgers.lock() else {
            return;
        };
        let Some(ledger) = ledgers.get_mut(&conversation) else {
            return;
        };
        if ledger
            .hold
            .is_some_and(|hold| hold.ticket == ticket && !hold.committed)
        {
            ledger.hold = None;
            // A conversation that has never been recompacted keeps no ledger.
            if ledger.epoch == 0 {
                ledgers.remove(&conversation);
            }
        }
    }

    /// The provider has answered the request holding `ticket` with usage.
    /// If that request was sent no earlier than the conversation's
    /// recompaction, what the tracker now holds describes the recompacted
    /// prefix and the hold ends. An answer to an older request — one still in
    /// flight when the recompaction went out — ends nothing.
    pub(crate) fn settled(&self, conversation: u64, ticket: u64) {
        if let Ok(mut ledgers) = self.ledgers.lock()
            && let Some(ledger) = ledgers.get_mut(&conversation)
            && ledger
                .hold
                .is_some_and(|hold| hold.committed && ticket >= hold.ticket)
        {
            ledger.hold = None;
        }
    }

    /// Whether the request holding `ticket` arrived before the conversation's
    /// last recompaction went upstream. What the provider reports for such a
    /// request is about the prefix that recompaction replaced.
    pub(crate) fn predates(&self, conversation: u64, ticket: u64) -> bool {
        self.ledgers.lock().ok().is_some_and(|ledgers| {
            ledgers
                .get(&conversation)
                .is_some_and(|ledger| ticket < ledger.recompacted_at)
        })
    }

    /// Evict the ledger idle longest when the table is full and
    /// `conversation` is not in it.
    fn make_room(ledgers: &mut HashMap<u64, Ledger>, conversation: u64) {
        if ledgers.len() >= REMEMBER
            && !ledgers.contains_key(&conversation)
            && let Some(oldest) = ledgers
                .iter()
                .min_by_key(|(_, ledger)| ledger.at)
                .map(|(key, _)| *key)
        {
            ledgers.remove(&oldest);
        }
    }

    /// Record a recompaction that went upstream: what it decided joins the
    /// ledger. Returns how many reads the ledger now holds.
    ///
    /// A decision is never taken back and only ever strengthened, so this
    /// adds, or replaces an entry with a stronger one. `decided` is the
    /// engine's whole answer for the turn, replays included.
    fn commit(&self, conversation: u64, ticket: u64, decided: &[(u64, ReadDecision)]) -> usize {
        let Ok(mut ledgers) = self.ledgers.lock() else {
            return 0;
        };
        Self::make_room(&mut ledgers, conversation);
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
        ledger.hold = Some(Hold {
            ticket,
            committed: true,
        });
        ledger.recompacted_at = ledger.recompacted_at.max(ticket);
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

    /// Conversations holding a ledger. For tests and the Maintenance page.
    pub fn tracked(&self) -> usize {
        self.ledgers.lock().map_or(0, |ledgers| ledgers.len())
    }

    /// Every recompacted conversation's ledger, for the state file, and the
    /// conversations left out of it for holding more than
    /// [`crate::persist::MAX_LEDGER_READS`] reads: the caller leaves their
    /// watermark out too, since a lowered floor without its ledger would
    /// send the reads whole into a prefix that holds them replaced.
    ///
    /// A reservation or an unanswered recompaction is not written: the
    /// request behind it ended with this process. Neither are the wrong
    /// predictions, which switch prediction off until restart on purpose.
    pub(crate) fn snapshot(
        &self,
        clock: &crate::persist::Clock,
    ) -> (Vec<LedgerRecord>, std::collections::HashSet<u64>) {
        let mut oversized = std::collections::HashSet::new();
        let Ok(ledgers) = self.ledgers.lock() else {
            return (Vec::new(), oversized);
        };
        let records = ledgers
            .iter()
            .filter(|(_, ledger)| ledger.epoch > 0)
            .filter(|(conversation, ledger)| {
                let fits = ledger.reads.len() <= crate::persist::MAX_LEDGER_READS;
                if !fits {
                    oversized.insert(**conversation);
                }
                fits
            })
            .map(|(&conversation, ledger)| LedgerRecord {
                conversation,
                age_ms: ledger.at.map_or(0, |at| clock.age(at)),
                epoch: ledger.epoch,
                reads: ledger
                    .reads
                    .iter()
                    .map(|(&key, &decision)| (key, decision))
                    .collect(),
            })
            .collect();
        (records, oversized)
    }

    /// Replace every ledger with `records`, newest first up to the cap.
    pub(crate) fn restore(&self, records: Vec<LedgerRecord>, clock: &crate::persist::Clock) {
        let Ok(mut ledgers) = self.ledgers.lock() else {
            return;
        };
        ledgers.clear();
        for record in crate::prefixcache::newest(records, REMEMBER, |record| record.age_ms) {
            if record.reads.len() > crate::persist::MAX_LEDGER_READS || record.epoch == 0 {
                continue;
            }
            ledgers.insert(
                record.conversation,
                Ledger {
                    at: Some(clock.at(record.age_ms)),
                    reads: record.reads.into_iter().collect(),
                    epoch: record.epoch,
                    hold: None,
                    recompacted_at: 0,
                },
            );
        }
    }
}

/// One conversation's ledger in the state file: what its cold turns decided
/// about each read, by `read_key` — a hash, never the read.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct LedgerRecord {
    conversation: u64,
    age_ms: u64,
    epoch: u32,
    reads: Vec<(u64, ReadDecision)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Reserve and commit in one step, as the request that finds a
    /// conversation cold does when nothing is in its way.
    fn recompact(
        state: &Arc<Recompact>,
        conversation: u64,
        decided: &[(u64, ReadDecision)],
    ) -> usize {
        let ticket = state.ticket();
        // Whatever held it before has been answered.
        state.settled(conversation, ticket);
        state
            .reserve(conversation, ticket)
            .expect("nothing holds the conversation")
            .commit(decided)
    }

    #[test]
    fn a_ledger_only_grows_and_counts_its_cold_turns() {
        use ReadDecision::{Matured, Stale, Superseded};
        let state = Arc::new(Recompact::default());
        assert!(state.reads(1).is_empty());
        assert_eq!(state.epoch(1), 0);
        assert_eq!(recompact(&state, 1, &[(10, Stale), (20, Matured)]), 2);
        // The next cold turn replays both, finds the matured one has since
        // been read again in full, and decides one more.
        assert_eq!(
            recompact(&state, 1, &[(10, Stale), (20, Superseded), (30, Matured)]),
            3
        );
        // One that decided nothing new forgets nothing, and a weaker answer
        // does not weaken what is held.
        assert_eq!(recompact(&state, 1, &[(20, Matured)]), 3);
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
        let state = Arc::new(Recompact::default());
        assert!(!state.unsettled(1), "nothing has been recompacted");
        let ticket = state.ticket();
        let reserved = state.reserve(1, ticket).unwrap();
        reserved.commit(&[(10, ReadDecision::Stale)]);
        assert!(state.unsettled(1));
        assert!(!state.unsettled(2));
        state.settled(1, ticket);
        assert!(!state.unsettled(1));
        state.settled(9, ticket);
        assert_eq!(
            state.tracked(),
            1,
            "settling an unknown conversation records nothing"
        );
    }

    /// Two requests of one conversation arrive together and both find it
    /// cold. One recompacts; the other is told the conversation is taken.
    #[test]
    fn of_two_requests_that_find_a_conversation_cold_one_recompacts() {
        let state = Arc::new(Recompact::default());
        let (first, second) = (state.ticket(), state.ticket());
        let reserved = state.reserve(1, first).expect("the first to ask has it");
        assert!(
            state.unsettled(1),
            "held from the decision, not from the send"
        );
        assert!(state.reserve(1, second).is_none());
        assert!(
            state.reserve(2, second).is_some(),
            "another conversation is free"
        );
        reserved.commit(&[(10, ReadDecision::Stale)]);
        assert!(
            state.reserve(1, state.ticket()).is_none(),
            "and still held once sent"
        );
        assert_eq!(state.epoch(1), 1);
    }

    /// A reservation that never commits — the engine declined, the rewrite
    /// could not be encoded, the client hung up mid-compression — is let go.
    #[test]
    fn a_reservation_that_sends_nothing_is_released() {
        let state = Arc::new(Recompact::default());
        drop(state.reserve(1, state.ticket()).unwrap());
        assert!(!state.unsettled(1));
        assert_eq!(state.tracked(), 0, "and leaves no ledger behind");
        assert_eq!(state.epoch(1), 0);
        // After a real recompaction, an abandoned second one keeps the ledger.
        recompact(&state, 1, &[(10, ReadDecision::Stale)]);
        let later = state.ticket();
        state.settled(1, later);
        drop(state.reserve(1, state.ticket()).unwrap());
        assert!(!state.unsettled(1));
        assert_eq!(state.reads(1).len(), 1);
    }

    /// A request sent before the recompaction and answered after it says
    /// nothing about the recompacted prefix.
    #[test]
    fn only_a_request_sent_after_the_recompaction_settles_it() {
        let state = Arc::new(Recompact::default());
        let older = state.ticket();
        let recompacting = state.ticket();
        assert!(!state.predates(1, older), "nothing recompacted yet");
        state
            .reserve(1, recompacting)
            .unwrap()
            .commit(&[(10, ReadDecision::Stale)]);
        assert!(state.predates(1, older));
        assert!(!state.predates(1, recompacting));
        state.settled(1, older);
        assert!(state.unsettled(1), "an older request's answer ends nothing");
        // The recompacting request's own answer does, and so would a later one.
        state.settled(1, recompacting);
        assert!(!state.unsettled(1));
        assert!(state.predates(1, older), "and it stays older for good");
        assert!(!state.predates(1, state.ticket()));
    }

    /// Letting go is by the ticket that took hold: a stale release — a
    /// request ending long after another has taken the conversation — and
    /// one for a conversation nobody holds change nothing.
    #[test]
    fn only_the_holder_releases_and_only_before_it_commits() {
        let state = Arc::new(Recompact::default());
        let (first, second) = (state.ticket(), state.ticket());
        assert!(second > first, "tickets order requests by arrival");
        state.release(1, first);
        assert_eq!(state.tracked(), 0, "nothing to release, nothing recorded");

        let held = state.reserve(1, second).unwrap();
        state.release(1, first);
        assert!(
            state.unsettled(1),
            "another request's ticket lets nothing go"
        );
        held.commit(&[(10, ReadDecision::Stale)]);
        state.release(1, second);
        assert!(
            state.unsettled(1),
            "a committed recompaction waits for the provider, not for a release"
        );
        assert_eq!(state.reads(1).len(), 1);
    }

    /// A reservation takes a place in the table like a ledger does, and at
    /// the cap it evicts the conversation idle longest.
    #[test]
    fn reservations_keep_the_table_bounded() {
        let state = Arc::new(Recompact::default());
        let mut held = Vec::new();
        for conversation in 0..(REMEMBER as u64 + 16) {
            held.push(state.reserve(conversation, state.ticket()).unwrap());
        }
        assert_eq!(state.tracked(), REMEMBER);
        // Dropping a reservation whose entry was evicted is harmless, and the
        // rest let go of theirs.
        drop(held);
        assert_eq!(state.tracked(), 0);
    }

    /// A poisoned table fails towards doing nothing: no reservation, so no
    /// recompaction, and nothing claimed about a ledger that cannot be read.
    #[test]
    fn a_poisoned_table_recompacts_nothing() {
        let state = Arc::new(Recompact::default());
        recompact(&state, 1, &[(10, ReadDecision::Stale)]);
        let poisoner = state.clone();
        let _ = std::thread::spawn(move || {
            let _held = poisoner.ledgers.lock().unwrap();
            panic!("poison the table");
        })
        .join();
        let ticket = state.ticket();
        assert!(state.reserve(1, ticket).is_none());
        assert!(!state.unsettled(1));
        assert!(!state.predates(1, 0));
        assert!(state.reads(1).is_empty());
        assert_eq!(state.would_hold(1, &[(10, ReadDecision::Stale)]), 1);
        assert_eq!(state.commit(1, ticket, &[(10, ReadDecision::Stale)]), 0);
        state.release(1, ticket);
        state.settled(1, ticket);
        assert_eq!((state.epoch(1), state.tracked()), (0, 0));
    }

    #[test]
    fn what_a_ledger_would_hold_counts_each_read_once() {
        use ReadDecision::{Matured, Stale};
        let state = Arc::new(Recompact::default());
        assert_eq!(state.would_hold(1, &[(10, Stale), (20, Matured)]), 2);
        recompact(&state, 1, &[(10, Stale), (20, Matured)]);
        assert_eq!(state.would_hold(1, &[(10, Stale), (30, Stale)]), 3);
        assert_eq!(state.would_hold(1, &[]), 2);
    }

    /// At the cap the ledger idle longest goes, not the one committed first.
    #[test]
    fn a_ledger_still_in_use_is_not_the_one_evicted() {
        let state = Arc::new(Recompact::default());
        recompact(&state, 0, &[(1, ReadDecision::Stale)]);
        for conversation in 1..REMEMBER as u64 {
            recompact(&state, conversation, &[(1, ReadDecision::Stale)]);
        }
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(state.reads(0).len(), 1, "a warm turn replays it");
        recompact(&state, u64::MAX, &[(1, ReadDecision::Stale)]);
        assert_eq!(state.reads(0).len(), 1, "and it is still there");
    }

    #[test]
    fn three_wrong_predictions_in_an_hour_end_prediction() {
        let state = Arc::new(Recompact::default());
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
        let state = Arc::new(Recompact::default());
        for conversation in 0..(REMEMBER as u64 + 16) {
            recompact(&state, conversation, &[(1, ReadDecision::Stale)]);
        }
        assert_eq!(state.tracked(), REMEMBER);
    }
}
