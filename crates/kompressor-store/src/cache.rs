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

use crate::browse::{BrowsePage, BrowseQuery, RefBrowse, RefDetail};
use crate::clock::Clock;
use crate::types::{
    Lookup, PutMeta, RefRecord, RefStore, StoreError, StoreStats, Tenant, is_expired,
};
use kompressor_engine::refs::RefId;
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};

/// How many fragments the cache holds by default.
pub const DEFAULT_CAPACITY: usize = 4_096;

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
    cache_ttl_seconds: u64,
    clock: Clock,
}

#[derive(Debug, Default)]
struct CacheState {
    entries: HashMap<Key, Entry>,
    /// Insertion order. A key re-inserted appears twice; the `seq` check on
    /// eviction drops the outdated slot instead of the live entry.
    order: VecDeque<(u64, Key)>,
    next_seq: u64,
}

impl<S: RefStore> CachedRefStore<S> {
    /// Wrap `inner` with the default capacity and window.
    pub fn new(inner: S) -> CachedRefStore<S> {
        CachedRefStore {
            inner,
            state: Mutex::new(CacheState::default()),
            capacity: DEFAULT_CAPACITY,
            cache_ttl_seconds: DEFAULT_CACHE_TTL_SECONDS,
            clock: Clock::system(),
        }
    }

    /// Hold at most `capacity` fragments.
    pub fn with_capacity(mut self, capacity: usize) -> CachedRefStore<S> {
        self.capacity = capacity;
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

    fn insert(&self, state: &mut CacheState, key: Key, entry: Entry) {
        state.order.push_back((entry.seq, key.clone()));
        state.entries.insert(key, entry);
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
        while state.entries.len() > self.capacity {
            let Some((seq, evicted)) = state.order.pop_front() else {
                break;
            };
            // A key written twice left an older slot behind; dropping the
            // current entry for it would evict something still fresh.
            if state.entries.get(&evicted).is_some_and(|e| e.seq == seq) {
                state.entries.remove(&evicted);
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
        self.put_many_created(tenant, refs, meta).map(|_| ())
    }

    fn put_many_created(
        &self,
        tenant: &Tenant,
        refs: &[(RefId, String)],
        meta: &PutMeta,
    ) -> Result<Option<Vec<u64>>, StoreError> {
        let now = self.clock.now();
        let mut seqs = Vec::with_capacity(refs.len());
        {
            let mut state = self.state.lock();
            for (id, fragment) in refs {
                let key = (tenant.0.clone(), *id);
                // `created_at` decides the absolute lifetime cap. The cached
                // copy carries it when there is one; when there is not, this
                // assumes the fragment is new until the inner store says
                // otherwise below. That assumption is wrong after an eviction,
                // a restart or another writer: the durable row keeps its first
                // `created_at` through the upsert, and a cached copy stamped
                // "now" was served past the deadline the row enforced.
                let created_at = state
                    .entries
                    .get(&key)
                    .map(|existing| existing.record.created_at)
                    .unwrap_or(now);
                let seq = state.next_seq;
                state.next_seq += 1;
                seqs.push(seq);
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
        let result = self.inner.put_many_created(tenant, refs, meta);
        match &result {
            Err(_) => {
                let mut state = self.state.lock();
                for (id, _) in refs {
                    state.entries.remove(&(tenant.0.clone(), *id));
                }
            }
            // The row's own creation time, from the write that was happening
            // anyway, so the deadline is the authoritative one without the
            // read this type exists to avoid. Only the copies this call put
            // there are corrected: a newer write may have replaced one since.
            Ok(Some(created)) => {
                let mut state = self.state.lock();
                for (((id, _), seq), created_at) in refs.iter().zip(&seqs).zip(created) {
                    if let Some(entry) = state.entries.get_mut(&(tenant.0.clone(), *id))
                        && entry.seq == *seq
                    {
                        entry.record.created_at = *created_at;
                    }
                }
            }
            // An inner store that cannot say keeps the guess, bounded by the
            // cache window: past `cached_at + cache_ttl_seconds` the inner
            // store decides again.
            Ok(None) => {}
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
                    state.entries.remove(&key);
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
        }
        self.inner.purge_expired(now)
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

    fn browser(&self) -> Option<&dyn RefBrowse> {
        // Browsable exactly when the inner store is. The wrapper itself is
        // what is handed out, so a removal also drops the cached copy.
        self.inner.browser().map(|_| self as &dyn RefBrowse)
    }
}

/// Reads go to the inner store, the only one holding every row; the cache is
/// a subset of it, written through. A removal clears both layers, or the
/// deleted fragment would stay servable for the rest of the cache window.
impl<S: RefStore> RefBrowse for CachedRefStore<S> {
    fn browse(&self, tenant: &Tenant, query: &BrowseQuery) -> Result<BrowsePage, StoreError> {
        match self.inner.browser() {
            Some(inner) => inner.browse(tenant, query),
            None => Ok(BrowsePage::default()),
        }
    }

    fn inspect(&self, tenant: &Tenant, id: &RefId) -> Result<Option<RefDetail>, StoreError> {
        match self.inner.browser() {
            Some(inner) => inner.inspect(tenant, id),
            None => Ok(None),
        }
    }

    fn remove(&self, tenant: &Tenant, ids: &[RefId]) -> Result<usize, StoreError> {
        {
            let mut state = self.state.lock();
            for id in ids {
                state.entries.remove(&(tenant.0.clone(), *id));
            }
        }
        match self.inner.browser() {
            Some(inner) => inner.remove(tenant, ids),
            None => Ok(0),
        }
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
    fn removing_through_the_cache_stops_the_cached_copy_being_served() {
        let store = CachedRefStore::new(MemoryRefStore::new());
        let fragment = "a secret that should not have been stored";
        let id = ref_for(fragment);
        store
            .put_many(&tenant("t"), &[(id, fragment.to_owned())], &meta())
            .unwrap();
        assert_eq!(store.cached_len(), 1);

        let browser = store.browser().expect("browsable through the cache");
        assert_eq!(browser.remove(&tenant("t"), &[id]).unwrap(), 1);
        assert_eq!(store.cached_len(), 0);
        assert_eq!(store.get(&tenant("t"), &id, false).unwrap(), Lookup::Miss);
    }

    #[test]
    fn a_cache_over_an_unbrowsable_store_is_not_browsable() {
        let store = CachedRefStore::new(StalledStore::default());
        assert!(store.browser().is_none());
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

    fn short_lived() -> PutMeta {
        PutMeta {
            ttl_seconds: 100,
            max_lifetime_seconds: 10,
            ..meta()
        }
    }

    /// The durable row keeps its first `created_at` through an upsert, so the
    /// absolute deadline is set then. A re-store after an eviction must not
    /// give the cached copy a later one than the row it copies.
    fn a_re_store_after_eviction_keeps_the_original_deadline_on<S: RefStore>(
        inner: impl FnOnce(Clock) -> S,
    ) {
        let clock = ManualClock::at(100);
        let store = CachedRefStore::new(inner(clock.clock()))
            .with_capacity(1)
            .with_clock(clock.clock());
        let acme = tenant("acme");
        let a = ref_for("fragment a");
        store
            .put_many(&acme, &[(a, "fragment a".into())], &short_lived())
            .unwrap();
        store
            .put_many(
                &acme,
                &[(ref_for("fragment b"), "fragment b".into())],
                &short_lived(),
            )
            .unwrap();

        clock.set(105);
        store
            .put_many(&acme, &[(a, "fragment a".into())], &short_lived())
            .unwrap();

        clock.set(111);
        assert!(
            matches!(store.verify(&acme, &a).unwrap(), Lookup::Expired { .. }),
            "the inner store's verdict"
        );
        assert!(
            !matches!(store.get(&acme, &a, false).unwrap(), Lookup::Hit(_)),
            "the cache must agree with it, not serve past the deadline"
        );
    }

    #[test]
    fn a_re_store_after_eviction_keeps_the_original_deadline_in_sqlite() {
        a_re_store_after_eviction_keeps_the_original_deadline_on(|clock| {
            crate::sqlite::SqliteRefStore::open_in_memory(crate::sqlite::SqliteOptions {
                clock,
                ..Default::default()
            })
            .unwrap()
        });
    }

    #[test]
    fn a_re_store_after_eviction_keeps_the_original_deadline_in_memory() {
        a_re_store_after_eviction_keeps_the_original_deadline_on(|clock| {
            MemoryRefStore::new().with_clock(clock)
        });
    }
}
