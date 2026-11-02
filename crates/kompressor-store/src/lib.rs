//! Tenant-scoped, TTL-bounded storage for the fragments behind CCR handles.
//!
//! In recoverable mode the engine replaces an elided span with a marker that
//! names a handle, and the model can ask for what was removed. That promise is
//! only as good as this crate: a handle whose fragment is gone is an invitation
//! to spend a turn asking for something that cannot be returned.
//!
//! # The TTL is an idle window
//!
//! An entry's clock restarts on every successful read (`get` with `touch`, or
//! [`RefStore::touch_many`]), and an absolute lifetime measured from
//! `created_at` bounds how long that can go on, so constant access cannot pin a
//! fragment forever.
//!
//! The sliding window closes a specific gap. Refreshing a fragment's TTL only
//! as a side effect of *re-compressing* the message that carries its marker
//! sounds sufficient and is not: a marker that drifts into content the request
//! declines to re-compress — a frozen prefix, or the model quoting the marker
//! back in its own turn — stops being refreshed. The marker lives on in the
//! conversation while its fragment ages out underneath
//! it. Refreshing on *access* fixes that directly, and
//! [`RefStore::touch_many`] lets a caller refresh handles it merely saw.
//!
//! [`Lookup::Expired`] is kept distinct from [`Lookup::Miss`] for the same
//! reason: "we held this and let it go" and "this was never ours" deserve
//! different answers to the model and different rows in the stats.
//!
//! # Backends
//!
//! - [`SqliteRefStore`] — the production default; one WAL file.
//! - [`MemoryRefStore`] — the test default, and what `--ephemeral` runs.
//! - [`CachedRefStore`] — a write-through cache to wrap either, covering the
//!   retrieval that lands while a write is still in flight.
//!
//! # Security
//!
//! The tenant is part of every key, so a handle that leaks across tenants
//! addresses a row that does not exist — there is no read-then-compare step for
//! a refactor to lose. Fragments are tool output and may contain secrets:
//! nothing here logs a fragment body at any level, and [`RefRecord`]'s `Debug`
//! renders the fragment's size instead of its bytes so no error chain or trace
//! field can leak one by accident.

#![forbid(unsafe_code)]

pub mod browse;
pub mod cache;
pub mod clock;
pub mod memory;
pub mod sqlite;
pub mod types;

pub use browse::{BrowseOrder, BrowsePage, BrowseQuery, RefBrowse, RefDetail, RefSummary};
pub use cache::CachedRefStore;
pub use clock::{Clock, ManualClock};
pub use memory::MemoryRefStore;
pub use sqlite::{SqliteOptions, SqliteRefStore};
pub use types::{
    DEFAULT_LIFETIME_MULTIPLIER, DEFAULT_MAX_LIFETIME_SECONDS, DEFAULT_TTL_SECONDS, Lookup,
    PutMeta, RefRecord, RefStore, StoreError, StoreStats, Tenant, expires_at, is_expired,
};

#[cfg(test)]
mod tests {
    use super::*;
    use kompressor_engine::refs::{RefId, ref_for};
    use kompressor_engine::types::Kind;
    use tempfile::TempDir;

    /// One backend under test, with the clock driving its TTLs.
    struct Fixture {
        store: Box<dyn RefStore>,
        clock: ManualClock,
        // dropped last, so the SQLite file outlives the store
        _dir: Option<TempDir>,
    }

    /// How a conformance body builds its backend: starting instant, byte cap.
    type Make = fn(u64, Option<u64>) -> Fixture;

    impl Fixture {
        fn memory(now: u64, max_bytes: Option<u64>) -> Fixture {
            let clock = ManualClock::at(now);
            let mut store = MemoryRefStore::new().with_clock(clock.clock());
            if let Some(cap) = max_bytes {
                store = store.with_max_bytes(cap);
            }
            Fixture {
                store: Box::new(store),
                clock,
                _dir: None,
            }
        }

        fn sqlite(now: u64, max_bytes: Option<u64>) -> Fixture {
            let clock = ManualClock::at(now);
            let dir = TempDir::new().expect("a temp dir");
            let store = SqliteRefStore::open_with(
                dir.path().join("refs.db"),
                SqliteOptions {
                    clock: clock.clock(),
                    max_bytes,
                },
            )
            .expect("a fresh refs.db opens");
            Fixture {
                store: Box::new(store),
                clock,
                _dir: Some(dir),
            }
        }

        fn store(&self) -> &dyn RefStore {
            self.store.as_ref()
        }
    }

    /// Generate one `#[test]` per backend from each shared body.
    ///
    /// The bodies state guarantees of the trait, not of an implementation, so
    /// running them against only one backend would leave the other free to
    /// disagree — and the two are chosen per deployment, not per behaviour.
    macro_rules! against_every_backend {
        ($($body:ident),+ $(,)?) => {$(
            mod $body {
                #[test]
                fn memory() {
                    super::$body(super::Fixture::memory as super::Make);
                }

                #[test]
                fn sqlite() {
                    super::$body(super::Fixture::sqlite as super::Make);
                }
            }
        )+};
    }

    against_every_backend!(
        a_stored_fragment_round_trips_by_its_handle,
        an_unknown_handle_is_a_miss_rather_than_an_error,
        a_handle_from_another_tenant_does_not_resolve,
        two_tenants_holding_the_same_handle_expire_independently,
        re_storing_a_handle_refreshes_the_clock_without_moving_created_at,
        a_re_store_without_a_request_id_keeps_the_original_attribution,
        a_read_inside_the_idle_window_extends_it,
        a_read_that_declines_to_touch_leaves_the_idle_window_alone,
        an_entry_past_its_idle_window_reports_expired_rather_than_missing,
        an_expired_entry_is_dropped_by_the_read_that_reported_it,
        an_entry_that_keeps_being_read_still_dies_at_the_lifetime_cap,
        touch_many_extends_several_at_once_and_reports_how_many_it_found,
        touching_does_not_resurrect_an_expired_entry,
        purge_expired_removes_exactly_the_dead_rows_and_returns_the_count,
        the_byte_cap_evicts_the_least_recently_accessed_first,
        stats_reports_live_rows_and_bytes,
        an_empty_batch_is_accepted_and_changes_nothing,
        browsing_lists_only_the_callers_tenant_newest_first,
        browsing_hides_expired_rows_unless_asked_and_never_touches,
        browsing_filters_by_search_kind_request_and_handle,
        browsing_pages_and_orders_deterministically,
        a_browse_preview_is_cut_on_a_character_boundary,
        a_search_preview_is_a_window_around_the_first_match,
        inspecting_an_expired_row_neither_deletes_nor_touches_it,
        removing_deletes_only_the_named_tenants_rows,
    );

    fn meta() -> PutMeta {
        PutMeta::new(Kind::Log)
    }

    /// `ttl` seconds of idle window, `max_lifetime` seconds of total life.
    fn meta_with(ttl: u64, max_lifetime: u64) -> PutMeta {
        PutMeta::new(Kind::Log)
            .with_ttl(ttl)
            .with_max_lifetime(max_lifetime)
    }

    fn put(store: &dyn RefStore, tenant: &Tenant, fragment: &str, meta: &PutMeta) -> RefId {
        let id = ref_for(fragment);
        store
            .put_many(tenant, &[(id, fragment.to_owned())], meta)
            .expect("a put succeeds");
        id
    }

    fn a() -> Tenant {
        Tenant::new("acme")
    }

    fn b() -> Tenant {
        Tenant::new("beta")
    }

    fn a_stored_fragment_round_trips_by_its_handle(make: Make) {
        let f = make(1_000, None);
        let fragment = "{\"rows\": [1, 2, 3], \"truncated\": true}";
        let id = put(f.store(), &a(), fragment, &meta());

        match f.store().get(&a(), &id, true).unwrap() {
            Lookup::Hit(record) => {
                assert_eq!(record.fragment, fragment);
                assert_eq!(record.id, id);
                assert_eq!(record.kind, Kind::Log.as_str());
                assert_eq!(record.created_at, 1_000);
                assert_eq!(record.last_accessed, 1_000);
                assert_eq!(record.bytes(), fragment.len() as u64);
            }
            other => panic!("expected a hit, got {other:?}"),
        }
    }

    fn an_unknown_handle_is_a_miss_rather_than_an_error(make: Make) {
        let f = make(1_000, None);
        let never_stored = ref_for("nothing was ever stored under this");
        assert_eq!(
            f.store().get(&a(), &never_stored, true).unwrap(),
            Lookup::Miss
        );
    }

    fn a_handle_from_another_tenant_does_not_resolve(make: Make) {
        let f = make(1_000, None);
        let id = put(f.store(), &a(), "tenant a's tool output", &meta());

        // The handle is real and the row exists — under a different tenant.
        // Nothing in the store compares tenants after a read, so there is no
        // check here to forget: the key simply is not present.
        assert_eq!(f.store().get(&b(), &id, true).unwrap(), Lookup::Miss);
        assert!(f.store().get(&a(), &id, true).unwrap().is_hit());
        assert_eq!(f.store().touch_many(&b(), &[id]).unwrap(), 0);
    }

    fn two_tenants_holding_the_same_handle_expire_independently(make: Make) {
        let f = make(1_000, None);
        let fragment = "identical output from two tenants";
        let id = put(f.store(), &a(), fragment, &meta_with(100, 10_000));
        assert_eq!(
            put(f.store(), &b(), fragment, &meta_with(9_000, 10_000)),
            id
        );

        f.clock.advance(200);
        assert!(matches!(
            f.store().get(&a(), &id, true).unwrap(),
            Lookup::Expired { .. }
        ));
        assert_eq!(
            f.store().get(&b(), &id, true).unwrap().fragment(),
            Some(fragment)
        );
    }

    fn re_storing_a_handle_refreshes_the_clock_without_moving_created_at(make: Make) {
        let f = make(1_000, None);
        let fragment = "the same bytes, stored twice";
        let id = put(f.store(), &a(), fragment, &meta_with(100, 10_000));

        f.clock.advance(90);
        let id_again = put(f.store(), &a(), fragment, &meta_with(100, 10_000));
        assert_eq!(id_again, id, "content addressing: same bytes, same handle");

        // Past the first idle deadline (1_100) but inside the refreshed one.
        f.clock.advance(50);
        match f.store().get(&a(), &id, false).unwrap() {
            Lookup::Hit(record) => {
                assert_eq!(record.last_accessed, 1_090, "the re-store moved the clock");
                assert_eq!(
                    record.created_at, 1_000,
                    "created_at must survive the upsert or the lifetime cap never bites"
                );
            }
            other => panic!("expected a hit, got {other:?}"),
        }
    }

    fn a_re_store_without_a_request_id_keeps_the_original_attribution(make: Make) {
        let f = make(1_000, None);
        let fragment = "attributed once";
        let first = meta().with_request_id("req-1");
        let id = put(f.store(), &a(), fragment, &first);

        f.clock.advance(10);
        put(f.store(), &a(), fragment, &meta());

        match f.store().get(&a(), &id, false).unwrap() {
            Lookup::Hit(record) => assert_eq!(record.request_id.as_deref(), Some("req-1")),
            other => panic!("expected a hit, got {other:?}"),
        }
    }

    fn a_read_inside_the_idle_window_extends_it(make: Make) {
        let f = make(1_000, None);
        let id = put(f.store(), &a(), "still wanted", &meta_with(100, 10_000));

        f.clock.advance(90);
        assert!(f.store().get(&a(), &id, true).unwrap().is_hit());

        // 1_180 is past the original deadline of 1_100 and inside the restarted
        // one: the window slides with access, it is not a countdown from the
        // write.
        f.clock.advance(90);
        assert!(
            f.store().get(&a(), &id, true).unwrap().is_hit(),
            "a touching read must restart the idle window"
        );
    }

    fn a_read_that_declines_to_touch_leaves_the_idle_window_alone(make: Make) {
        let f = make(1_000, None);
        let id = put(
            f.store(),
            &a(),
            "inspected, not used",
            &meta_with(100, 10_000),
        );

        f.clock.advance(90);
        assert!(f.store().get(&a(), &id, false).unwrap().is_hit());

        f.clock.advance(20);
        assert!(
            matches!(
                f.store().get(&a(), &id, false).unwrap(),
                Lookup::Expired { .. }
            ),
            "an untouching read must not keep an entry alive"
        );
    }

    fn an_entry_past_its_idle_window_reports_expired_rather_than_missing(make: Make) {
        let f = make(1_000, None);
        let id = put(f.store(), &a(), "aged out", &meta_with(100, 10_000));

        f.clock.advance(101);
        assert_eq!(
            f.store().get(&a(), &id, true).unwrap(),
            Lookup::Expired { expired_at: 1_100 },
            "the caller has to tell 'we let it go' from 'never had it'"
        );
    }

    fn an_expired_entry_is_dropped_by_the_read_that_reported_it(make: Make) {
        let f = make(1_000, None);
        let id = put(f.store(), &a(), "aged out", &meta_with(100, 10_000));

        f.clock.advance(101);
        assert!(matches!(
            f.store().get(&a(), &id, true).unwrap(),
            Lookup::Expired { .. }
        ));
        // Lazy purge: the read that noticed the corpse also buried it, so the
        // sweep never has to keep up with the handles actually being asked for.
        assert_eq!(f.store().get(&a(), &id, true).unwrap(), Lookup::Miss);
        assert_eq!(f.store().stats().unwrap().live_refs, 0);
    }

    fn an_entry_that_keeps_being_read_still_dies_at_the_lifetime_cap(make: Make) {
        let f = make(1_000, None);
        let id = put(f.store(), &a(), "pinned by attention", &meta_with(100, 250));

        // Read often enough that the idle window alone would never lapse.
        for _ in 0..4 {
            f.clock.advance(50);
            assert!(f.store().get(&a(), &id, true).unwrap().is_hit());
        }

        f.clock.advance(51);
        assert_eq!(
            f.store().get(&a(), &id, true).unwrap(),
            Lookup::Expired { expired_at: 1_250 },
            "the absolute cap bounds the sliding window"
        );
    }

    fn touch_many_extends_several_at_once_and_reports_how_many_it_found(make: Make) {
        let f = make(1_000, None);
        let policy = meta_with(100, 10_000);
        let first = put(f.store(), &a(), "marker one", &policy);
        let second = put(f.store(), &a(), "marker two", &policy);
        let never_stored = ref_for("a handle the model invented");

        f.clock.advance(90);
        assert_eq!(
            f.store()
                .touch_many(&a(), &[first, second, never_stored])
                .unwrap(),
            2,
            "the count is what was found, not what was asked for"
        );

        // Both are now good until 1_190, past the original 1_100 deadline.
        f.clock.advance(90);
        assert!(f.store().get(&a(), &first, false).unwrap().is_hit());
        assert!(f.store().get(&a(), &second, false).unwrap().is_hit());
    }

    fn touching_does_not_resurrect_an_expired_entry(make: Make) {
        let f = make(1_000, None);
        let id = put(f.store(), &a(), "already gone", &meta_with(100, 10_000));

        f.clock.advance(101);
        assert_eq!(f.store().touch_many(&a(), &[id]).unwrap(), 0);
        assert!(
            matches!(
                f.store().get(&a(), &id, true).unwrap(),
                Lookup::Expired { .. } | Lookup::Miss
            ),
            "a touch must not bring a dead entry back"
        );
    }

    fn purge_expired_removes_exactly_the_dead_rows_and_returns_the_count(make: Make) {
        let f = make(1_000, None);
        let short = put(f.store(), &a(), "short lived", &meta_with(100, 10_000));
        let long = put(f.store(), &a(), "long lived", &meta_with(10_000, 10_000));
        let capped = put(f.store(), &b(), "capped", &meta_with(10_000, 150));

        // At 1_200: `short` is past its idle window, `capped` past its absolute
        // lifetime, `long` is fine. The sweep crosses tenants; reads never do.
        assert_eq!(f.store().purge_expired(1_200).unwrap(), 2);
        assert_eq!(f.store().get(&a(), &short, false).unwrap(), Lookup::Miss);
        assert_eq!(f.store().get(&b(), &capped, false).unwrap(), Lookup::Miss);
        assert!(f.store().get(&a(), &long, false).unwrap().is_hit());
        assert_eq!(f.store().purge_expired(1_200).unwrap(), 0, "idempotent");
    }

    fn the_byte_cap_evicts_the_least_recently_accessed_first(make: Make) {
        let f = make(1_000, Some(100));
        let policy = meta_with(10_000, 100_000);
        let oldest = put(f.store(), &a(), &"o".repeat(40), &policy);
        f.clock.advance(10);
        let middle = put(f.store(), &a(), &"m".repeat(40), &policy);

        f.clock.advance(10);
        // The third write puts live bytes at 120 over a cap of 100, so the
        // store evicts down to 90: only the oldest has to go.
        let newest = put(f.store(), &a(), &"n".repeat(40), &policy);

        assert_eq!(
            f.store().get(&a(), &oldest, false).unwrap(),
            Lookup::Miss,
            "eviction is a deletion, not an expiry"
        );
        assert!(f.store().get(&a(), &middle, false).unwrap().is_hit());
        assert!(f.store().get(&a(), &newest, false).unwrap().is_hit());
        assert_eq!(f.store().stats().unwrap().live_bytes, 80);
    }

    fn stats_reports_live_rows_and_bytes(make: Make) {
        let f = make(1_000, None);
        assert_eq!(f.store().stats().unwrap(), StoreStats::default());

        put(f.store(), &a(), "0123456789", &meta_with(100, 10_000));
        f.clock.advance(5);
        put(
            f.store(),
            &b(),
            "01234567890123456789",
            &meta_with(10_000, 10_000),
        );

        let stats = f.store().stats().unwrap();
        assert_eq!(stats.live_refs, 2);
        assert_eq!(stats.live_bytes, 30);
        assert_eq!(stats.oldest_created_at, Some(1_000));

        // An entry past its window stops counting even before a sweep runs.
        f.clock.advance(200);
        let stats = f.store().stats().unwrap();
        assert_eq!(stats.live_refs, 1);
        assert_eq!(stats.live_bytes, 20);
        assert_eq!(stats.oldest_created_at, Some(1_005));
    }

    fn an_empty_batch_is_accepted_and_changes_nothing(make: Make) {
        let f = make(1_000, None);
        f.store().put_many(&a(), &[], &meta()).unwrap();
        assert_eq!(f.store().touch_many(&a(), &[]).unwrap(), 0);
        assert_eq!(f.store().stats().unwrap(), StoreStats::default());
    }

    fn browser(f: &Fixture) -> &dyn RefBrowse {
        f.store().browser().expect("every backend can be browsed")
    }

    fn handles(page: &BrowsePage) -> Vec<RefId> {
        page.entries.iter().map(|entry| entry.id).collect()
    }

    fn browsing_lists_only_the_callers_tenant_newest_first(make: Make) {
        let f = make(1_000, None);
        let first = put(f.store(), &a(), "first", &meta());
        f.clock.advance(10);
        let second = put(f.store(), &a(), "second", &meta());
        put(f.store(), &b(), "someone else's", &meta());

        let page = browser(&f).browse(&a(), &BrowseQuery::default()).unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(handles(&page), vec![second, first]);
        assert_eq!(page.kinds, vec![("log".to_owned(), 2)]);
        let entry = &page.entries[1];
        assert_eq!(entry.preview, "first");
        assert!(!entry.truncated);
        assert_eq!(entry.bytes, 5);
        assert_eq!(entry.created_at, 1_000);
        assert_eq!(entry.expires_at, 1_000 + DEFAULT_TTL_SECONDS);
        assert!(!entry.expired);
    }

    fn browsing_hides_expired_rows_unless_asked_and_never_touches(make: Make) {
        let f = make(1_000, None);
        let short = put(f.store(), &a(), "short", &meta_with(100, 10_000));
        let long = put(f.store(), &a(), "long", &meta_with(10_000, 10_000));

        f.clock.advance(90);
        browser(&f).browse(&a(), &BrowseQuery::default()).unwrap();
        browser(&f).inspect(&a(), &short).unwrap();
        f.clock.advance(20);

        // Had the browse or the inspect touched `short`, it would be live at
        // 1_110; it is not, because looking is not using.
        let live = browser(&f).browse(&a(), &BrowseQuery::default()).unwrap();
        assert_eq!(handles(&live), vec![long]);
        let all = browser(&f)
            .browse(
                &a(),
                &BrowseQuery {
                    include_expired: true,
                    order: BrowseOrder::ExpiringSoon,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(handles(&all), vec![short, long]);
        assert!(all.entries[0].expired);
        assert_eq!(all.entries[0].expires_at, 1_100);
    }

    fn browsing_filters_by_search_kind_request_and_handle(make: Make) {
        let f = make(1_000, None);
        let secret = put(
            f.store(),
            &a(),
            "export AWS_SECRET=abc",
            &meta().with_request_id("req-7"),
        );
        let json = put(
            f.store(),
            &a(),
            "{\"rows\": []}",
            &PutMeta::new(Kind::Json).with_request_id("req-8"),
        );
        let search = |text: &str| {
            browser(&f)
                .browse(
                    &a(),
                    &BrowseQuery {
                        search: Some(text.to_owned()),
                        ..Default::default()
                    },
                )
                .unwrap()
        };

        assert_eq!(handles(&search("aws_secret")), vec![secret], "case folds");
        assert_eq!(handles(&search("REQ-8")), vec![json], "request ids match");
        assert_eq!(handles(&search(&secret.to_string())), vec![secret]);
        assert_eq!(handles(&search(&format!("ref:{secret}"))), vec![secret]);
        let partial = format!("k_{}", &json.body()[..6]);
        assert!(
            handles(&search(&partial)).contains(&json),
            "a partial handle"
        );
        assert_eq!(search("nothing like this").total, 0);
        assert_eq!(search("   ").total, 2, "blank is no filter");

        let by_kind = browser(&f)
            .browse(
                &a(),
                &BrowseQuery {
                    kind: Some("json".to_owned()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(handles(&by_kind), vec![json]);
        assert_eq!(by_kind.kinds.len(), 2, "kinds ignore the other filters");

        let by_request = browser(&f)
            .browse(
                &a(),
                &BrowseQuery {
                    request_id: Some("req-7".to_owned()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(handles(&by_request), vec![secret]);

        // Tenant b holds the same bytes under the same handle and still sees
        // only its own copy.
        put(f.store(), &b(), "export AWS_SECRET=abc", &meta());
        let other = browser(&f)
            .browse(
                &b(),
                &BrowseQuery {
                    request_id: Some("req-7".to_owned()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(other.total, 0);
    }

    fn browsing_pages_and_orders_deterministically(make: Make) {
        let f = make(1_000, None);
        // One batch, one second: only the handle can break the tie.
        let mut ids: Vec<RefId> = (0..5)
            .map(|n| put(f.store(), &a(), &"x".repeat(n + 1), &meta()))
            .collect();

        let largest = browser(&f)
            .browse(
                &a(),
                &BrowseQuery {
                    order: BrowseOrder::Largest,
                    limit: 2,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(largest.total, 5);
        assert_eq!(handles(&largest), vec![ids[4], ids[3]]);

        ids.sort_by(|x, y| x.body().cmp(y.body()));
        let page = |offset| {
            handles(
                &browser(&f)
                    .browse(
                        &a(),
                        &BrowseQuery {
                            order: BrowseOrder::Oldest,
                            offset,
                            limit: 2,
                            ..Default::default()
                        },
                    )
                    .unwrap(),
            )
        };
        assert_eq!(page(0), ids[..2]);
        assert_eq!(page(2), ids[2..4]);
        assert_eq!(page(4), ids[4..]);
        assert!(page(10).is_empty());

        let capped = browser(&f)
            .browse(
                &a(),
                &BrowseQuery {
                    limit: usize::MAX,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(capped.entries.len(), 5);
    }

    fn a_browse_preview_is_cut_on_a_character_boundary(make: Make) {
        let f = make(1_000, None);
        let fragment = "ёжик в тумане ".repeat(50);
        put(f.store(), &a(), &fragment, &meta());
        let page = browser(&f)
            .browse(
                &a(),
                &BrowseQuery {
                    preview_chars: 7,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(page.entries[0].preview, "ёжик в ");
        assert!(page.entries[0].truncated);
        assert_eq!(page.entries[0].bytes, fragment.len() as u64);
    }

    fn a_search_preview_is_a_window_around_the_first_match(make: Make) {
        let f = make(1_000, None);
        let fragment = format!("{}AWS_SECRET=x{}", "ё".repeat(300), "b".repeat(300));
        let deep = put(f.store(), &a(), &fragment, &meta().with_request_id("req-ё"));
        let browse = |search: &str| {
            browser(&f)
                .browse(
                    &a(),
                    &BrowseQuery {
                        search: Some(search.to_owned()),
                        preview_chars: 40,
                        ..Default::default()
                    },
                )
                .unwrap()
        };

        let page = browse("aws_secret");
        let entry = &page.entries[0];
        assert_eq!(entry.id, deep);
        assert_eq!(entry.preview_start, 290, "ten characters of lead-in");
        assert_eq!(
            entry.preview,
            format!("{}AWS_SECRET=x{}", "ё".repeat(10), "b".repeat(18))
        );
        assert!(entry.truncated);

        // A hit on the request id, not the text: the preview is the start.
        let page = browse("req-ё");
        assert_eq!(page.entries[0].preview_start, 0);
        assert_eq!(page.entries[0].preview, "ё".repeat(40));

        // A window that reaches the end of the fragment is not a cut.
        put(
            f.store(),
            &a(),
            &format!("{}THE END", "z".repeat(100)),
            &meta(),
        );
        let tail = browse("the end");
        assert_eq!(
            tail.entries[0].preview,
            format!("{}THE END", "z".repeat(10))
        );
        assert_eq!(tail.entries[0].preview_start, 90);
        assert!(!tail.entries[0].truncated);
    }

    fn inspecting_an_expired_row_neither_deletes_nor_touches_it(make: Make) {
        let f = make(1_000, None);
        let id = put(f.store(), &a(), "aged out", &meta_with(100, 10_000));
        f.clock.advance(150);

        let detail = browser(&f)
            .inspect(&a(), &id)
            .unwrap()
            .expect("still on disk");
        assert!(detail.expired);
        assert_eq!(detail.expires_at, 1_100);
        assert_eq!(detail.record.fragment, "aged out");
        assert_eq!(detail.max_lifetime_seconds, 10_000);
        assert!(
            browser(&f).inspect(&a(), &id).unwrap().is_some(),
            "not deleted"
        );
        assert!(browser(&f).inspect(&b(), &id).unwrap().is_none(), "scoped");
    }

    fn removing_deletes_only_the_named_tenants_rows(make: Make) {
        let f = make(1_000, None);
        let fragment = "held by both tenants";
        let id = put(f.store(), &a(), fragment, &meta());
        put(f.store(), &b(), fragment, &meta());
        let never = ref_for("never stored");

        assert_eq!(browser(&f).remove(&a(), &[id, never]).unwrap(), 1);
        assert_eq!(f.store().get(&a(), &id, false).unwrap(), Lookup::Miss);
        assert!(f.store().get(&b(), &id, false).unwrap().is_hit());
        assert_eq!(browser(&f).remove(&a(), &[id]).unwrap(), 0, "idempotent");
        assert_eq!(browser(&f).remove(&a(), &[]).unwrap(), 0);
    }

    #[test]
    fn a_record_never_renders_its_fragment_in_debug_output() {
        let store = MemoryRefStore::new();
        let secret = "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG";
        let id = put(&store, &a(), secret, &meta());

        let record = match store.get(&a(), &id, false).unwrap() {
            Lookup::Hit(record) => record,
            other => panic!("expected a hit, got {other:?}"),
        };
        let rendered = format!("{record:?}");
        assert!(
            !rendered.contains("wJalrXUtnFEMI"),
            "fragments are tool output: Debug must not carry the body"
        );
        assert!(rendered.contains("fragment_bytes"));

        // and the same holds through the enclosing Lookup
        assert!(!format!("{:?}", Lookup::Hit(record)).contains("wJalrXUtnFEMI"));
    }

    #[test]
    fn the_anonymous_tenant_is_the_empty_name() {
        assert!(Tenant::anonymous().is_anonymous());
        assert_eq!(Tenant::anonymous().as_str(), "");
        assert!(!Tenant::new("acme").is_anonymous());
    }

    #[test]
    fn an_absurd_ttl_does_not_wrap_into_the_past() {
        // u64::MAX means "never expire"; arithmetic that overflowed would turn
        // it into a deadline behind us and expire the entry on the spot.
        let f = Fixture::sqlite(1_000, None);
        let id = put(
            f.store(),
            &a(),
            "kept indefinitely",
            &PutMeta::new(Kind::Text)
                .with_ttl(u64::MAX)
                .with_max_lifetime(u64::MAX),
        );
        f.clock.set(u64::MAX / 2);
        assert!(f.store().get(&a(), &id, true).unwrap().is_hit());
    }
}
