//! The in-process backend: the test default, and what `--ephemeral` runs.
//!
//! Fragments are tool output and may hold secrets; keeping them in a process
//! that exits is the strongest retention policy available, so this is not only
//! a testing convenience. What it gives up is sharing: a second instance of the
//! service sees none of these rows, and a restart between the compression that
//! minted a handle and the retrieval that asks for it answers `Miss`.

use crate::clock::Clock;
use crate::types::{
    Lookup, PutMeta, RefRecord, RefStore, StoreError, StoreStats, Tenant, is_expired,
};
use kompressor_engine::refs::RefId;
use parking_lot::Mutex;
use std::collections::HashMap;

/// The record plus the one field [`RefRecord`] does not carry.
#[derive(Clone, Debug)]
struct Entry {
    record: RefRecord,
    max_lifetime_seconds: u64,
}

/// A `HashMap` of fragments with the full TTL semantics.
#[derive(Debug)]
pub struct MemoryRefStore {
    // The tenant is half of the key, not a field to compare after the lookup:
    // a handle from another tenant hashes to a slot that was never filled.
    entries: Mutex<HashMap<(String, RefId), Entry>>,
    clock: Clock,
    max_bytes: Option<u64>,
}

impl MemoryRefStore {
    /// An unbounded store on the system clock.
    pub fn new() -> MemoryRefStore {
        MemoryRefStore {
            entries: Mutex::new(HashMap::new()),
            clock: Clock::system(),
            max_bytes: None,
        }
    }

    /// Drive TTLs from `clock` instead of the host clock.
    pub fn with_clock(mut self, clock: Clock) -> MemoryRefStore {
        self.clock = clock;
        self
    }

    /// Evict least-recently-accessed entries once live fragments exceed
    /// `max_bytes`.
    pub fn with_max_bytes(mut self, max_bytes: u64) -> MemoryRefStore {
        self.max_bytes = Some(max_bytes);
        self
    }

    /// Evict by last access until live bytes fit under the cap.
    ///
    /// Eviction goes to 90% of the cap rather than exactly to it so a store
    /// sitting at the limit does not evict once per write for the rest of its
    /// life.
    fn enforce_byte_cap(&self, entries: &mut HashMap<(String, RefId), Entry>, now: u64) {
        let Some(cap) = self.max_bytes else {
            return;
        };
        let live: u64 = entries
            .values()
            .filter(|e| !is_expired(&e.record, e.max_lifetime_seconds, now))
            .map(|e| e.record.bytes())
            .sum();
        if live <= cap {
            return;
        }

        let target = cap / 10 * 9;
        // Live rows only, matching `live` above. An expired entry among the
        // candidates had its bytes subtracted from a total that never included
        // them, so the loop stopped while live bytes were still over the cap.
        // Reclaiming expired entries is `purge_expired`'s job, not this one's.
        let mut candidates: Vec<((String, RefId), u64, u64)> = entries
            .iter()
            .filter(|(_, entry)| !is_expired(&entry.record, entry.max_lifetime_seconds, now))
            .map(|(key, entry)| {
                (
                    key.clone(),
                    entry.record.last_accessed,
                    entry.record.bytes(),
                )
            })
            .collect();
        // The tie-break on the key keeps eviction deterministic when a burst of
        // writes lands in the same second, which is the normal case: one
        // compression stores every fragment it elided at once.
        candidates.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

        let mut remaining = live;
        for (key, _, bytes) in candidates {
            if remaining <= target {
                break;
            }
            entries.remove(&key);
            remaining = remaining.saturating_sub(bytes);
        }
        tracing::debug!(
            live_bytes_before = live,
            live_bytes_after = remaining,
            "evicted least-recently-accessed fragments to fit the byte cap"
        );
    }
}

impl Default for MemoryRefStore {
    fn default() -> MemoryRefStore {
        MemoryRefStore::new()
    }
}

impl RefStore for MemoryRefStore {
    fn put_many(
        &self,
        tenant: &Tenant,
        refs: &[(RefId, String)],
        meta: &PutMeta,
    ) -> Result<(), StoreError> {
        let now = self.clock.now();
        let mut entries = self.entries.lock();
        for (id, fragment) in refs {
            let key = (tenant.0.clone(), *id);
            match entries.get_mut(&key) {
                Some(existing) => {
                    // A row past its window and not yet swept is no row: its
                    // `created_at` must not make the new handle born expired.
                    if is_expired(&existing.record, existing.max_lifetime_seconds, now) {
                        existing.record.created_at = now;
                    }
                    existing.record.fragment.clear();
                    existing.record.fragment.push_str(fragment);
                    existing.record.kind = meta.kind.as_str().to_owned();
                    existing.record.last_accessed = now;
                    existing.record.ttl_seconds = meta.ttl_seconds;
                    existing.max_lifetime_seconds = meta.max_lifetime_seconds;
                    // A re-store with no request id must not erase the
                    // attribution of the request that first paid for the
                    // elision, which is what the stats join reads.
                    if meta.request_id.is_some() {
                        existing.record.request_id = meta.request_id.clone();
                    }
                }
                None => {
                    entries.insert(
                        key,
                        Entry {
                            record: RefRecord {
                                id: *id,
                                fragment: fragment.clone(),
                                kind: meta.kind.as_str().to_owned(),
                                request_id: meta.request_id.clone(),
                                created_at: now,
                                last_accessed: now,
                                ttl_seconds: meta.ttl_seconds,
                            },
                            max_lifetime_seconds: meta.max_lifetime_seconds,
                        },
                    );
                }
            }
        }
        self.enforce_byte_cap(&mut entries, now);
        Ok(())
    }

    fn get(&self, tenant: &Tenant, id: &RefId, touch: bool) -> Result<Lookup, StoreError> {
        let now = self.clock.now();
        let key = (tenant.0.clone(), *id);
        let mut entries = self.entries.lock();
        let Some(entry) = entries.get_mut(&key) else {
            return Ok(Lookup::Miss);
        };
        if is_expired(&entry.record, entry.max_lifetime_seconds, now) {
            let expired_at = crate::types::expires_at(&entry.record, entry.max_lifetime_seconds);
            // Lazy purge: a dead row read is a dead row deleted, so the sweep
            // never has to catch up with the handles that are actually asked for.
            entries.remove(&key);
            return Ok(Lookup::Expired { expired_at });
        }
        if touch {
            entry.record.last_accessed = now;
        }
        Ok(Lookup::Hit(entry.record.clone()))
    }

    fn touch_many(&self, tenant: &Tenant, ids: &[RefId]) -> Result<usize, StoreError> {
        let now = self.clock.now();
        let mut entries = self.entries.lock();
        let mut touched = 0;
        for id in ids {
            let key = (tenant.0.clone(), *id);
            if let Some(entry) = entries.get_mut(&key) {
                if is_expired(&entry.record, entry.max_lifetime_seconds, now) {
                    continue;
                }
                entry.record.last_accessed = now;
                touched += 1;
            }
        }
        Ok(touched)
    }

    fn purge_expired(&self, now: u64) -> Result<u64, StoreError> {
        let mut entries = self.entries.lock();
        let before = entries.len();
        entries.retain(|_, entry| !is_expired(&entry.record, entry.max_lifetime_seconds, now));
        let removed = (before - entries.len()) as u64;
        self.enforce_byte_cap(&mut entries, now);
        Ok(removed)
    }

    fn stats(&self) -> Result<StoreStats, StoreError> {
        self.tally(None)
    }

    fn stats_for(&self, tenant: &Tenant) -> Result<StoreStats, StoreError> {
        self.tally(Some(tenant.0.as_str()))
    }
}

impl MemoryRefStore {
    fn tally(&self, only: Option<&str>) -> Result<StoreStats, StoreError> {
        let now = self.clock.now();
        let entries = self.entries.lock();
        let mut stats = StoreStats::default();
        for ((owner, _), entry) in entries.iter() {
            if only.is_some_and(|wanted| owner != wanted) {
                continue;
            }
            if is_expired(&entry.record, entry.max_lifetime_seconds, now) {
                continue;
            }
            stats.live_refs += 1;
            stats.live_bytes += entry.record.bytes();
            stats.oldest_created_at = Some(match stats.oldest_created_at {
                Some(oldest) => oldest.min(entry.record.created_at),
                None => entry.record.created_at,
            });
        }
        Ok(stats)
    }
}

#[cfg(test)]
mod byte_cap_tests {
    use super::*;
    use crate::clock::ManualClock;
    use crate::types::PutMeta;
    use kompressor_engine::refs::ref_for;
    use kompressor_engine::types::Kind;

    /// The byte cap counts live bytes, so eviction must spend live bytes.
    ///
    /// Candidates used to include expired rows. Deleting one subtracted its
    /// bytes from a running total that never included them, so the loop
    /// stopped early and left the store over its cap — between purges, on an
    /// ordinary write, with no error anywhere. The expired row is still there
    /// afterwards; reclaiming it is `purge_expired`'s job, and this asserts the
    /// cap rather than the row count.
    #[test]
    fn an_expired_row_does_not_pay_for_a_live_one() {
        let clock = ManualClock::at(100);
        let store = MemoryRefStore::new()
            .with_clock(clock.clock())
            .with_max_bytes(100);
        let tenant = Tenant::new("t");

        let dead = "x".repeat(100);
        store
            .put_many(
                &tenant,
                &[(ref_for(&dead), dead.clone())],
                &PutMeta::new(Kind::Log).with_ttl(1),
            )
            .unwrap();

        clock.set(103);
        for fill in ["a", "b"] {
            let live = fill.repeat(60);
            store
                .put_many(
                    &tenant,
                    &[(ref_for(&live), live.clone())],
                    &PutMeta::new(Kind::Log).with_ttl(10_000),
                )
                .unwrap();
        }

        let live_bytes = store.stats().unwrap().live_bytes;
        assert!(
            live_bytes <= 100,
            "cap is 100 and eviction left {live_bytes} live bytes"
        );
    }
}
