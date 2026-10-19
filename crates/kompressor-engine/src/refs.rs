//! Content-addressed fragment handles (CCR) and the per-request collector.
//!
//! Each elision site — an omitted array slice, a clipped string tail, folded
//! log lines — hands the *original* fragment to [`RefCollector::stash`] and
//! embeds the returned handle in its marker. The collector accumulates
//! `(handle, fragment)` pairs; persisting them is the caller's job, so the
//! engine stays free of I/O.
//!
//! Handles are a truncated BLAKE2b digest of the fragment, **not** a random
//! token, and that is load-bearing rather than tidy: an agent re-sends its
//! whole history every turn and the proxy re-compresses it, so a random id
//! would rewrite every marker on every turn — bursting the provider's prompt
//! cache, which is where most of the money is — and would insert a duplicate
//! store row per elision per turn. With content addressing, unchanged history
//! re-emits byte-identical markers and re-storing a fragment is an idempotent
//! overwrite that refreshes its TTL.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use blake2::Blake2b;
use blake2::Digest;
use blake2::digest::consts::U9;
use std::collections::HashSet;
use std::fmt;

/// Every handle reads `k_` + [`HANDLE_WIDTH`] url-safe characters.
pub const REF_PREFIX: &str = "k_";
/// The digest width, as a type: blake2 0.11 takes it as a const generic rather
/// than a runtime argument. Nine bytes encode to exactly twelve base64
/// characters with no padding, which is [`HANDLE_WIDTH`] — change one and the
/// other stops being true, so `a_handle_is_twelve_url_safe_characters` checks
/// the pair rather than either alone.
type DigestWidth = U9;
pub const HANDLE_WIDTH: usize = 12;

/// A fragment handle, stored without its `k_` prefix.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RefId([u8; HANDLE_WIDTH]);

impl RefId {
    /// The 12 payload characters, without the prefix.
    pub fn body(&self) -> &str {
        // every byte came from the base64 alphabet
        std::str::from_utf8(&self.0).expect("handle is ascii")
    }

    /// Parse `k_XXXXXXXXXXXX` or the bare 12-character body.
    pub fn parse(s: &str) -> Option<RefId> {
        let body = s.strip_prefix(REF_PREFIX).unwrap_or(s);
        if body.len() != HANDLE_WIDTH || !body.bytes().all(is_handle_byte) {
            return None;
        }
        let mut out = [0u8; HANDLE_WIDTH];
        out.copy_from_slice(body.as_bytes());
        Some(RefId(out))
    }
}

impl fmt::Display for RefId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REF_PREFIX)?;
        f.write_str(self.body())
    }
}

impl fmt::Debug for RefId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RefId({self})")
    }
}

impl serde::Serialize for RefId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

#[inline]
fn is_handle_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// The handle for `fragment`: BLAKE2b with a 9-byte digest, url-safe base64.
///
/// The digest length is a BLAKE2 parameter rather than a truncation, so this
/// is `blake2b(data, digest_size=9)`, base64url with no padding.
pub fn ref_for(fragment: &str) -> RefId {
    // blake2 0.11 replaced the runtime-sized `Blake2bVar` with a const-generic
    // width. Same function, same bytes: the digest length is a BLAKE2
    // parameter either way, so `Blake2b::<U9>` is `blake2b(data, 9)`.
    let mut hasher = Blake2b::<DigestWidth>::new();
    hasher.update(fragment.as_bytes());
    let digest = hasher.finalize();
    let mut encoded = [0u8; HANDLE_WIDTH];
    let written = URL_SAFE_NO_PAD
        .encode_slice(digest, &mut encoded)
        .expect("9 bytes encode to 12 base64 characters");
    debug_assert_eq!(written, HANDLE_WIDTH);
    RefId(encoded)
}

/// Handles present in `content`, deduplicated in first-seen order.
///
/// This runs over every protected message, which on an agent turn is most of
/// the payload (the system prompt plus the model's own turns) and almost never
/// holds a handle — so the literal `ref:` is checked first with `memmem`, which
/// rules the same bytes out far faster than a scan can.
///
/// The shape is what keeps ordinary prose out: `refs/heads/main` and a bare
/// `git ref:abcdef123456` are common in agent transcripts and match neither the
/// `k_` prefix nor the fixed width.
pub fn refs_in(content: &str) -> Vec<RefId> {
    const NEEDLE: &[u8] = b"ref:k_";
    if content.len() < NEEDLE.len() {
        return Vec::new();
    }
    let bytes = content.as_bytes();
    let mut found: Vec<RefId> = Vec::new();
    let mut seen: HashSet<RefId> = HashSet::new();
    for pos in memchr::memmem::find_iter(bytes, NEEDLE) {
        // `\b` before `ref`: the preceding byte must not be a word character.
        if pos > 0 {
            let prev = bytes[pos - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                continue;
            }
        }
        let start = pos + NEEDLE.len();
        let end = start + HANDLE_WIDTH;
        if end > bytes.len() {
            continue;
        }
        if !bytes[start..end].iter().copied().all(is_handle_byte) {
            continue;
        }
        // a longer run of handle characters is not a handle
        if bytes.get(end).copied().is_some_and(is_handle_byte) {
            continue;
        }
        let mut body = [0u8; HANDLE_WIDTH];
        body.copy_from_slice(&bytes[start..end]);
        let id = RefId(body);
        if seen.insert(id) {
            found.push(id);
        }
    }
    found
}

/// Per-request, in-memory accumulator of elided fragments.
///
/// A *disabled* collector is how aggressive and lossless modes are expressed:
/// `stash` always answers `None`, every compressor emits its plain marker, and
/// no caller has to thread an `Option` through the whole engine.
#[derive(Clone, Debug)]
pub struct RefCollector {
    enabled: bool,
    max_ref_bytes: usize,
    pairs: Vec<(RefId, String)>,
    seen: HashSet<RefId>,
    oversized_attempts: usize,
    /// Every fragment refused for size, for reporting rather than for control.
    ///
    /// Separate from `oversized_attempts`, which is the *veto* signal and is
    /// deliberately restored by `rollback` and retracted by
    /// `forgive_oversized` — both of which happen at exactly the moments a
    /// report would want to know. This one is sticky: it answers "did the
    /// fragment limit cost this payload anything", whether that cost was the
    /// whole compression or one fold group that stayed inline.
    oversized_refusals: usize,
}

/// A point to roll a collector back to.
///
/// Both halves matter: the fragments stashed since the mark are dropped, and
/// the count of fragments refused for being oversized is restored. Without the
/// second, a candidate that lost an arbitration would still veto the one that
/// won it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    /// Fragments held when the mark was taken, and the index `pairs()` is
    /// sliced from to read what a candidate added.
    pub pairs: usize,
    oversized: usize,
}

impl RefCollector {
    /// A collector that retains fragments up to `max_ref_bytes` UTF-8 bytes.
    pub fn new(max_ref_bytes: usize) -> Self {
        RefCollector {
            enabled: true,
            max_ref_bytes,
            pairs: Vec::new(),
            seen: HashSet::new(),
            oversized_attempts: 0,
            oversized_refusals: 0,
        }
    }

    /// The aggressive/lossless collector: retains nothing, mints nothing.
    pub fn disabled() -> Self {
        RefCollector {
            enabled: false,
            max_ref_bytes: 0,
            pairs: Vec::new(),
            seen: HashSet::new(),
            oversized_attempts: 0,
            oversized_refusals: 0,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn max_ref_bytes(&self) -> usize {
        self.max_ref_bytes
    }

    /// Oversized fragments a recoverable candidate attempted to elide.
    pub fn oversized_attempts(&self) -> usize {
        self.oversized_attempts
    }

    /// Retain `fragment` and return its handle, or `None` when it is too large
    /// to keep — the caller then emits a plain, non-recoverable marker rather
    /// than a handle pointing at nothing.
    ///
    /// The cap is in UTF-8 bytes, matching what the store will hold.
    pub fn stash(&mut self, fragment: &str) -> Option<RefId> {
        if !self.enabled {
            return None;
        }
        if fragment.len() > self.max_ref_bytes {
            self.oversized_attempts += 1;
            self.oversized_refusals += 1;
            return None;
        }
        let id = ref_for(fragment);
        if self.seen.insert(id) {
            self.pairs.push((id, fragment.to_owned()));
        }
        Some(id)
    }

    /// A position to roll back to.
    pub fn mark(&self) -> Mark {
        Mark {
            pairs: self.pairs.len(),
            oversized: self.oversized_attempts,
        }
    }

    /// Retract a refusal the caller dealt with itself.
    ///
    /// The refusal count exists so the pipeline can veto a candidate that
    /// elided something it could not retain. A caller that asked, was refused,
    /// and then *did not elide* — keeping every occurrence, or shipping a
    /// marker that reconstructs without a fragment — shipped nothing
    /// unfetchable, so its refusal must not veto the payload. Counting it
    /// anyway discarded 97.88% of a payload whose log fold had already declined
    /// the one group it could not keep and folded the other two cleanly.
    ///
    /// Saturating: a retraction without a matching refusal is a no-op rather
    /// than a wrap into a count that would veto everything forever.
    /// Fragments refused for size, including ones a compressor then handled.
    pub fn oversized_refusals(&self) -> usize {
        self.oversized_refusals
    }

    /// Carry a dropped candidate's refusals across for reporting.
    ///
    /// A candidate that is disqualified is never adopted, so without this the
    /// one refusal that explains why the payload came back whole disappears
    /// with it.
    pub fn absorb_oversized_refusals(&mut self, other: &RefCollector) {
        self.oversized_refusals += other.oversized_refusals;
    }

    pub fn forgive_oversized(&mut self) {
        self.oversized_attempts = self.oversized_attempts.saturating_sub(1);
    }

    /// Discard everything stashed since `mark`.
    ///
    /// Used when the never-larger guard, the accounted guard or an arbitration
    /// rejects a candidate compression, so the store never holds fragments no
    /// marker points at.
    pub fn rollback(&mut self, mark: Mark) {
        for (id, _) in self.pairs.drain(mark.pairs..) {
            self.seen.remove(&id);
        }
        // The refusals go back too. A candidate that was discarded did not
        // ship, so neither did anything it could not store — and leaving the
        // count raised lets a losing reading veto a winning one whose
        // fragments were all within the cap.
        self.oversized_attempts = mark.oversized;
    }

    /// An empty collector with the same capacity rules, for one candidate of an
    /// arbitration. The loser's fragments are dropped; the winner's are
    /// replayed with [`RefCollector::adopt`].
    pub fn scratch(&self) -> RefCollector {
        RefCollector {
            enabled: self.enabled,
            max_ref_bytes: self.max_ref_bytes,
            pairs: Vec::new(),
            seen: HashSet::new(),
            oversized_attempts: 0,
            oversized_refusals: 0,
        }
    }

    /// Replay a winning candidate's fragments into this collector.
    ///
    /// Content addressing makes the replay exact: a re-stashed fragment yields
    /// the handle its marker already carries.
    pub fn adopt(&mut self, other: RefCollector) {
        self.oversized_attempts += other.oversized_attempts;
        self.oversized_refusals += other.oversized_refusals;
        for (_, fragment) in other.pairs {
            self.stash(&fragment);
        }
    }

    /// Replay fragments taken from a previous `pairs()` snapshot.
    pub fn restash_all<S: AsRef<str>>(&mut self, fragments: &[S]) {
        for fragment in fragments {
            self.stash(fragment.as_ref());
        }
    }

    pub fn pairs(&self) -> &[(RefId, String)] {
        &self.pairs
    }

    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// Total UTF-8 bytes retained, for `stats.ref_bytes_emitted`.
    pub fn bytes(&self) -> u64 {
        self.pairs.iter().map(|(_, f)| f.len() as u64).sum()
    }

    pub fn into_pairs(self) -> Vec<(RefId, String)> {
        self.pairs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handle_is_twelve_url_safe_characters() {
        let id = ref_for("hello world");
        assert_eq!(id.body().len(), HANDLE_WIDTH);
        assert!(id.body().bytes().all(is_handle_byte));
        assert!(id.to_string().starts_with("k_"));
    }

    #[test]
    fn handles_are_content_addressed() {
        assert_eq!(ref_for("same payload"), ref_for("same payload"));
        assert_ne!(ref_for("alpha"), ref_for("beta"));
    }

    #[test]
    fn a_handle_round_trips_through_its_text_form() {
        let id = ref_for("fragment");
        assert_eq!(RefId::parse(&id.to_string()), Some(id));
        assert_eq!(RefId::parse(id.body()), Some(id));
        assert_eq!(RefId::parse("k_short"), None);
        assert_eq!(RefId::parse("k_thirteenchars!"), None);
    }

    #[test]
    fn the_scanner_finds_handles_in_every_marker_shape() {
        let id = ref_for("x");
        let text = format!(
            "[... 4 items omitted [ref:{id}]] and […[+9 chars ref:{id}]] and [×3 ref:{id}]"
        );
        assert_eq!(refs_in(&text), vec![id]);
    }

    #[test]
    fn the_scanner_leaves_ordinary_prose_alone() {
        assert!(refs_in("see refs/heads/main").is_empty());
        assert!(refs_in("git ref:abcdef123456789").is_empty(), "too long");
        assert!(
            refs_in("xref:k_abcdefghijkl").is_empty(),
            "no word boundary"
        );
        assert!(refs_in("no handles here").is_empty());
    }

    #[test]
    fn a_handle_whose_body_begins_with_the_prefix_round_trips() {
        // The body is 12 characters from `[A-Za-z0-9_-]`, so one in a few
        // hundred starts with `k_` and the rendered handle reads `k_k_…`.
        // Observed live: `k_k_b306r9LLlg`, one of 67 handles in a single run.
        // It looks like a model doubling the prefix and is not -- a reading
        // this test exists to stop anyone repeating, including by "fixing" the
        // parser to strip a second prefix, which would break exactly these
        // handles and only these.
        let rendered = "k_k_b306r9LLlg";
        let id = RefId::parse(rendered).expect("a legal handle");
        assert_eq!(
            id.to_string(),
            rendered,
            "parsing then rendering must give back the same handle"
        );
        assert_eq!(
            refs_in("[... 9 lines omitted [ref:k_k_b306r9LLlg]]"),
            vec![id],
            "and the scanner must find it in a marker"
        );
    }

    #[test]
    fn a_disabled_collector_mints_nothing() {
        let mut refs = RefCollector::disabled();
        assert_eq!(refs.stash("anything"), None);
        assert!(refs.is_empty());
    }

    #[test]
    fn an_oversized_fragment_is_refused_rather_than_dangled() {
        let mut refs = RefCollector::new(8);
        assert_eq!(refs.stash("a fragment well over the cap"), None);
        assert!(refs.is_empty());
        assert!(refs.stash("small").is_some());
    }

    #[test]
    fn the_cap_counts_utf8_bytes() {
        // four code points, eight bytes
        let mut refs = RefCollector::new(7);
        assert_eq!(refs.stash("ééé"), Some(ref_for("ééé")), "6 bytes fits");
        let mut refs = RefCollector::new(5);
        assert_eq!(refs.stash("ééé"), None, "6 bytes does not");
    }

    #[test]
    fn a_repeated_fragment_is_stashed_once() {
        let mut refs = RefCollector::new(1024);
        let a = refs.stash("dup").unwrap();
        let b = refs.stash("dup").unwrap();
        assert_eq!(a, b);
        assert_eq!(refs.len(), 1);
    }

    #[test]
    fn rollback_discards_only_what_came_after_the_mark() {
        let mut refs = RefCollector::new(1024);
        refs.stash("kept");
        let mark = refs.mark();
        refs.stash("rejected");
        refs.rollback(mark);
        assert_eq!(refs.len(), 1);
        // and the rolled-back fragment can be stashed again later
        refs.stash("rejected");
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn adopting_a_scratch_reproduces_its_handles() {
        let mut real = RefCollector::new(1024);
        let mut scratch = real.scratch();
        let id = scratch.stash("candidate").unwrap();
        real.adopt(scratch);
        assert_eq!(real.pairs()[0].0, id);
    }
}
