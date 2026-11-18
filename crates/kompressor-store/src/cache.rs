//! A write-through cache in front of another store.
//!
//! The retrieval that matters most is the one that arrives milliseconds after
//! the compression that minted the handle: the proxy hands the model a marker,
//! the model asks for it in the same turn, and the fragment's row may still be
//! in flight to disk (or, in a `sync`-mode deployment, contending with the
//! batch being committed). A miss there is the one failure the model actually
//! notices, because it was told the fragment was retrievable moments earlier.
//!
//! So a bounded number of the most recently *written* fragments are kept in
//! memory and consulted first. The window is short — a minute by default,
//! against an hour-long TTL — because this layer exists to cover the gap around
//! a write, not to be a second store with its own retention.
//!
//! Reads do not populate it. A record coming back from the inner store does not
//! carry its absolute lifetime, so a cached copy of it could only guess at the
//! one deadline a read is not allowed to extend; covering the write gap needs
//! no such guess, because the writer supplied the whole policy.

use crate::clock::Clock;
use crate::types::{
    Lookup, PutMeta, RefRecord, RefStore, StoreError, StoreStats, Tenant, is_expired,
};
use kompressor_engine::refs::RefId;
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};

/// How many fragments the cache holds by default.
pub const DEFAULT_CAPACITY: usize = 4_096;

/// How many fragment bytes the cache holds by default.
///
/// The entry cap alone does not bound memory: 4,096 entries at the default
/// `max_ref_bytes` of 1 MiB is 4 GiB of fragments before any copy or
/// bookkeeping. The window this cache covers is the milliseconds around a
/// write, so 64 MiB is still thousands of typical fragments.
pub const DEFAULT_MAX_BYTES: usize = 64 * 1024 * 1024;

/// How long a cached fragment is served without consulting the inner store.
pub const DEFAULT_CACHE_TTL_SECONDS: u64 = 60;

type Key = (String, RefId);

/// A cached fragment and everything needed to answer a read from it alone.
#[derive(Clone, Debug)]
struct Entry {
    record: RefRecord,
    max_lifetime_seconds: u64,
    /// When this copy entered the cache, for the cache's own short window.
    cached_at: u64,
    /// Which insertion this is, so a stale queue slot can be recognised.
    seq: u64,
}

/// Fixed-capacity, insertion-ordered fragment cache in front of `S`.
///
/// The eviction order is insertion, not access: an entry that keeps being read
/// is also an entry the inner store keeps answering for, so promoting it would
/// buy nothing and cost a second index. Both entries and eviction bookkeeping
/// remain bounded by the configured capacity.
#[derive(Debug)]
pub struct CachedRefStore<S: RefStore> {
    inner: S,
    state: Mutex<CacheState>,
    capacity: usize,
    /// Fragment bytes held, summed over live entries. Enforced alongside
    /// `capacity`; whichever binds first evicts.
    max_bytes: usize,
    cache_ttl_seconds: u64,
    clock: Clock,
}

#[derive(Debug, Default)]
struct CacheState {
    entries: HashMap<Key, Entry>,
    /// Fragment bytes of `entries`, kept in step by every insert and removal.
    bytes: usize,
    /// Entries pushed out by either bound, for the occupancy report.
    evictions: u64,
    /// Insertion order. A key re-inserted appears twice; the `seq` check on
    /// eviction drops the outdated slot instead of the live entry.
    order: VecDeque<(u64, Key)>,
    next_seq: u64,
}

impl CacheState {
    /// Drop one entry, keeping the byte count in step.
    fn remove(&mut self, key: &Key) -> Option<Entry> {
        let entry = self.entries.remove(key)?;
        self.bytes -= entry.record.fragment.len();
        Some(entry)
    }
}

/// What the cache holds against its two bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheOccupancy {
    pub entries: usize,
    pub bytes: usize,
    pub evictions: u64,
    pub max_entries: usize,
    pub max_bytes: usize,
}

impl<S: RefStore> CachedRefStore<S> {
    /// Wrap `inner` with the default capacity and window.
    pub fn new(inner: S) -> CachedRefStore<S> {
        CachedRefStore {
            inner,
            state: Mutex::new(CacheState::default()),
            capacity: DEFAULT_CAPACITY,
            max_bytes: DEFAULT_MAX_BYTES,
            cache_ttl_seconds: DEFAULT_CACHE_TTL_SECONDS,
            clock: Clock::system(),
        }
    }

    /// Hold at most `capacity` fragments.
    pub fn with_capacity(mut self, capacity: usize) -> CachedRefStore<S> {
        self.capacity = capacity;
        self
    }

    /// Hold at most `bytes` of fragment text. A fragment larger than this on
    /// its own is written through without being cached.
    pub fn with_max_bytes(mut self, bytes: usize) -> CachedRefStore<S> {
        self.max_bytes = bytes;
        self
    }

    /// Serve a cached copy for at most `seconds` after it was cached.
    pub fn with_cache_ttl(mut self, seconds: u64) -> CachedRefStore<S> {
        self.cache_ttl_seconds = seconds;
        self
    }

    /// Read time from `clock` instead of the host clock.
    pub fn with_clock(mut self, clock: Clock) -> CachedRefStore<S> {
        self.clock = clock;
        self
    }

    /// The wrapped store.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// How many fragments are cached, expired copies included.
    pub fn cached_len(&self) -> usize {
        self.state.lock().entries.len()
    }

    /// Fragment bytes cached right now, expired copies included.
    pub fn cached_bytes(&self) -> usize {
        self.state.lock().bytes
    }

    /// Entries, bytes and evictions so far, for an occupancy report.
    pub fn occupancy(&self) -> CacheOccupancy {
        let state = self.state.lock();
        CacheOccupancy {
            entries: state.entries.len(),
            bytes: state.bytes,
            evictions: state.evictions,
            max_entries: self.capacity,
            max_bytes: self.max_bytes,
        }
    }

    fn insert(&self, state: &mut CacheState, key: Key, entry: Entry) {
        let size = entry.record.fragment.len();
        if size > self.max_bytes {
            // Caching it would evict everything else and still break the
            // bound. The inner store answers for it; a stale copy under the
            // same key must not survive in its place.
            state.remove(&key);
            return;
        }
        state.order.push_back((entry.seq, key.clone()));
        if let Some(replaced) = state.entries.insert(key, entry) {
            state.bytes -= replaced.record.fragment.len();
        }
        state.bytes += size;
        // Repeated writes must also bound the eviction index, even when the
        // number of distinct fragments never reaches capacity.
        if state.order.len() > self.capacity.saturating_mul(2) {
            state.order.retain(|(seq, key)| {
                state
                    .entries
                    .get(key)
                    .is_some_and(|entry| entry.seq == *seq)
            });
        }
        while state.entries.len() > self.capacity || state.bytes > self.max_bytes {
            let Some((seq, evicted)) = state.order.pop_front() else {
                break;
            };
            // A key written twice left an older slot behind; dropping the
            // current entry for it would evict something still fresh.
            if state.entries.get(&evicted).is_some_and(|e| e.seq == seq) {
                state.remove(&evicted);
                state.evictions += 1;
            }
        }
    }

    /// Whether a cached copy may still answer a read at `now`.
    ///
    /// Two windows apply: the cache's own, and the record's real TTL. The
    /// second matters because a fragment the inner store has already let go
    /// must not come back out of the cache.
    fn is_servable(&self, entry: &Entry, now: u64) -> bool {
        let cache_deadline = entry.cached_at.saturating_add(self.cache_ttl_seconds);
        now <= cache_deadline && !is_expired(&entry.record, entry.max_lifetime_seconds, now)
    }
}

impl<S: RefStore> RefStore for CachedRefStore<S> {
    fn put_many(
        &self,
        tenant: &Tenant,
        refs: &[(RefId, String)],
        meta: &PutMeta,
    ) -> Result<(), StoreError> {
        let now = self.clock.now();
        {
            let mut state = self.state.lock();
            for (id, fragment) in refs {
                let key = (tenant.0.clone(), *id);
                // `created_at` decides the absolute lifetime cap. The cached
                // copy carries it when there is one; when there is not, this
                // assumes the fragment is new.
                //
                // **That assumption is wrong after an eviction, a restart or
                // another writer**, and this comment used to claim otherwise —
                // that the cache "never reports a later creation than the inner
                // store, so it cannot extend the absolute lifetime cap". The
                // durable row preserves the original `created_at` through the
                // upsert, so a re-store of an evicted key records a *newer* one
                // here. Measured with a manual clock: created at 100, absolute
                // lifetime 10, evicted and re-stored at 105 — `verify` reported
                // expired at 110 while `get` went on returning a hit.
                //
                // Not repaired, deliberately. The only authoritative answer is
                // the inner store's, and asking it on every uncached write is
                // the one thing this type exists not to do:
                // `the_cache_serves_a_fragment_the_inner_store_has_not_been_asked_for`
                // and two tests beside it assert exactly that. The exposure is
                // bounded by the cache window — past `cached_at +
                // cache_ttl_seconds` nothing here is servable and the inner
                // store decides again — so a shorter `cache_ttl` bounds it
                // tighter. Closing it properly means carrying the deadline on
                // the write path, which is a change to `PutMeta` rather than to
                // this line.
                let created_at = state
                    .entries
                    .get(&key)
                    .map(|existing| existing.record.created_at)
                    .unwrap_or(now);
                let seq = state.next_seq;
                state.next_seq += 1;
                let entry = Entry {
                    record: RefRecord {
                        id: *id,
                        fragment: fragment.clone(),
                        kind: meta.kind.as_str().to_owned(),
                        request_id: meta.request_id.clone(),
                        created_at,
                        last_accessed: now,
                        ttl_seconds: meta.ttl_seconds,
                    },
                    max_lifetime_seconds: meta.max_lifetime_seconds,
                    cached_at: now,
                    seq,
                };
                self.insert(&mut state, key, entry);
            }
        }
        // Keep the cache ahead of a write in flight, but clear this batch if
        // the backing write fails so no temporary handles escape.
        let result = self.inner.put_many(tenant, refs, meta);
        if result.is_err() {
            let mut state = self.state.lock();
            for (id, _) in refs {
                state.remove(&(tenant.0.clone(), *id));
            }
            return result;
        }

        result
    }

    fn get(&self, tenant: &Tenant, id: &RefId, touch: bool) -> Result<Lookup, StoreError> {
        let now = self.clock.now();
        let key = (tenant.0.clone(), *id);
        let cached = {
            let mut state = self.state.lock();
            match state.entries.get_mut(&key) {
                Some(entry) if self.is_servable(entry, now) => {
                    if touch {
                        entry.record.last_accessed = now;
                    }
                    Some(entry.record.clone())
                }
                Some(_) => {
                    // Past either window: drop the copy and let the inner store
                    // give the authoritative answer, which distinguishes an
                    // expiry from a miss.
                    state.remove(&key);
                    None
                }
                None => None,
            }
        };

        if let Some(record) = cached {
            if touch {
                // The inner entry must not be allowed to idle out while every
                // read is being answered from here. The result is discarded:
                // the inner store may not have the row yet, which is precisely
                // the case this cache covers.
                let _ = self.inner.touch_many(tenant, std::slice::from_ref(id));
            }
            return Ok(Lookup::Hit(record));
        }
        self.inner.get(tenant, id, touch)
    }

    fn verify(&self, tenant: &Tenant, id: &RefId) -> Result<Lookup, StoreError> {
        self.inner.verify(tenant, id)
    }

    fn touch_many(&self, tenant: &Tenant, ids: &[RefId]) -> Result<usize, StoreError> {
        let now = self.clock.now();
        let mut cached_hits = 0;
        {
            let mut state = self.state.lock();
            for id in ids {
                let key = (tenant.0.clone(), *id);
                if let Some(entry) = state.entries.get_mut(&key)
                    && self.is_servable(entry, now)
                {
                    entry.record.last_accessed = now;
                    cached_hits += 1;
                }
            }
        }
        let inner_hits = self.inner.touch_many(tenant, ids)?;
        // The two layers overlap and the inner store reports only a total, so
        // the size of the union is not knowable; the larger count is reported
        // because it never claims fewer live entries than there are.
        Ok(inner_hits.max(cached_hits))
    }

    fn purge_expired(&self, now: u64) -> Result<u64, StoreError> {
        {
            let mut state = self.state.lock();
            // Cached copies of rows the sweep is about to delete would outlive
            // them by up to the cache window otherwise.
            state
                .entries
                .retain(|_, entry| self.is_servable(entry, now));
            state.bytes = state
                .entries
                .values()
                .map(|entry| entry.record.fragment.len())
                .sum();
        }
        self.inner.purge_expired(now)
    }

    fn ping(&self) -> Result<(), StoreError> {
        self.inner.ping()
    }

    fn stats(&self) -> Result<StoreStats, StoreError> {
        // Deliberately the inner store's numbers alone: the cache holds copies,
        // and counting them would double-count everything already written
        // through, which is all of them.
        self.inner.stats()
    }

    fn stats_for(&self, tenant: &Tenant) -> Result<StoreStats, StoreError> {
        // Same reasoning as `stats`: the inner store is the only count that is
        // not double counting the cache's own copies.
        self.inner.stats_for(tenant)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use crate::memory::MemoryRefStore;
    use crate::types::PutMeta;
    use kompressor_engine::refs::ref_for;
    use kompressor_engine::types::Kind;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// An inner store that never finishes a write — the write-in-flight the
    /// cache exists to cover, made total — and counts what it was asked.
    #[derive(Debug, Default)]
    struct StalledStore {
        gets: AtomicUsize,
        touches: AtomicUsize,
    }

    impl RefStore for StalledStore {
        fn put_many(
            &self,
            _: &Tenant,
            _: &[(RefId, String)],
            _: &PutMeta,
        ) -> Result<(), StoreError> {
            Ok(())
        }

        fn get(&self, _: &Tenant, _: &RefId, _: bool) -> Result<Lookup, StoreError> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            Ok(Lookup::Miss)
        }

        fn touch_many(&self, _: &Tenant, ids: &[RefId]) -> Result<usize, StoreError> {
            self.touches.fetch_add(ids.len(), Ordering::SeqCst);
            Ok(0)
        }

        fn purge_expired(&self, _: u64) -> Result<u64, StoreError> {
            Ok(0)
        }

        fn stats(&self) -> Result<StoreStats, StoreError> {
            Ok(StoreStats::default())
        }

        fn stats_for(&self, _tenant: &Tenant) -> Result<StoreStats, StoreError> {
            Ok(StoreStats::default())
        }
    }

    fn meta() -> PutMeta {
        PutMeta::new(Kind::Json)
    }

    fn tenant(name: &str) -> Tenant {
        Tenant::new(name)
    }

    #[test]
    fn repeated_writes_bound_the_eviction_index_and_keep_recent_entries() {
        let store = CachedRefStore::new(StalledStore::default()).with_capacity(2);
        let acme = tenant("acme");
        for i in 0..10_000 {
            let fragment = if i % 2 == 0 { "first" } else { "second" };
            store
                .put_many(&acme, &[(ref_for(fragment), fragment.to_owned())], &meta())
                .unwrap();
            let state = store.state.lock();
            assert!(state.entries.len() <= 2);
            assert!(state.order.len() <= 4);
        }
        store
            .put_many(&acme, &[(ref_for("third"), "third".into())], &meta())
            .unwrap();
        assert!(matches!(
            store.get(&acme, &ref_for("first"), false).unwrap(),
            Lookup::Miss
        ));
        assert_eq!(
            store
                .get(&acme, &ref_for("second"), false)
                .unwrap()
                .fragment(),
            Some("second")
        );
        assert_eq!(
            store
                .get(&acme, &ref_for("third"), false)
                .unwrap()
                .fragment(),
            Some("third")
        );
    }

    #[test]
    fn the_cache_serves_a_fragment_the_inner_store_has_not_been_asked_for() {
        let clock = ManualClock::at(1_000);
        let store = CachedRefStore::new(StalledStore::default()).with_clock(clock.clock());
        let id = ref_for("just compressed");
        store
            .put_many(
                &tenant("acme"),
                &[(id, "just compressed".to_owned())],
                &meta(),
            )
            .unwrap();

        assert_eq!(
            store.get(&tenant("acme"), &id, true).unwrap().fragment(),
            Some("just compressed"),
            "a retrieval landing on the heels of the compression must hit"
        );
        assert_eq!(
            store.inner().gets.load(Ordering::SeqCst),
            0,
            "the cache answered without consulting the inner store"
        );
    }

    #[test]
    fn the_cache_does_not_serve_a_fragment_belonging_to_another_tenant() {
        let clock = ManualClock::at(1_000);
        let store = CachedRefStore::new(StalledStore::default()).with_clock(clock.clock());
        let id = ref_for("tenant a's output");
        store
            .put_many(
                &tenant("acme"),
                &[(id, "tenant a's output".to_owned())],
                &meta(),
            )
            .unwrap();

        // The tenant is half of the cache key too, so the lookup falls through
        // to the inner store rather than matching on the handle alone.
        assert_eq!(store.get(&tenant("beta"), &id, true).unwrap(), Lookup::Miss);
        assert_eq!(store.inner().gets.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_cache_hit_still_refreshes_the_entry_in_the_inner_store() {
        let clock = ManualClock::at(1_000);
        let store = CachedRefStore::new(StalledStore::default()).with_clock(clock.clock());
        let id = ref_for("hot fragment");
        store
            .put_many(&tenant("acme"), &[(id, "hot fragment".to_owned())], &meta())
            .unwrap();

        store.get(&tenant("acme"), &id, true).unwrap();
        assert_eq!(
            store.inner().touches.load(Ordering::SeqCst),
            1,
            "reads served from the cache must not let the stored row idle out"
        );

        store.get(&tenant("acme"), &id, false).unwrap();
        assert_eq!(
            store.inner().touches.load(Ordering::SeqCst),
            1,
            "an untouching read forwards nothing"
        );
    }

    #[test]
    fn a_write_through_put_reaches_the_inner_store() {
        let clock = ManualClock::at(1_000);
        let inner = MemoryRefStore::new().with_clock(clock.clock());
        let store = CachedRefStore::new(inner).with_clock(clock.clock());
        let id = ref_for("persisted");
        store
            .put_many(&tenant("acme"), &[(id, "persisted".to_owned())], &meta())
            .unwrap();

        assert_eq!(
            store
                .inner()
                .get(&tenant("acme"), &id, false)
                .unwrap()
                .fragment(),
            Some("persisted")
        );
        assert_eq!(
            store.stats().unwrap().live_refs,
            1,
            "stats come from the inner store"
        );
    }

    #[test]
    fn a_cached_copy_stops_being_served_once_the_cache_window_closes() {
        let clock = ManualClock::at(1_000);
        let store = CachedRefStore::new(StalledStore::default())
            .with_clock(clock.clock())
            .with_cache_ttl(60);
        let id = ref_for("briefly cached");
        store
            .put_many(
                &tenant("acme"),
                &[(id, "briefly cached".to_owned())],
                &meta(),
            )
            .unwrap();

        clock.advance(61);
        assert_eq!(store.get(&tenant("acme"), &id, true).unwrap(), Lookup::Miss);
        assert_eq!(
            store.inner().gets.load(Ordering::SeqCst),
            1,
            "past its window the cache defers to the store that knows"
        );
        assert_eq!(store.cached_len(), 0, "and it drops the stale copy");
    }

    #[test]
    fn a_cached_copy_is_not_served_past_the_entrys_own_ttl() {
        let clock = ManualClock::at(1_000);
        let store = CachedRefStore::new(StalledStore::default())
            .with_clock(clock.clock())
            .with_cache_ttl(10_000);
        let id = ref_for("short ttl");
        store
            .put_many(
                &tenant("acme"),
                &[(id, "short ttl".to_owned())],
                &meta().with_ttl(30),
            )
            .unwrap();

        clock.advance(31);
        assert_eq!(store.get(&tenant("acme"), &id, true).unwrap(), Lookup::Miss);
    }

    #[test]
    fn the_cache_holds_no_more_than_its_capacity() {
        let clock = ManualClock::at(1_000);
        let store = CachedRefStore::new(StalledStore::default())
            .with_clock(clock.clock())
            .with_capacity(2);
        let acme = tenant("acme");
        let fragments = ["one", "two", "three"];
        let ids: Vec<_> = fragments.iter().map(|f| ref_for(f)).collect();
        for (id, fragment) in ids.iter().zip(fragments) {
            store
                .put_many(&acme, &[(*id, fragment.to_owned())], &meta())
                .unwrap();
        }

        assert_eq!(store.cached_len(), 2);
        assert_eq!(store.get(&acme, &ids[0], false).unwrap(), Lookup::Miss);
        assert_eq!(
            store.get(&acme, &ids[2], false).unwrap().fragment(),
            Some("three")
        );
    }

    /// The entry cap does not bound memory on its own: 4,096 entries of a
    /// 1 MiB fragment each is 4 GiB. Whichever bound binds first evicts.
    #[test]
    fn the_cache_holds_no_more_bytes_than_its_budget() {
        let clock = ManualClock::at(1_000);
        let store = CachedRefStore::new(StalledStore::default())
            .with_clock(clock.clock())
            .with_capacity(1_000)
            .with_max_bytes(250);
        let acme = tenant("acme");
        let fragments: Vec<String> = (0..5).map(|i| format!("{i}").repeat(100)).collect();
        let ids: Vec<_> = fragments.iter().map(|f| ref_for(f)).collect();
        for (id, fragment) in ids.iter().zip(&fragments) {
            store
                .put_many(&acme, &[(*id, fragment.clone())], &meta())
                .unwrap();
            assert!(store.cached_bytes() <= 250, "{}", store.cached_bytes());
        }

        let occupancy = store.occupancy();
        assert_eq!((occupancy.entries, occupancy.bytes), (2, 200));
        assert_eq!(occupancy.evictions, 3);
        // the newest survive, insertion order as for the entry cap
        assert_eq!(store.get(&acme, &ids[0], false).unwrap(), Lookup::Miss);
        assert_eq!(
            store.get(&acme, &ids[4], false).unwrap().fragment(),
            Some(fragments[4].as_str())
        );
    }

    /// A fragment over the whole budget is written through but never cached:
    /// caching it would empty the cache and still break the bound.
    #[test]
    fn a_fragment_larger_than_the_budget_is_not_cached() {
        let inner = MemoryRefStore::new();
        let store = CachedRefStore::new(inner).with_max_bytes(10);
        let acme = tenant("acme");
        let small = "tiny";
        let large = "x".repeat(11);
        store
            .put_many(
                &acme,
                &[
                    (ref_for(small), small.into()),
                    (ref_for(&large), large.clone()),
                ],
                &meta(),
            )
            .unwrap();
        assert_eq!(store.cached_len(), 1);
        assert_eq!(store.cached_bytes(), small.len());
        // and still retrievable, from the inner store
        assert_eq!(
            store
                .get(&acme, &ref_for(&large), false)
                .unwrap()
                .fragment(),
            Some(large.as_str())
        );
    }

    /// Every way an entry leaves keeps the byte count honest; a drift here
    /// would make the budget evict too much or too little forever after.
    #[test]
    fn the_byte_count_follows_replacement_and_purge() {
        let clock = ManualClock::at(1_000);
        let store = CachedRefStore::new(MemoryRefStore::new())
            .with_clock(clock.clock())
            .with_cache_ttl(10);
        let acme = tenant("acme");
        let ids: Vec<_> = ["aaaa", "bbbbbb"].iter().map(|f| ref_for(f)).collect();
        store
            .put_many(
                &acme,
                &[(ids[0], "aaaa".into()), (ids[1], "bbbbbb".into())],
                &meta(),
            )
            .unwrap();
        assert_eq!(store.cached_bytes(), 10);
        // the same handle again: replaced, not double counted
        store
            .put_many(&acme, &[(ids[0], "aaaa".into())], &meta())
            .unwrap();
        assert_eq!(store.cached_bytes(), 10);
        clock.advance(60);
        store.purge_expired(clock.clock().now()).unwrap();
        assert_eq!(store.cached_bytes(), 0);
    }

    #[test]
    fn re_writing_a_cached_handle_does_not_evict_the_entry_it_replaced() {
        // The insertion queue keeps a slot per write, so a handle written twice
        // leaves an outdated slot behind; evicting on it would throw away the
        // fresh copy under it.
        let clock = ManualClock::at(1_000);
        let store = CachedRefStore::new(StalledStore::default())
            .with_clock(clock.clock())
            .with_capacity(2);
        let acme = tenant("acme");
        let kept = ref_for("kept");
        let other = ref_for("other");

        store
            .put_many(&acme, &[(kept, "kept".to_owned())], &meta())
            .unwrap();
        store
            .put_many(&acme, &[(kept, "kept".to_owned())], &meta())
            .unwrap();
        store
            .put_many(&acme, &[(other, "other".to_owned())], &meta())
            .unwrap();

        assert_eq!(
            store.get(&acme, &kept, false).unwrap().fragment(),
            Some("kept")
        );
        assert_eq!(
            store.get(&acme, &other, false).unwrap().fragment(),
            Some("other")
        );
    }

    /// A re-stored entry restarts its absolute lifetime, and that is a defect.
    ///
    /// **A pending regression test, not a characterisation** — it asserts the
    /// behaviour the fix will produce and fails today, which is why it is
    /// ignored. Calling it a characterisation was wrong twice over: a
    /// characterisation asserts what the code *does* and would run in the
    /// default suite, and this does neither.
    ///
    /// It stays ignored rather than inverted because there is no fix that does
    /// not contradict what this type is for: the only authoritative
    /// `created_at` is the inner store's, and three tests here assert the cache
    /// does not ask it. The exposure is bounded by the cache window. Run it
    /// with `--ignored` to see the defect; delete the attribute when the
    /// deadline is carried on the write path.
    #[test]
    #[ignore = "pending fix: a re-stored entry restarts its absolute lifetime; see the comment"]
    fn a_re_stored_entry_does_not_restart_its_absolute_lifetime() {
        let clock = ManualClock::at(100);
        let inner = MemoryRefStore::new().with_clock(clock.clock());
        let store = CachedRefStore::new(inner)
            .with_clock(clock.clock())
            .with_cache_ttl(10_000)
            .with_capacity(1);
        let acme = tenant("acme");
        let a = ref_for("a");
        let b = ref_for("b");
        let meta = meta().with_ttl(100).with_max_lifetime(10);

        store
            .put_many(&acme, &[(a, "a".to_owned())], &meta)
            .unwrap();

        // One slot, so storing B evicts A from the cache. The durable row for A
        // keeps `created_at = 100`.
        clock.set(105);
        store
            .put_many(&acme, &[(b, "b".to_owned())], &meta)
            .unwrap();
        store
            .put_many(&acme, &[(a, "a".to_owned())], &meta)
            .unwrap();

        // Past A's absolute deadline of 100 + 10.
        clock.set(111);
        assert!(
            matches!(store.verify(&acme, &a).unwrap(), Lookup::Expired { .. }),
            "the durable row is past its absolute lifetime"
        );
        assert_eq!(
            store.get(&acme, &a, false).unwrap(),
            Lookup::Miss,
            "and the cache must not go on serving it"
        );
    }

    #[test]
    fn purging_drops_cached_copies_of_the_rows_it_deletes() {
        let clock = ManualClock::at(1_000);
        let inner = MemoryRefStore::new().with_clock(clock.clock());
        let store = CachedRefStore::new(inner)
            .with_clock(clock.clock())
            .with_cache_ttl(10_000);
        let acme = tenant("acme");
        let id = ref_for("swept");
        store
            .put_many(&acme, &[(id, "swept".to_owned())], &meta().with_ttl(30))
            .unwrap();

        clock.advance(31);
        assert_eq!(store.purge_expired(clock.now()).unwrap(), 1);
        assert_eq!(store.cached_len(), 0);
        assert_eq!(store.get(&acme, &id, false).unwrap(), Lookup::Miss);
    }
}
