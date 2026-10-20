//! Every visible marker the engine can emit, and the two predicates that
//! decide whether a shrink is accounted for.
//!
//! A shrink carrying none of these markers is indistinguishable from data loss
//! by inspection, so the pipeline ships one only when [`whitespace_only`] proves
//! nothing but layout changed, or when the output declares a reversible
//! envelope. Both predicates live beside the builders so the engine and its
//! tests cannot drift apart on what counts as a marker.

use crate::refs::{RefCollector, RefId};

/// Literals that mark an elision: the `[... N <noun> omitted]` family, the
/// `[×N]` fold, the `…[+N chars]` clip, and any embedded handle.
pub const MARKERS: [&str; 4] = [" omitted", "[×", " chars]", "ref:k_"];

/// Whether `text` carries at least one omission marker.
pub fn has_marker(text: &str) -> bool {
    MARKERS.iter().any(|m| text.contains(m))
}

/// Whether the output declares a reversible columnar envelope.
///
/// Key factoring removes the repeated key names of a homogeneous collection and
/// is reversible by construction from the envelope itself, so it needs no
/// omission marker — but it is also not "layout only", so the accounted guard
/// has to recognise it explicitly or it would refuse a transform that loses
/// nothing.
pub fn has_envelope(text: &str) -> bool {
    text.contains("\"__cols__\"")
}

/// Whether `after` differs from `before` in layout alone.
///
/// This has to be exact rather than approximate, because a JSON round-trip can
/// silently rewrite numeric literals — `1e5` becoming `100000.0` — and that loss
/// looks identical to harmless re-indentation from the outside.
///
/// Whitespace is dropped entirely rather than tokenised: re-indenting a JSON
/// block joins tokens (`"a": 1` to `"a":1`), so a token-list comparison would
/// reject the very case this exists to allow. The trade is that deleting a space
/// *inside* prose would slip through; no compressor does that, and every one of
/// them works line-wise.
pub fn whitespace_only(before: &str, after: &str) -> bool {
    let a = before.chars().filter(|c| !c.is_whitespace());
    let b = after.chars().filter(|c| !c.is_whitespace());
    a.eq(b)
}

/// Plural noun for an omission marker, keyed by the eliding site.
fn noun_for(kind: &str) -> &str {
    match kind {
        "match" => "matches",
        "log" => "log lines",
        "diff" => "diff lines",
        "row" => "rows",
        "entry" => "entries",
        "code" => "body lines",
        "config" => "comment/blank lines",
        other => other,
    }
}

/// One visible omission marker, with its exact hidden bytes retained.
///
/// `[... 412 matches omitted [ref:k_AbC123] in src/app.rs]`
pub fn omission_marker(
    kind: &str,
    count: usize,
    fragment: &str,
    refs: &mut RefCollector,
    suffix: &str,
) -> String {
    let handle = match refs.stash(fragment) {
        Some(id) => format!(" [ref:{id}]"),
        None => String::new(),
    };
    format!("[... {count} {} omitted{handle}{suffix}]", noun_for(kind))
}

/// The tail embedded inside an existing bracket, as the string clip does:
/// `…[+1840 chars ref:k_DeF456]`.
/// Cut `text` down to `limit` characters, announcing the tail.
///
/// The marker is `…[+N chars ref:k_…]`, the same shape a clipped JSON string
/// field carries, because a reader should not have to learn two spellings for
/// "the rest of this is behind a handle".
///
/// The cut backs up to a line break or a space rather than landing mid-token —
/// but only as far as halfway, because past that the clip gives away more than
/// it saves, and ending at the limit beats keeping a sliver.
pub fn clip_to(text: &str, limit: usize, refs: &mut RefCollector) -> String {
    use crate::text::{byte_offset_of_char, char_prefix, chars_len};

    if chars_len(text) <= limit {
        return text.to_string();
    }
    let window = char_prefix(text, limit);
    let half = limit / 2;
    let boundary = |c: char| window.rfind(c).map(|byte| chars_len(&window[..byte]));
    let cut = match boundary('\n').filter(|&index| index >= half) {
        Some(index) => index + 1,
        None => match boundary(' ').filter(|&index| index >= half) {
            Some(index) => index + 1,
            None => limit,
        },
    };
    let (head, elided) = text.split_at(byte_offset_of_char(text, cut));
    format!(
        "{head}…[+{} chars{}]",
        chars_len(elided),
        ref_tag(refs, elided)
    )
}

pub fn ref_tag(refs: &mut RefCollector, fragment: &str) -> String {
    match refs.stash(fragment) {
        Some(id) => format!(" ref:{id}"),
        None => String::new(),
    }
}

/// The `[×37 ref:k_…]` suffix a folded log line carries.
///
/// `folded` is what the fold removed — the 2nd..Nth occurrences — and `kept` is
/// the one line left standing in their place.
///
/// # Why most folds carry no handle
///
/// When every folded-away line is byte-identical to the one kept, the count
/// *is* the fragment: `line [×4]` reconstructs to four copies of `line` and
/// nothing else, so a handle adds a second way to say the same thing. It is not
/// free to add. In one measured run **241 of 275 stored fragments** were such
/// repeats, holding 15,639 bytes for content the marker already carried, and
/// they made up 88% of every marker the model saw — averaging 64 bytes behind
/// each, 158 of them a single duplicate line. Retrieval across that run was
/// zero. A model that spends a round trip on two of those has learned the right
/// lesson about markers, and it is the wrong lesson.
///
/// So the handle is minted only when the fold really did drop something the
/// count cannot give back: lines that share a template but differ in their
/// values, which is the case the handle exists for. `always_handle`
/// (`fold_handles_identical_repeats`) restores the older behaviour for a caller
/// that wants one fetchable fragment per elision whatever it holds.
///
/// # Why this can refuse
///
/// `None` means "do not fold this group". It is returned when the fold would
/// drop lines the count cannot reconstruct *and* no fragment was stored to
/// hold them — whether because the store refused an oversized one or because
/// the mode retains nothing at all. That is the only combination where a
/// marker would be a lie rather than a summary. The caller keeps every
/// occurrence instead.
pub fn fold_suffix(
    kept: &str,
    folded: &[&str],
    refs: &mut RefCollector,
    always_handle: bool,
) -> Option<String> {
    use crate::text::join_lines;

    let count = folded.len() + 1;
    let identical = folded.iter().all(|line| *line == kept);
    let bare = || Some(format!(" [×{count}]"));

    if identical && !always_handle {
        return bare();
    }
    match refs.stash(&join_lines(folded)) {
        Some(id) => Some(format!(" [×{count} ref:{id}]")),
        // Identical repeats reconstruct from the count whether or not a
        // fragment could be stored, so the opt-in degrades rather than refuses.
        None if identical => {
            // Refused for size, but the count alone reconstructs these lines,
            // so nothing unfetchable ships and the refusal is retracted.
            refs.forgive_oversized();
            bare()
        }
        // Nothing was stored and the lines were not the same bytes, so a bare
        // `[×N]` here would assert a repeat that did not happen. That is the
        // one thing this must never do. Measured at the default 65,536, a
        // payload of 4,000 lines differing only in a number came back as one
        // line and `[×3980]`, a line that occurs exactly *once* in the input,
        // with 3,979 distinct lines destroyed.
        //
        // A mode that retains nothing by design was never going to have a
        // fragment here, and that is not an anomaly — it is the contract. The
        // removal is real and describable, so it is described, in the omission
        // vocabulary the engine already uses everywhere else. Declining
        // instead would be honest but needlessly expensive: measured over the
        // shipped corpus it costs 4.02pp of aggressive coverage against the
        // 0.16pp this costs.
        None if !refs.is_enabled() => Some(format!(" [... {} similar lines omitted]", count - 1)),
        // The store *refused* a fragment the mode promised to keep — it is
        // over `max_ref_bytes`. That is a broken promise rather than a
        // contract, so this declines rather than papering over it, and the
        // caller keeps every occurrence.
        //
        // Having declined, the refusal is retracted: every occurrence is still
        // in the output, so there is no unfetchable marker for the pipeline's
        // guard to protect anyone from, and leaving the count raised discards
        // the groups that folded cleanly alongside this one.
        None => {
            refs.forgive_oversized();
            None
        }
    }
}

/// Announce the lines the redraw fold resolved.
///
/// Its own builder rather than [`omission_marker`] because nothing was elided
/// from between the lines: each counted line is still present, holding what the
/// terminal was showing once its carriage returns had been applied. The handle
/// returns those lines with every overwritten frame intact.
pub fn redraw_marker(count: usize, id: Option<RefId>) -> String {
    let handle = match id {
        Some(id) => format!(" [ref:{id}]"),
        None => String::new(),
    };
    let noun = if count == 1 { "line" } else { "lines" };
    format!("[... {count} overwritten progress {noun} resolved{handle}]")
}

/// The comment a code body elision leaves behind.
pub fn code_marker(
    symbol: &str,
    count: usize,
    fragment: &str,
    refs: &mut RefCollector,
    comment: &str,
) -> String {
    let handle = match refs.stash(fragment) {
        Some(id) => format!("; retrieve: ref:{id}"),
        None => String::new(),
    };
    format!("{comment} [... {count} body lines omitted from {symbol}{handle}]")
}

/// The replacement for a file Read a later edit invalidated.
pub fn stale_read_marker(path: &str, content: &str, refs: &mut RefCollector) -> String {
    let inner = omission_marker("read", 1, content, refs, "");
    format!("[Read content stale: {path} {inner}]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fold_of_identical_lines_mints_no_handle() {
        let mut refs = RefCollector::new(1024);
        assert_eq!(
            fold_suffix("same", &["same", "same"], &mut refs, false),
            Some(" [×3]".to_string())
        );
        assert!(
            refs.pairs().is_empty(),
            "nothing to store: the count says it all"
        );
    }

    #[test]
    fn the_opt_in_restores_a_handle_on_an_identical_repeat() {
        let mut refs = RefCollector::new(1024);
        let suffix = fold_suffix("same", &["same", "same"], &mut refs, true).expect("stored");
        assert!(suffix.starts_with(" [×3 ref:k_"), "got {suffix:?}");
        assert_eq!(refs.pairs()[0].1, "same\nsame");
    }

    #[test]
    fn a_fold_of_lines_that_differ_keeps_its_handle() {
        let mut refs = RefCollector::new(1024);
        let suffix =
            fold_suffix("took 1ms", &["took 2ms", "took 3ms"], &mut refs, false).expect("stored");
        assert!(suffix.starts_with(" [×3 ref:k_"), "got {suffix:?}");
        assert_eq!(refs.pairs().len(), 1);
        assert_eq!(refs.pairs()[0].1, "took 2ms\ntook 3ms");
    }

    /// A mode that retains nothing still may not say something untrue.
    ///
    /// `aggressive` and `lossless` run a disabled collector, and a disabled
    /// collector used to short-circuit to a bare count before the lines were
    /// ever compared. On 4,000 lines differing only in a number that produced
    /// `[×3980]` against a line occurring exactly once — the same fabrication
    /// the oversized-fragment path was fixed to prevent, reached by the other
    /// road.
    ///
    /// It says what left instead of inventing a repeat, which keeps the
    /// compression: refusing the fold outright costs 4.02pp of aggressive
    /// coverage on the shipped corpus, this costs 0.16pp.
    #[test]
    fn a_mode_that_retains_nothing_describes_the_removal_rather_than_faking_a_repeat() {
        let mut off = RefCollector::disabled();
        let suffix = fold_suffix("took 1ms", &["took 2ms", "took 3ms"], &mut off, false)
            .expect("the removal is real and describable");
        assert!(
            !suffix.contains("[×"),
            "a count cannot describe lines that differ: {suffix:?}"
        );
        assert_eq!(suffix, " [... 2 similar lines omitted]");
        assert!(has_marker(&suffix), "and it still has to be accounted for");

        // Identical occurrences are reconstructible from the count alone, so
        // they still fold — that is the whole contract of the lossy modes and
        // it is not what was wrong.
        assert_eq!(
            fold_suffix("same", &["same", "same"], &mut off, false),
            Some(" [×3]".to_string())
        );
        // …including when the opt-in asks for a handle the mode cannot mint.
        assert_eq!(
            fold_suffix("same", &["same", "same"], &mut off, true),
            Some(" [×3]".to_string())
        );
    }

    #[test]
    fn a_fold_the_store_will_not_take_is_refused_rather_than_faked() {
        // `max_ref_bytes` is the realistic trigger: the fragment is over the
        // cap, the lines differ, and a bare `[×N]` would claim a repeat that
        // never happened.
        let mut refs = RefCollector::new(8);
        assert_eq!(
            fold_suffix("took 1ms", &["took 2ms", "took 3ms"], &mut refs, false),
            None
        );
        assert!(refs.pairs().is_empty());

        // Identical lines are reconstructible from the count, so the same
        // refusal does not apply to them.
        assert_eq!(
            fold_suffix("same", &["same", "same"], &mut refs, false),
            Some(" [×3]".to_string())
        );
    }

    #[test]
    fn a_collector_that_is_off_still_folds_identical_lines() {
        // Aggressive mode keeps no fragments by design, and that is no
        // obstacle to folding occurrences that were the same bytes: the count
        // is the whole of what a fetch could return. What a disabled collector
        // may *not* do is fold lines that differed — this used to assert
        // exactly that, with `took 1ms [×3]` against a line occurring once.
        let mut refs = RefCollector::disabled();
        assert_eq!(
            fold_suffix("same", &["same", "same"], &mut refs, false),
            Some(" [×3]".to_string())
        );
    }

    #[test]
    fn the_marker_predicate_covers_every_shape_the_engine_emits() {
        let mut refs = RefCollector::new(1024);
        assert!(has_marker(&omission_marker("match", 3, "x", &mut refs, "")));
        assert!(has_marker(&omission_marker("log", 3, "x", &mut refs, "")));
        assert!(has_marker(
            &fold_suffix("x", &["y", "z"], &mut refs, false).unwrap()
        ));
        // the handle-free shape is still a marker: `[×` is what `MARKERS` names
        assert!(has_marker(
            &fold_suffix("x", &["x", "x"], &mut refs, false).unwrap()
        ));
        assert!(has_marker(&format!(
            "head…[+12 chars{}]",
            ref_tag(&mut refs, "y")
        )));
        assert!(has_marker(&redraw_marker(
            2,
            Some(crate::refs::ref_for("z"))
        )));
        assert!(has_marker(&code_marker("f", 30, "x", &mut refs, "#")));
        assert!(!has_marker("an ordinary line of output"));
    }

    #[test]
    fn nouns_are_pluralised_per_site() {
        let mut refs = RefCollector::disabled();
        assert_eq!(
            omission_marker("entry", 12, "", &mut refs, ""),
            "[... 12 entries omitted]"
        );
        assert_eq!(
            omission_marker("config", 4, "", &mut refs, ""),
            "[... 4 comment/blank lines omitted]"
        );
        assert_eq!(
            omission_marker("quoted source", 4, "", &mut refs, ""),
            "[... 4 quoted source omitted]"
        );
    }

    #[test]
    fn a_disabled_collector_yields_a_plain_marker() {
        let mut refs = RefCollector::disabled();
        let marker = omission_marker("match", 5, "fragment", &mut refs, " in a.rs");
        assert_eq!(marker, "[... 5 matches omitted in a.rs]");
        assert!(!marker.contains("ref:"));
    }

    #[test]
    fn indentation_alone_is_whitespace_only() {
        assert!(whitespace_only("{\n  \"a\": 1\n}", "{\"a\":1}"));
        assert!(whitespace_only("a b  c", "a\nb\tc"));
    }

    #[test]
    fn a_changed_number_is_not_whitespace_only() {
        assert!(!whitespace_only("{\"a\": 1e5}", "{\"a\":100000.0}"));
        assert!(!whitespace_only("keep this line", "keep line"));
    }

    #[test]
    fn an_envelope_is_recognised_as_a_declared_transform() {
        assert!(has_envelope(r#"{"__cols__":["a"],"__rows__":[[1]]}"#));
        assert!(!has_envelope(r#"{"cols":["a"]}"#));
    }
}
