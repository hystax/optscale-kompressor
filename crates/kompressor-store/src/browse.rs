//! Walking the store, for an operator looking at what it holds.
//!
//! [`RefStore`](crate::RefStore) is deliberately point-lookup only: serving a
//! handle never needs to enumerate the table, and nothing in the request path
//! should be able to. Browsing is a different job — someone asking "what did
//! the proxy elide from my session, and is that secret still in there?" — so it
//! is a separate trait, reached through [`RefStore::browser`](crate::RefStore::browser)
//! and only by the surfaces that serve the ref browser.
//!
//! Nothing here touches an entry. A fragment an operator glanced at is not a
//! fragment the model is using, and keeping it alive because somebody looked
//! would make the browser change the very retention it reports on.

use crate::types::{RefRecord, StoreError, Tenant, expires_at};
use kompressor_engine::refs::RefId;

/// The most entries one page may hold.
pub const MAX_PAGE: usize = 200;

/// The most preview characters an entry may carry.
pub const MAX_PREVIEW_CHARS: usize = 1_000;

/// How a page is ordered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BrowseOrder {
    /// Most recently created first.
    #[default]
    Newest,
    /// Oldest first.
    Oldest,
    /// Most recently written or read with `touch` first.
    RecentlyUsed,
    /// Most bytes first.
    Largest,
    /// The entry that stops being retrievable soonest first.
    ExpiringSoon,
}

impl BrowseOrder {
    /// The name the HTTP query uses.
    pub fn as_str(self) -> &'static str {
        match self {
            BrowseOrder::Newest => "newest",
            BrowseOrder::Oldest => "oldest",
            BrowseOrder::RecentlyUsed => "recent",
            BrowseOrder::Largest => "largest",
            BrowseOrder::ExpiringSoon => "expiring",
        }
    }

    /// The inverse of [`Self::as_str`].
    pub fn parse(name: &str) -> Option<BrowseOrder> {
        Some(match name {
            "newest" => BrowseOrder::Newest,
            "oldest" => BrowseOrder::Oldest,
            "recent" => BrowseOrder::RecentlyUsed,
            "largest" => BrowseOrder::Largest,
            "expiring" => BrowseOrder::ExpiringSoon,
            _ => return None,
        })
    }
}

/// Which entries a page lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowseQuery {
    /// Case-insensitive (ASCII) substring of the fragment, the handle or the
    /// request id. A complete handle matches that handle only.
    pub search: Option<String>,
    /// Exact content kind.
    pub kind: Option<String>,
    /// Exact request id.
    pub request_id: Option<String>,
    /// List rows past their window that no read or sweep has removed yet.
    pub include_expired: bool,
    pub order: BrowseOrder,
    pub offset: usize,
    /// Clamped to [`MAX_PAGE`].
    pub limit: usize,
    /// Characters of fragment per entry, clamped to [`MAX_PREVIEW_CHARS`].
    pub preview_chars: usize,
}

impl Default for BrowseQuery {
    fn default() -> BrowseQuery {
        BrowseQuery {
            search: None,
            kind: None,
            request_id: None,
            include_expired: false,
            order: BrowseOrder::Newest,
            offset: 0,
            limit: 50,
            preview_chars: 160,
        }
    }
}

impl BrowseQuery {
    pub(crate) fn page_limit(&self) -> usize {
        self.limit.min(MAX_PAGE)
    }

    pub(crate) fn preview_limit(&self) -> usize {
        self.preview_chars.min(MAX_PREVIEW_CHARS)
    }

    /// The search text, lowercased, or `None` when there is nothing to match.
    pub(crate) fn needle(&self) -> Option<String> {
        self.search
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_ascii_lowercase)
    }

    /// The search text as it would appear in a stored handle: the marker's
    /// `ref:` and the handle's `k_` are not part of the stored key.
    pub(crate) fn handle_needle(&self) -> Option<String> {
        let needle = self.needle()?;
        let bare = needle.strip_prefix("ref:").unwrap_or(&needle);
        let bare = bare.strip_prefix("k_").unwrap_or(bare);
        Some(bare.to_owned()).filter(|bare| !bare.is_empty())
    }

    /// The needle that searches fragment text, which an exact-handle search
    /// does not.
    pub(crate) fn text_needle(&self) -> Option<String> {
        if self.exact_handle().is_some() {
            return None;
        }
        self.needle()
    }

    /// The handle the search names, when it is exactly one.
    pub(crate) fn exact_handle(&self) -> Option<RefId> {
        let text = self.search.as_deref()?.trim();
        RefId::parse(text.strip_prefix("ref:").unwrap_or(text))
    }
}

/// One row of a page: everything but the fragment itself.
#[derive(Clone, PartialEq, Eq)]
pub struct RefSummary {
    pub id: RefId,
    pub kind: String,
    pub request_id: Option<String>,
    pub bytes: u64,
    pub created_at: u64,
    pub last_accessed: u64,
    pub ttl_seconds: u64,
    pub max_lifetime_seconds: u64,
    /// When the entry stops being retrievable unless it is read first.
    pub expires_at: u64,
    pub expired: bool,
    /// A window of the fragment, cut on character boundaries: its start, or,
    /// under a search that matched the text, a little before the first match.
    pub preview: String,
    /// Characters of fragment before the preview; non-zero only for a match
    /// window.
    pub preview_start: u64,
    /// Whether fragment text follows the preview.
    pub truncated: bool,
}

impl std::fmt::Debug for RefSummary {
    /// The preview is fragment text, so it is rendered by length only, for
    /// the same reason [`RefRecord`]'s `Debug` is.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefSummary")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("bytes", &self.bytes)
            .field("expired", &self.expired)
            .field("preview_chars", &self.preview.chars().count())
            .finish_non_exhaustive()
    }
}

/// A page of entries, and what the whole match looks like.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BrowsePage {
    /// Entries matching the query, across every page.
    pub total: u64,
    pub entries: Vec<RefSummary>,
    /// Entry count per kind for the tenant, ignoring every filter but expiry,
    /// so a kind picker can offer what exists rather than what is on screen.
    pub kinds: Vec<(String, u64)>,
}

/// One entry in full, read without touching it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefDetail {
    pub record: RefRecord,
    pub max_lifetime_seconds: u64,
    pub expires_at: u64,
    pub expired: bool,
}

impl RefDetail {
    pub(crate) fn new(record: RefRecord, max_lifetime_seconds: u64, now: u64) -> RefDetail {
        let deadline = expires_at(&record, max_lifetime_seconds);
        RefDetail {
            record,
            max_lifetime_seconds,
            expires_at: deadline,
            expired: deadline < now,
        }
    }
}

/// Listing, inspecting and deleting entries. See the module docs.
pub trait RefBrowse: Send + Sync {
    /// One page of `tenant`'s entries.
    fn browse(&self, tenant: &Tenant, query: &BrowseQuery) -> Result<BrowsePage, StoreError>;

    /// One entry, expired or not, without restarting its idle window and
    /// without the lazy delete a [`RefStore::get`](crate::RefStore::get) of
    /// an expired row performs.
    fn inspect(&self, tenant: &Tenant, id: &RefId) -> Result<Option<RefDetail>, StoreError>;

    /// Delete entries outright, returning how many existed.
    ///
    /// For an operator who found something in a fragment that should never
    /// have been stored. The model asking for a removed handle gets a miss,
    /// which it already knows how to handle.
    fn remove(&self, tenant: &Tenant, ids: &[RefId]) -> Result<usize, StoreError>;
}

/// The start of `fragment`, at most `chars` characters, and whether it was cut.
pub(crate) fn preview(fragment: &str, chars: usize) -> (String, bool) {
    match fragment.char_indices().nth(chars) {
        Some((at, _)) => (fragment[..at].to_owned(), true),
        None => (fragment.to_owned(), false),
    }
}

/// How much of a match window comes before the match: a quarter, so the
/// match reads in context without pushing it off the end of a short preview.
pub(crate) fn lead_in(chars: usize) -> usize {
    chars / 4
}

/// The preview of `fragment` for a search for `needle` (lowercased): a
/// window starting [`lead_in`] characters before the first match, or the
/// start of the fragment when the text does not match. Returns the window,
/// how many characters precede it and whether text follows it.
pub(crate) fn snippet(fragment: &str, needle: Option<&str>, chars: usize) -> (String, u64, bool) {
    // ASCII folding keeps byte offsets, so a hit in the folded copy is the
    // same offset in the original.
    let start = needle
        .and_then(|needle| fragment.to_ascii_lowercase().find(needle))
        .map_or(0, |at| {
            fragment[..at]
                .chars()
                .count()
                .saturating_sub(lead_in(chars))
        });
    let from = fragment
        .char_indices()
        .nth(start)
        .map_or(fragment.len(), |(at, _)| at);
    let (window, cut) = preview(&fragment[from..], chars);
    (window, start as u64, cut)
}

/// Whether `record` matches `query`'s filters. The in-memory reading of the
/// `WHERE` clause the SQLite backend builds; the two must agree.
pub(crate) fn matches(record: &RefRecord, query: &BrowseQuery, needle: Option<&str>) -> bool {
    if query
        .kind
        .as_deref()
        .is_some_and(|kind| record.kind != kind)
    {
        return false;
    }
    if let Some(wanted) = query.request_id.as_deref()
        && record.request_id.as_deref() != Some(wanted)
    {
        return false;
    }
    if let Some(id) = query.exact_handle() {
        return record.id == id;
    }
    let Some(needle) = needle else {
        return true;
    };
    contains_ascii_ci(&record.fragment, needle)
        || query
            .handle_needle()
            .is_some_and(|handle| contains_ascii_ci(record.id.body(), &handle))
        || record
            .request_id
            .as_deref()
            .is_some_and(|request| contains_ascii_ci(request, needle))
}

/// Substring match folding ASCII case only — what SQLite's `lower()` does,
/// so both backends answer the same search the same way.
fn contains_ascii_ci(haystack: &str, lowered_needle: &str) -> bool {
    haystack.to_ascii_lowercase().contains(lowered_needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preview_is_cut_on_a_character_boundary() {
        assert_eq!(preview("héllo wörld", 4), ("héll".to_owned(), true));
        assert_eq!(preview("short", 10), ("short".to_owned(), false));
        assert_eq!(preview("exact", 5), ("exact".to_owned(), false));
        assert_eq!(preview("", 0), (String::new(), false));
    }

    #[test]
    fn a_search_preview_is_a_window_around_the_first_match() {
        let fragment = format!("{}NEEDLE{}", "a".repeat(100), "b".repeat(100));
        let (window, start, cut) = snippet(&fragment, Some("needle"), 20);
        assert_eq!(start, 95, "five characters of lead-in");
        assert_eq!(window, "aaaaaNEEDLEbbbbbbbbb");
        assert!(cut);
        // no match in the text: the start, as without a search
        assert_eq!(
            snippet(&fragment, Some("zzz"), 5),
            ("aaaaa".to_owned(), 0, true)
        );
        assert_eq!(snippet("short", None, 50), ("short".to_owned(), 0, false));
        // a match near the start is not pushed before it
        assert_eq!(
            snippet("NEEDLE", Some("needle"), 50),
            ("NEEDLE".to_owned(), 0, false)
        );
        // character, not byte, arithmetic
        let (window, start, _) = snippet("ёёёёёёёёёёX", Some("x"), 4);
        assert_eq!((window.as_str(), start), ("ёX", 9));
    }

    #[test]
    fn orders_round_trip_through_their_names() {
        for order in [
            BrowseOrder::Newest,
            BrowseOrder::Oldest,
            BrowseOrder::RecentlyUsed,
            BrowseOrder::Largest,
            BrowseOrder::ExpiringSoon,
        ] {
            assert_eq!(BrowseOrder::parse(order.as_str()), Some(order));
        }
        assert_eq!(BrowseOrder::parse("random"), None);
    }

    #[test]
    fn a_whole_handle_is_an_exact_match_with_or_without_the_marker_prefix() {
        let id = kompressor_engine::refs::ref_for("x");
        for text in [id.to_string(), format!("ref:{id}"), id.body().to_owned()] {
            let query = BrowseQuery {
                search: Some(text),
                ..Default::default()
            };
            assert_eq!(query.exact_handle(), Some(id));
        }
    }
}
