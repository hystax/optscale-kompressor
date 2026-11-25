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

use crate::types::Kind;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use blake2::Blake2b;
use blake2::Digest;
use blake2::digest::consts::U9;
use std::collections::{HashMap, HashSet};
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
        // A bare body is tried first: `k` and `_` are both handle bytes, so
        // about one body in 4096 itself begins with `k_`, and stripping the
        // prefix unconditionally cut such a body to ten characters.
        let body = if s.len() == HANDLE_WIDTH {
            s
        } else {
            s.strip_prefix(REF_PREFIX)?
        };
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
    /// Source bodies that a selective anti-re-elision pass must preserve.
    inline_code: HashSet<RefId>,
    max_ref_bytes: usize,
    pairs: Vec<(RefId, String)>,
    seen: HashSet<RefId>,
    /// The kind of payload being compressed, as the route that is running
    /// reported it. Stamped on each fragment `stash` retains.
    kind: Kind,
    /// The kind each retained fragment came out of, for the store's `kind`
    /// column. Absent for a fragment stashed before any route set a kind.
    kinds: HashMap<RefId, Kind>,
    /// Every handle `stash` answered, repeats included, in order. `pairs`
    /// holds a fragment once, so a later message eliding the same bytes left
    /// no trace of it; this is how a caller sees what one message elided.
    stashed: Vec<RefId>,
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
    /// Payloads shipped whole by `min_elision_saving_chars`. Sticky like
    /// `oversized_refusals`, and for the same reason: the gate rolls back
    /// everything the payload stashed, so nothing else is left to say why it
    /// went out uncompressed.
    gated: usize,
    /// Payloads the net-cost gate was asked about, admitted or not. Sticky
    /// like `gated`; what tells `compress_messages` a message's decision
    /// depended on the gate's inputs, and so must be pinned.
    gate_consulted: usize,
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
    stashed: usize,
}

/// A candidate set aside by [`RefCollector::shelve`].
#[derive(Clone, Debug, Default)]
pub struct Shelved {
    fragments: Vec<String>,
    /// Every handle the candidate stashed, in order, repeats included: one an
    /// earlier message already holds is not among `fragments`, and it is the
    /// one `stashed_since` exists to report.
    stashed: Vec<RefId>,
    /// Stashes refused for size while the candidate ran.
    oversized: usize,
}

impl RefCollector {
    /// A collector that retains fragments up to `max_ref_bytes` UTF-8 bytes.
    pub fn new(max_ref_bytes: usize) -> Self {
        RefCollector {
            enabled: true,
            inline_code: HashSet::new(),
            max_ref_bytes,
            pairs: Vec::new(),
            seen: HashSet::new(),
            kind: Kind::Text,
            kinds: HashMap::new(),
            stashed: Vec::new(),
            oversized_attempts: 0,
            oversized_refusals: 0,
            gated: 0,
            gate_consulted: 0,
        }
    }

    /// The aggressive/lossless collector: retains nothing, mints nothing.
    pub fn disabled() -> Self {
        RefCollector {
            enabled: false,
            inline_code: HashSet::new(),
            max_ref_bytes: 0,
            pairs: Vec::new(),
            seen: HashSet::new(),
            kind: Kind::Text,
            kinds: HashMap::new(),
            stashed: Vec::new(),
            oversized_attempts: 0,
            oversized_refusals: 0,
            gated: 0,
            gate_consulted: 0,
        }
    }

    pub(crate) fn preserve_code(&mut self, ids: &[RefId]) {
        self.inline_code.extend(ids.iter().copied());
    }

    #[cfg(feature = "code-aware")]
    pub(crate) fn code_is_preserved(&self, fragment: &str) -> bool {
        !self.inline_code.is_empty() && self.inline_code.contains(&ref_for(fragment))
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
        if crate::markers::is_bare_marker(fragment) {
            // Never an original: see `is_bare_marker`. Counted as an attempt
            // the store could not honour, like an oversized one, so a
            // recoverable payload that would ship it behind a marker ships
            // whole instead. Not a *size* refusal, so not reported as one.
            self.oversized_attempts += 1;
            return None;
        }
        let id = ref_for(fragment);
        if self.seen.insert(id) {
            self.pairs.push((id, fragment.to_owned()));
            self.kinds.insert(id, self.kind);
        }
        self.stashed.push(id);
        Some(id)
    }

    /// A position to roll back to.
    pub fn mark(&self) -> Mark {
        Mark {
            pairs: self.pairs.len(),
            oversized: self.oversized_attempts,
            stashed: self.stashed.len(),
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

    /// Payloads `min_elision_saving_chars` has shipped whole so far.
    pub fn gated(&self) -> usize {
        self.gated
    }

    pub(crate) fn note_gated(&mut self) {
        self.gated += 1;
    }

    /// Payloads the net-cost gate has been asked about so far.
    pub fn gate_consulted(&self) -> usize {
        self.gate_consulted
    }

    pub(crate) fn note_gate_consulted(&mut self) {
        self.gate_consulted += 1;
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
        // `min`: a mark taken before an earlier rollback can point past the
        // end, and there is then nothing of its candidate left to discard.
        for (id, _) in self.pairs.drain(mark.pairs.min(self.pairs.len())..) {
            self.seen.remove(&id);
            self.kinds.remove(&id);
        }
        self.stashed.truncate(mark.stashed);
        // The refusals go back too. A candidate that was discarded did not
        // ship, so neither did anything it could not store — and leaving the
        // count raised lets a losing reading veto a winning one whose
        // fragments were all within the cap.
        self.oversized_attempts = mark.oversized;
    }

    /// Set a candidate aside: remember what it stashed and what it was refused
    /// since `mark`, then roll the collector back so the next candidate starts
    /// clean. [`RefCollector::reinstate`] brings it back if it wins after all.
    ///
    /// The refusal count has to travel with the fragments. `rollback` clears
    /// it — rightly, for a candidate that is gone — but a candidate that is
    /// rolled back and then *re-adopted* ships its text, elisions included. If
    /// one of those elisions was refused for being over `max_ref_bytes`, its
    /// marker has no handle, and with the count cleared nothing downstream
    /// knows: the payload goes out with content removed and no way back to it.
    pub fn shelve(&mut self, mark: Mark) -> Shelved {
        let shelved = Shelved {
            fragments: self.pairs[mark.pairs.min(self.pairs.len())..]
                .iter()
                .map(|(_, fragment)| fragment.clone())
                .collect(),
            oversized: self.oversized_attempts.saturating_sub(mark.oversized),
            stashed: self.stashed[mark.stashed.min(self.stashed.len())..].to_vec(),
        };
        self.rollback(mark);
        shelved
    }

    /// Roll back to `mark` and put a shelved candidate back: its fragments
    /// (content addressing reproduces the handles its markers carry) and its
    /// refusals, so the oversized veto still sees them.
    pub fn reinstate(&mut self, mark: Mark, shelved: &Shelved) {
        self.rollback(mark);
        self.restash_all(&shelved.fragments);
        // The fragments alone are not the whole candidate. One it elided
        // under a handle an earlier message already holds is not in
        // `fragments` — the collector kept it once — so re-stashing them
        // forgets that this message stashed it too, and `stashed_since` is
        // what tells `messages` the model read the same bytes again.
        self.stashed.truncate(mark.stashed);
        self.stashed.extend_from_slice(&shelved.stashed);
        self.oversized_attempts += shelved.oversized;
    }

    /// An empty collector with the same capacity rules, for one candidate of an
    /// arbitration. The loser's fragments are dropped; the winner's are
    /// replayed with [`RefCollector::adopt`].
    pub fn scratch(&self) -> RefCollector {
        RefCollector {
            enabled: self.enabled,
            inline_code: self.inline_code.clone(),
            max_ref_bytes: self.max_ref_bytes,
            pairs: Vec::new(),
            seen: HashSet::new(),
            kind: self.kind,
            kinds: HashMap::new(),
            stashed: Vec::new(),
            oversized_attempts: 0,
            oversized_refusals: 0,
            gated: 0,
            gate_consulted: 0,
        }
    }

    /// Replay a winning candidate's fragments into this collector.
    ///
    /// Content addressing makes the replay exact: a re-stashed fragment yields
    /// the handle its marker already carries.
    pub fn adopt(&mut self, other: RefCollector) {
        self.oversized_attempts += other.oversized_attempts;
        self.oversized_refusals += other.oversized_refusals;
        self.gated += other.gated;
        let outer = self.kind;
        for (id, fragment) in other.pairs {
            // each fragment keeps the kind its own route stamped on it
            self.kind = other.kinds.get(&id).copied().unwrap_or(outer);
            self.stash(&fragment);
        }
        self.kind = outer;
    }

    /// Replay fragments taken from a previous `pairs()` snapshot.
    pub fn restash_all<S: AsRef<str>>(&mut self, fragments: &[S]) {
        for fragment in fragments {
            self.stash(fragment.as_ref());
        }
    }

    /// Each handle stashed since `mark`, once, in first-stashed order —
    /// including fragments an earlier stash already holds, which
    /// `pairs()[mark.pairs..]` does not show.
    pub fn stashed_since(&self, mark: Mark) -> Vec<RefId> {
        let mut seen = HashSet::new();
        self.stashed[mark.stashed.min(self.stashed.len())..]
            .iter()
            .copied()
            .filter(|id| seen.insert(*id))
            .collect()
    }

    /// The fragment held under `id`.
    pub fn fragment(&self, id: RefId) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(other, _)| *other == id)
            .map(|(_, fragment)| fragment.as_str())
    }

    /// Record what kind of payload the fragments stashed from here on come
    /// out of. Kept for statistics and the store browser; nothing routes on it.
    pub fn set_kind(&mut self, kind: Kind) {
        self.kind = kind;
    }

    /// The kind of payload `id` was elided from; `Text` if none was recorded.
    pub fn kind_of(&self, id: &RefId) -> Kind {
        self.kinds.get(id).copied().unwrap_or(Kind::Text)
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
    #[test]
    fn a_fragment_remembers_the_kind_it_was_stashed_under() {
        let mut refs = RefCollector::new(1024);
        let text = refs.stash("plain").unwrap();
        refs.set_kind(Kind::Json);
        let json = refs.stash("{\"a\":1}").unwrap();
        assert_eq!(refs.kind_of(&text), Kind::Text);
        assert_eq!(refs.kind_of(&json), Kind::Json);

        let mut scratch = refs.scratch();
        scratch.set_kind(Kind::SourceCode);
        let code = scratch.stash("fn main() {}").unwrap();
        refs.set_kind(Kind::Log);
        refs.adopt(scratch);
        assert_eq!(refs.kind_of(&code), Kind::SourceCode);
    }

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

    /// `parse` takes "`k_XXXXXXXXXXXX` or the bare 12-character body". A body
    /// that itself begins with `k_` is still a body: stripping the prefix
    /// first cut it to ten characters and refused a legal handle.
    #[test]
    fn a_bare_body_that_begins_with_the_prefix_parses_as_a_body() {
        let id = RefId::parse("k_k_b306r9LLlg").expect("rendered");
        assert_eq!(id.body(), "k_b306r9LLlg");
        assert_eq!(RefId::parse(id.body()), Some(id), "the bare body");
        // Every handle round-trips through both spellings.
        for fragment in ["a", "b", "some fragment", "k_"] {
            let id = ref_for(fragment);
            assert_eq!(RefId::parse(&id.to_string()), Some(id));
            assert_eq!(RefId::parse(id.body()), Some(id));
        }
        // Still refused: wrong widths, and a prefix with nothing legal behind it.
        assert_eq!(RefId::parse("k_short"), None);
        assert_eq!(RefId::parse("k_k_k_b306r9LLlg"), None);
        assert_eq!(RefId::parse("x_k_b306r9LLlg"), None);
        assert_eq!(RefId::parse("k_b306r9LL!g"), None);
        assert_eq!(RefId::parse(""), None);
    }

    /// A candidate that is set aside and then wins after all ships its text,
    /// so the refusals it collected must come back with its fragments. With
    /// only `rollback` + `restash_all`, the count stayed at zero and an
    /// elision with no handle passed the oversized veto.
    #[test]
    fn a_reinstated_candidate_brings_its_refusals_back() {
        let mut refs = RefCollector::new(16);
        refs.stash("before");
        let mark = refs.mark();
        let kept = refs.stash("fits").expect("under the cap");
        assert_eq!(
            refs.stash("this one is well over the sixteen byte cap"),
            None
        );
        assert_eq!(refs.oversized_attempts(), 1);

        let shelved = refs.shelve(mark);
        assert_eq!(
            refs.oversized_attempts(),
            0,
            "a shelved candidate vetoes nothing"
        );
        assert_eq!(refs.pairs().len(), 1, "and holds nothing");

        // The rival runs, is refused something of its own, and loses.
        assert_eq!(refs.stash("the rival is also far beyond the cap"), None);
        refs.stash("rival");

        refs.reinstate(mark, &shelved);
        assert_eq!(
            refs.oversized_attempts(),
            1,
            "its own refusal, not the rival's"
        );
        let held: Vec<RefId> = refs.pairs().iter().map(|(id, _)| *id).collect();
        assert_eq!(held, vec![ref_for("before"), kept]);
        assert_eq!(refs.stashed_since(mark), vec![kept]);
    }

    /// Shelving a candidate that was refused nothing changes nothing.
    #[test]
    fn shelving_and_reinstating_a_clean_candidate_is_exact() {
        let mut refs = RefCollector::new(1024);
        let mark = refs.mark();
        let a = refs.stash("alpha").unwrap();
        let b = refs.stash("beta").unwrap();
        let shelved = refs.shelve(mark);
        assert!(refs.is_empty());
        refs.reinstate(mark, &shelved);
        assert_eq!(refs.stashed_since(mark), vec![a, b]);
        assert_eq!(refs.oversized_attempts(), 0);
    }

    #[test]
    fn reinstating_preserves_handles_already_held_by_earlier_messages() {
        let mut refs = RefCollector::new(1024);
        let earlier = refs.stash("earlier message's fragment").unwrap();
        let mark = refs.mark();
        let new = refs.stash("new fragment").unwrap();
        refs.stash("earlier message's fragment");
        let shelved = refs.shelve(mark);
        refs.stash("losing candidate");
        refs.reinstate(mark, &shelved);
        assert_eq!(refs.stashed_since(mark), vec![new, earlier]);
        assert_eq!(refs.pairs().len(), 2);
    }

    /// A mark can outlive what it points at: an arbitration is handed the
    /// caller's mark, and a rollback in between leaves it past the end.
    #[test]
    fn shelving_from_a_mark_past_the_end_sets_nothing_aside() {
        let mut refs = RefCollector::new(1024);
        let start = refs.mark();
        refs.stash("gone before the shelve");
        let later = refs.mark();
        refs.rollback(start);
        let shelved = refs.shelve(later);
        refs.reinstate(start, &shelved);
        assert!(refs.is_empty());
        assert!(refs.stashed_since(start).is_empty());
    }

    #[test]
    fn an_oversized_fragment_is_refused_rather_than_dangled() {
        let mut refs = RefCollector::new(8);
        assert_eq!(refs.stash("a fragment well over the cap"), None);
        assert!(refs.is_empty());
        assert!(refs.stash("small").is_some());
    }

    /// A marker is a pointer, not a payload: a fragment that is nothing but
    /// markers is refused, and the refusal vetoes the elision the way an
    /// oversized one does.
    #[test]
    fn a_bare_marker_is_never_stored_as_an_original() {
        let mut refs = RefCollector::new(1 << 20);
        for bare in [
            "[... 40 log lines omitted [ref:k_AbC123dEf456]]",
            "  # [... 77 body lines omitted from join; retrieve: ref:k_AbC123dEf456]",
            "\"... 30 similar items omitted [ref:k_AbC123dEf456] ...\",",
            "…[+1840 chars ref:k_DeF456ghI789]",
            "[... 3 library frames omitted [ref:k_AbC123dEf456]]\n\n[... 2 rows omitted]",
            "[Read superseded: src/a.rs was read again later in this conversation [... 1 read omitted [ref:k_AbC123dEf456]]]",
        ] {
            let before = refs.oversized_attempts();
            assert_eq!(refs.stash(bare), None, "{bare}");
            assert_eq!(refs.oversized_attempts(), before + 1, "{bare}");
        }
        assert!(refs.is_empty());
        assert_eq!(refs.oversized_refusals(), 0, "not a size refusal");
        // content that merely carries a marker is still content
        for content in [
            "error: build failed\n[... 40 log lines omitted [ref:k_AbC123dEf456]]",
            "retry: connection refused [×4]",
            "",
        ] {
            assert!(refs.stash(content).is_some(), "{content:?}");
        }
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
