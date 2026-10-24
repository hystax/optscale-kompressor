//! Template folding for logs, duplicate collapse for plain text.
//!
//! Lines are normalized — timestamps, uuids, hex ids and numbers become
//! placeholders — and every line whose template was seen before collapses into
//! the first occurrence plus an `[×N]` count. The first occurrence is always
//! kept verbatim, so an error and its stack trace survive untouched, and the
//! last `keep_tail_lines` lines are never folded at all: recent context is
//! disproportionately load-bearing.
//!
//! Plain text folds only byte-identical lines (`normalize = false`), and that
//! floor is not squeamishness. Normalizing a payload whose *identity is its
//! numbers* deletes it: a directory of dated logs folded to one
//! `<n>-<n>-<n>_api.log` and lost 20 of 25 distinct dates, and a disassembly
//! folded to `<n>: <n> <n> …` is nothing an agent can read.
//!
//! The mode shows through in two places only. A disabled [`RefCollector`]
//! (aggressive, lossless) makes every marker come out plain, with no branch
//! here. Lossless additionally turns normalization off and keeps blank lines in
//! place, which leaves the fold reversible in both content and multiplicity —
//! so unlike the lossy compressors this one has no reason to decline.

use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use once_cell::sync::Lazy;
use regex::Regex;

use crate::adaptive::adaptive_k;
use crate::echo::{echoed_line_count, quoted_source_runs};
use crate::markers::{fold_suffix, has_marker, omission_marker};
use crate::options::EngineOptions;
use crate::preamble::split_harness_preamble;
use crate::refs::RefCollector;
use crate::text::{chars_len, is_blank, join_lines, split_lines};
use crate::types::Context;

/// A separator line that names what follows: `==== file ====`, `--- section ---`,
/// a bare rule, or the `==> path <==` banner `head` and `tail` print over
/// several files.
///
/// The run of punctuation must *close* the line. A diff's `--- a/file` header
/// does not close, so it stays ordinary content and folding still reaches
/// across it — which is what a diff needs.
///
/// `BEGIN:VEVENT` … `END:VEVENT` bound one record in iCalendar, vCard and the
/// like. Every record repeats the pair verbatim, so folding saw eighteen
/// identical lines and collapsed them into `BEGIN:VEVENT [×18]`, leaving the
/// events with no boundaries — on a task whose whole job was finding a free
/// hour among them. A delimiter is structure: treating it as a section boundary
/// keeps it verbatim *and* stops folding reaching across records.
static SECTION: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^\s*(?:==+>.*<==+|={3,}(?:.*={3,})?|-{3,}(?:.*-{3,})?|(?:BEGIN|END):[A-Z][A-Z0-9-]*)\s*$",
    )
    .unwrap()
});

/// The placeholders, applied in the order they are declared below.
///
/// The wider datetime goes first, so the bare clock cannot eat the time half of
/// an ISO stamp and leave its date behind, and the bare number goes last, so it
/// only ever sees what the specific patterns did not claim.
static ISO_STAMP: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?")
        .unwrap()
});
static CLOCK: Lazy<Regex> = Lazy::new(|| Regex::new(r"\d{2}:\d{2}:\d{2}(?:[.,]\d+)?").unwrap());
static UUID: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b")
        .unwrap()
});
static HEX_RUN: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(?:0x)?[0-9a-fA-F]{8,}\b").unwrap());
static NUMBER: Lazy<Regex> = Lazy::new(|| Regex::new(r"\d+").unwrap());

/// What a line's identity ignores when the fold looks for repeats.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Identity {
    /// Byte-identical lines only. What a payload whose meaning *is* its numbers
    /// gets, so a count can never stand in for a value.
    Exact,
    /// Lines differing only in run-specific values: timestamps, uuids, hex
    /// runs, bare numbers.
    Values,
    /// Also lines differing only in a *name* — a package, a path, an artefact.
    ///
    /// Reserved for build output, where that is the whole shape of the payload:
    /// `Setting up libbsd0:amd64 (0.12.1-1build1.1) ...` repeated once per
    /// package differs from its neighbours in a name, not a number, so [`Values`]
    /// sees sixty-seven distinct lines and folds nothing. On a measured apt
    /// transcript the engine removed 9% of 27,790 characters — six lines out of
    /// 389 — while three quarters of what was left was four line shapes with one
    /// varying field each.
    ///
    /// [`Values`]: Identity::Values
    ValuesAndNames,
}

/// A token that has a name in it: at least one letter outside the placeholders
/// the value pass left behind, and at least one separator a name is built from.
///
/// Anchored on both ends so a token is classified whole. `[0-9]` cannot appear
/// outside a placeholder because the value pass already replaced every digit
/// run, which is what lets the letter test stand in for "this is a name".
static NAMED_TOKEN: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^\s]*[./:_+~-][^\s]*").unwrap());
/// Everything the value pass substitutes, so a name test can ignore it.
static PLACEHOLDER: Lazy<Regex> = Lazy::new(|| Regex::new(r"<(?:ts|uuid|hex|n)>").unwrap());

/// The identity of a line: what is left once its run-specific values are gone.
fn template(line: &str, identity: Identity) -> String {
    let out = ISO_STAMP.replace_all(line, "<ts>");
    let out = CLOCK.replace_all(&out, "<ts>");
    let out = UUID.replace_all(&out, "<uuid>");
    let out = HEX_RUN.replace_all(&out, "<hex>");
    let out = NUMBER.replace_all(&out, "<n>").into_owned();
    if identity != Identity::ValuesAndNames || is_diagnostic(line) {
        return out;
    }
    NAMED_TOKEN
        .replace_all(&out, |caps: &regex::Captures<'_>| {
            let token = &caps[0];
            let bare = PLACEHOLDER.replace_all(token, "");
            if bare.chars().any(|c| c.is_ascii_alphabetic()) {
                "<id>".to_string()
            } else {
                token.to_string()
            }
        })
        .into_owned()
}

/// The identity build output folds by, given the options.
///
/// One place, because three call sites route build output into the log
/// compressor — the detector's own strategy, the nested router and the mixed
/// segmenter — and a flag honoured by two of them would be a flag that does
/// nothing depending on how the payload arrived.
pub fn build_identity(opts: &EngineOptions) -> Identity {
    if opts.enable_build_templates {
        Identity::ValuesAndNames
    } else {
        Identity::Values
    }
}

/// Whether a line is the kind whose *name* is the information.
///
/// Name folding is what makes a build transcript compressible, and it is also
/// the one thing that must not touch its diagnostics: `error: foo.c:12` and
/// `error: bar.c:99` differ in exactly the field name folding erases, and a
/// build log exists to be read for those two lines. They fall back to value
/// identity, where the file names keep them apart.
fn is_diagnostic(line: &str) -> bool {
    ERROR.is_match(line) || WARN.is_match(line)
}

/// How many echo/caret lines make the diagnostic pair the payload's structure
/// rather than a coincidence. Mirrors the detector's own co-signal.
const ECHO_MIN: usize = 2;

/// Drop the source echo and caret a compiler prints under each diagnostic.
///
/// They restate what `file:line:col:` already said and they are most of the
/// bytes — on a real gcc payload, 1,873 of 2,334 characters. The diagnostics
/// themselves, and everything else, are untouched; one marker per run carries
/// the count and a handle to the exact lines removed.
///
/// Which lines count as quoted source is [`quoted_source_runs`], so the rule
/// that a run has to sit beside a diagnostic is stated once and routing and
/// elision cannot disagree. Matching the shape is not enough: a psql result set
/// is `|`-separated too.
fn elide_diagnostic_echo<'a>(
    lines: &[&'a str],
    refs: &mut RefCollector,
) -> (Vec<Cow<'a, str>>, usize) {
    let runs = quoted_source_runs(lines);
    if echoed_line_count(&runs) < ECHO_MIN {
        return (lines.iter().map(|l| Cow::Borrowed(*l)).collect(), 0);
    }
    let mut out: Vec<Cow<'a, str>> = Vec::with_capacity(lines.len());
    let mut elided = 0usize;
    let mut cursor = 0usize;
    for &(start, stop) in &runs {
        out.extend(lines[cursor..start].iter().map(|l| Cow::Borrowed(*l)));
        let block = &lines[start..stop];
        elided += block.len();
        out.push(Cow::Owned(omission_marker(
            "quoted source",
            block.len(),
            &join_lines(block),
            refs,
            "",
        )));
        cursor = stop;
    }
    out.extend(lines[cursor..].iter().map(|l| Cow::Borrowed(*l)));
    (out, elided)
}

/// Fold `content`, or `None` to leave it exactly as it is.
///
/// `Some` is returned only for output that is strictly shorter than the input;
/// anything the fold stashed on the way to a rejected candidate is rolled back,
/// so the store never holds a fragment no surviving marker names.
pub fn crush_log(
    content: &str,
    opts: &EngineOptions,
    _ctx: &Context<'_>,
    refs: &mut RefCollector,
    identity: Identity,
) -> Option<String> {
    let mark = refs.mark();
    match crush_framed(content, opts, refs, identity) {
        Some(out) if chars_len(&out) < chars_len(content) => Some(out),
        _ => {
            refs.rollback(mark);
            None
        }
    }
}

/// Hold the harness frame out of the fold and put it back verbatim.
///
/// The frame carries the outcome (`Process exited with code 1`) and the handle
/// for anything still running, and a compressor that scores lines dropped both
/// on a real run.
fn crush_framed(
    content: &str,
    opts: &EngineOptions,
    refs: &mut RefCollector,
    identity: Identity,
) -> Option<String> {
    let (preamble, body) = split_harness_preamble(content);
    if preamble.is_empty() {
        return crush_body(content, opts, refs, identity);
    }
    let crushed = crush_body(body, opts, refs, identity)?;
    // measured against the body, because the frame it will be re-attached to is
    // the same either way
    if chars_len(&crushed) >= chars_len(body) {
        return None;
    }
    Some(format!("{preamble}{crushed}"))
}

/// Fold the body, and on a diagnostics payload decide whether eliding the
/// compiler's quoted source first actually helps.
///
/// It cannot simply be done first. Removing lines shrinks the list the scored
/// path then samples, and its keep-the-last-20 tail is a *fraction* of that
/// list — so eliding 60 echo lines makes the sampler keep proportionally more
/// of what remains, and two corpus payloads came out larger that way (2,395 to
/// 4,084 characters). Both orders are cheap at these sizes, so compute both and
/// keep the smaller, rolling the loser's refs back.
fn crush_body(
    content: &str,
    opts: &EngineOptions,
    refs: &mut RefCollector,
    identity: Identity,
) -> Option<String> {
    let lossless = opts.is_lossless();
    // only byte-identical lines may fold, which is what makes the lossless
    // reading recoverable from the `[×N]` count alone
    let identity = if lossless { Identity::Exact } else { identity };
    let lines = split_lines(content);
    if lossless || echoed_line_count(&quoted_source_runs(&lines)) < ECHO_MIN {
        return fold(&lines, opts, identity, refs, 0);
    }

    let mark = refs.mark();
    let plain = fold(&lines, opts, identity, refs, 0);
    // collected before the rollback: handles are content-addressed, so
    // re-stashing these exact fragments later reproduces the very handles the
    // plain reading's markers already carry
    let plain_fragments: Vec<String> = refs.pairs()[mark.pairs..]
        .iter()
        .map(|(_, fragment)| fragment.clone())
        .collect();
    refs.rollback(mark);

    let (elided_lines, echoed) = elide_diagnostic_echo(&lines, refs);
    let view: Vec<&str> = elided_lines.iter().map(|l| l.as_ref()).collect();
    let joined = join_lines(&view);
    // `fold` only declines when nothing at all was removed, which cannot be true
    // once the echo has gone
    let elided = match fold(&view, opts, identity, refs, echoed) {
        Some(out) => out,
        None => joined,
    };
    let plain_text = plain.as_deref().unwrap_or(content);
    if chars_len(&elided) <= chars_len(plain_text) {
        return Some(elided);
    }
    refs.rollback(mark);
    refs.restash_all(&plain_fragments);
    plain
}

/// Collapse repeated templates, `echoed` saying how many lines the diagnostic
/// elision already took out.
///
/// `None` means the input is to be handed back untouched, which is why the
/// original `content` is not a parameter: every path that would have returned it
/// verbatim answers `None` instead, and the caller keeps what it already has.
fn fold(
    lines: &[&str],
    opts: &EngineOptions,
    identity: Identity,
    refs: &mut RefCollector,
    echoed: usize,
) -> Option<String> {
    if identity == Identity::Exact || lines.len() < 50 {
        return fold_templates(lines, opts, identity, refs, echoed);
    }
    let mark = refs.mark();

    // Build output is the one shape where scoring and templating have to be
    // compared rather than tried in order. Everywhere else scoring goes first
    // and wins the moment it beats the *input*, which is the behaviour every
    // other log payload was measured against; the scored reading is also the
    // one that keeps error context, and letting a shorter fold displace it
    // would trade a diagnostic for bytes.
    //
    // A build transcript has no such context to lose and a great deal to fold:
    // on a measured apt payload scoring removed six lines of 389 and, by
    // returning first, prevented a name fold that would have removed 265. So
    // here both readings are computed and the shorter wins.
    // Length is only a fair test between the two readings when neither can win
    // it by discarding something the other kept. The scored reading exists to
    // preserve errors and warnings, so on any payload that has some it keeps
    // its historical right of way: it runs first and wins the moment it beats
    // the input, exactly as before. A build transcript that reports a problem
    // is therefore compressed today's way, unchanged.
    //
    // Pure bookkeeping — the apt and dpkg transcripts that make up most build
    // output, where every measured payload had not one diagnostic line — has
    // nothing to lose that way, and there the two readings are compared on
    // length.
    if identity != Identity::ValuesAndNames || lines.iter().any(|line| is_diagnostic(line)) {
        match crush_scored(lines, opts, refs) {
            Some(out) if chars_len(&out) < chars_len(&join_lines(lines)) => return Some(out),
            _ => refs.rollback(mark),
        }
        return fold_templates(lines, opts, identity, refs, echoed);
    }

    let templated = fold_templates(lines, opts, identity, refs, echoed);
    // collected before the rollback: handles are content-addressed, so
    // re-stashing these exact fragments reproduces the very handles the
    // templated reading's markers already carry
    let templated_fragments: Vec<String> = refs.pairs()[mark.pairs..]
        .iter()
        .map(|(_, fragment)| fragment.clone())
        .collect();
    let templated_len = templated.as_deref().map(chars_len);
    refs.rollback(mark);

    match crush_scored(lines, opts, refs) {
        Some(out)
            if chars_len(&out) < chars_len(&join_lines(lines))
                && templated_len.is_none_or(|folded| chars_len(&out) < folded) =>
        {
            return Some(out);
        }
        _ => refs.rollback(mark),
    }
    refs.restash_all(&templated_fragments);
    templated
}

/// Collapse lines that share a template, keeping the first of each.
fn fold_templates(
    lines: &[&str],
    opts: &EngineOptions,
    identity: Identity,
    refs: &mut RefCollector,
    echoed: usize,
) -> Option<String> {
    let lossless = opts.is_lossless();
    let echo_mark = refs.mark();
    let keep_tail = opts.keep_tail_lines;
    if lines.len() <= keep_tail {
        // Short once the echo went, but the echo still went: hand back the
        // reduced body, or that elision is silently lost.
        return if echoed > 0 {
            Some(join_lines(lines))
        } else {
            None
        };
    }
    // Split at `len - keep_tail`, computed as an index. The tempting form is
    // a negative offset counted from the end, and at `keep_tail_lines = 0` that
    // wraps to mean *everything* rather than nothing — routing the whole payload
    // into the untouched tail and silently disabling compression.
    let split = lines.len() - keep_tail;
    let (body, tail) = lines.split_at(split);

    let mut out: Vec<&str> = Vec::new();
    let mut counts: Vec<usize> = Vec::new();
    let mut members: Vec<Vec<&str>> = Vec::new();
    let mut index: HashMap<Cow<'_, str>, usize> = HashMap::new();
    // Which slot of `out` each body line belongs to, `None` for a line this
    // pass drops outright. It is what lets a group that declines to fold put
    // its occurrences back **where they were**: `out` holds only the first of
    // each, so rebuilding from it alone would move the others up to join it.
    let mut slot_of: Vec<Option<usize>> = Vec::with_capacity(body.len());
    for &line in body {
        if SECTION.is_match(line) {
            // A separator starts a new document: fold within one, never across.
            // Folding keeps the *first* occurrence globally, so a line repeated
            // once per section was being moved into the earliest section and
            // deleted from the rest. On a real run one tool result held the
            // extracted text of six PDFs; three contained the line `Invoice`,
            // all three collapsed into the first, the agent classified the other
            // two as something else and its total came out 1336.00 short. A
            // count says how often a line occurred, never in which document.
            index.clear();
            slot_of.push(Some(out.len()));
            out.push(line);
            counts.push(1);
            members.push(Vec::new());
            continue;
        }
        if has_marker(line) {
            // A marker is the only path back to the fragment it names, so a
            // line carrying one has to survive verbatim. Folding would keep the
            // *first* such line and hide the rest inside a fragment, leaving
            // their handles reachable only from text the model can no longer
            // see. Name folding made that easy to hit — every handle is a token
            // full of separators, so `[... 2 quoted source omitted [ref:k_A]]`
            // and `[... 2 quoted source omitted [ref:k_B]]` share a template
            // and twelve compiler diagnostics collapsed into one — but value
            // folding could reach it too, with two handles differing only in a
            // digit. Excluded once, here, for both.
            slot_of.push(Some(out.len()));
            out.push(line);
            counts.push(1);
            members.push(Vec::new());
            continue;
        }
        if is_blank(line) {
            if lossless {
                // preserved in place and never folded, so the blank structure
                // of the original is reproducible
                slot_of.push(Some(out.len()));
                out.push(line);
                counts.push(1);
                members.push(Vec::new());
            } else {
                slot_of.push(None);
            }
            continue;
        }
        let key = match identity {
            Identity::Exact => Cow::Borrowed(line),
            Identity::Values | Identity::ValuesAndNames => Cow::Owned(template(line, identity)),
        };
        match index.entry(key) {
            Entry::Vacant(slot) => {
                slot.insert(out.len());
                slot_of.push(Some(out.len()));
                out.push(line);
                counts.push(1);
                members.push(vec![line]);
            }
            Entry::Occupied(slot) => {
                let pos = *slot.get();
                slot_of.push(Some(pos));
                counts[pos] += 1;
                members[pos].push(line);
            }
        }
    }

    if !counts.iter().any(|&count| count > 1) {
        // Nothing folded. Rebuilding the body here would still read as a win to
        // a `len(out) < len(content)` test, because splitlines drops the
        // trailing newline and the loop above drops blank lines — a few bytes of
        // whitespace, no omission marker, and a bogus optimization event. On one
        // run 5 of 6 text payloads were "compressed" by 1 to 8 bytes that way.
        // Hand back the input untouched instead, unless the diagnostic echo
        // went, which is a real removal carrying its own marker and has to
        // survive nothing else folding.
        if echoed > 0 {
            // `lines` already holds the echo markers in place of the echo. The
            // Python appends the tail a second time here, which duplicates
            // `keep_tail_lines` lines of real output; this returns the reduced
            // body once, as the comparison against the original expects.
            return Some(join_lines(lines));
        }
        // declining leaves the store exactly as this fold found it, so no
        // fragment outlives the marker that would have named it
        refs.rollback(echo_mark);
        return None;
    }

    // The suffix each group will carry, or `None` for a group that does not
    // fold. `fold_suffix` refuses when the lines it would remove are not
    // recoverable from the count *and* the store would not hold them — see its
    // docs; a bare `[×N]` there would assert a repeat that never happened.
    //
    // The folded-away occurrences (2nd..Nth) otherwise go behind the handle, so
    // their specific values survive normalization and stay retrievable — unless
    // they are byte-identical to the line kept, in which case `[×N]` already
    // says everything a fetch could, and no handle is minted.
    let suffixes: Vec<Option<String>> = out
        .iter()
        .zip(counts.iter())
        .zip(members.iter())
        .map(|((line, &count), member)| {
            if count > 1 {
                fold_suffix(
                    line,
                    &member[1..],
                    refs,
                    opts.fold_handles_identical_repeats,
                    opts.fold_marks_variants,
                )
            } else {
                None
            }
        })
        .collect();

    if suffixes.iter().all(Option::is_none) {
        // Every group that could have folded refused. That is the same position
        // as nothing having repeated at all, and takes the same exit.
        if echoed > 0 {
            return Some(join_lines(lines));
        }
        refs.rollback(echo_mark);
        return None;
    }

    let mut rebuilt: Vec<Cow<'_, str>> = Vec::with_capacity(out.len() + tail.len());
    let mut folded_yet = vec![false; out.len()];
    for (&line, slot) in body.iter().zip(slot_of.iter()) {
        let Some(pos) = *slot else {
            // a blank the non-lossless pass drops
            continue;
        };
        match &suffixes[pos] {
            Some(suffix) => {
                if !folded_yet[pos] {
                    folded_yet[pos] = true;
                    rebuilt.push(Cow::Owned(format!("{}{suffix}", out[pos])));
                }
            }
            // Either a line that never repeated, or a group whose fold was
            // refused: every occurrence stays, in the place it stood.
            None => rebuilt.push(Cow::Borrowed(line)),
        }
    }
    rebuilt.extend(tail.iter().map(|line| Cow::Borrowed(*line)));
    Some(join_lines(&rebuilt))
}

/// What counts as an error line.
///
/// The second group is there because a native crash announces itself without
/// ever saying "error": `Segmentation fault (core dumped)` and a bare `SIGSEGV`
/// carry none of the first group's words, so a process that died hard scored
/// 0.1 — the same as a routine progress line — and was dropped out of the
/// middle of a log while the marker reported `0 ERROR`. A Python-wrapped
/// segfault was kept only because CPython prefixes it with `Fatal Python
/// error:`, which is an accident of that runtime and not a property of crashes.
///
/// The third group is the operational failures a long run actually dies of,
/// found by probing the vocabulary against real messages: five of eight common
/// ones were elided with no hint, because `Permission denied`,
/// `Connection refused`, `Operation timed out` and `No space left on device`
/// contain none of the words above.
///
/// Deliberately absent: a bare `killed`, which is the OOM killer's word but
/// also ordinary process-management vocabulary; `not found` and `no such file`,
/// which are routine in build and strace output; and `timeout`, which is a
/// configuration key as often as an event — `timed out` is the event.
/// `out of memory` and `oom-killed` catch the OOM case without claiming every
/// line that mentions killing something.
static ERROR: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:error|fail(?:ed|ure)?|fatal|panic(?:ked|king)?|exception|critical|segfault|segmentation\s+fault|core\s+dumped|sig(?:segv|abrt|bus|ill|fpe)|double\s+free|stack\s+smashing|out\s+of\s+memory|oom[-_]?kill(?:ed|er)?|terminate\s+called|aborted|permission\s+denied|access\s+denied|operation\s+not\s+permitted|connection\s+(?:refused|reset)|broken\s+pipe|timed\s+out|no\s+space\s+left|disk\s+full|read-only\s+file\s+system|too\s+many\s+open\s+files|address\s+already\s+in\s+use|deadlock)\b",
    )
    .unwrap()
});

/// Whether a line carries error signal, by the one definition in this crate.
///
/// Exposed so detection can ask the question without growing a second copy of
/// the vocabulary: two derivations of one value diverge, and this one decides
/// both what the log path keeps and what the detector routes here at all.
pub(crate) fn is_error_line(line: &str) -> bool {
    ERROR.is_match(line)
}

/// Whether a line is the log reporting an outcome rather than being one.
///
/// Routing consults this to subtract summaries from its error count. A test
/// log's `0 tests failed` carries error vocabulary while announcing the
/// absence of errors, and counting it diverted a clean OCaml testsuite run to
/// the scored path. An event buried mid-stream — a crash — carries error
/// vocabulary and no summary vocabulary, which is the case routing exists for.
pub(crate) fn is_summary_line(line: &str) -> bool {
    SUMMARY.is_match(line)
}
static WARN: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bwarn(?:ing)?\b").unwrap());
static SUMMARY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:summary|passed|failed|total|result|complete(?:d)?)\b").unwrap()
});
static TRACE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"^\s*(?:File ".+", line \d+|at \S+\(|\.\.\.|Caused by:)"#).unwrap());

/// Keep bounded error context, unique warnings, summaries and the tail.
///
/// `None` when there is no signal to score by, or when the selection omits
/// nothing, so the caller falls through to template folding.
fn crush_scored(lines: &[&str], opts: &EngineOptions, refs: &mut RefCollector) -> Option<String> {
    if !lines
        .iter()
        .any(|line| ERROR.is_match(line) || WARN.is_match(line) || SUMMARY.is_match(line))
    {
        return None;
    }

    let mut scores: Vec<f32> = Vec::with_capacity(lines.len());
    let mut warning_seen: HashSet<String> = HashSet::new();
    for &line in lines {
        let mut score = 0.1f32;
        if ERROR.is_match(line) {
            score = 1.0;
        } else if WARN.is_match(line) {
            // the tenth copy of one warning is worth far less than the first
            let fingerprint = warning_fingerprint(line);
            score = if warning_seen.contains(&fingerprint) {
                0.05
            } else {
                0.5
            };
            warning_seen.insert(fingerprint);
        } else if SUMMARY.is_match(line) {
            score = 0.5;
        }
        if TRACE.is_match(line) && score < 0.8 {
            // a frame carries no error word of its own, but dropping it out of
            // the middle of a traceback loses the path to the error
            score = 0.8;
        }
        scores.push(score);
    }

    let budget = adaptive_k(lines, 10, 100.min(lines.len()), 1.0);
    let mut keep: HashSet<usize> =
        (lines.len().saturating_sub(opts.keep_tail_lines)..lines.len()).collect();
    // A marker already in the input is not content to be scored: an earlier
    // stage put it there and stashed a fragment behind it, so dropping it
    // strands a ref nothing can retrieve. Scoring found them uninteresting (no
    // error word, no summary word) and cut two of them on the corpus.
    keep.extend(
        lines
            .iter()
            .enumerate()
            .filter(|(_, line)| has_marker(line))
            .map(|(index, _)| index),
    );
    let error_indices: Vec<usize> = scores
        .iter()
        .enumerate()
        .filter(|&(_, &score)| score >= 1.0)
        .map(|(index, _)| index)
        .collect();
    // bounded at ten so a payload that is nothing but errors does not keep
    // itself whole through the context rule alone
    for &index in error_indices.iter().take(10) {
        for around in index.saturating_sub(3)..(index + 4).min(lines.len()) {
            keep.insert(around);
        }
    }
    let mut order: Vec<usize> = (0..lines.len()).collect();
    // highest score first, earliest line breaking the tie, so the selection is
    // byte-stable across turns
    order.sort_by(|&left, &right| {
        scores[right]
            .total_cmp(&scores[left])
            .then_with(|| left.cmp(&right))
    });
    for index in order {
        if keep.len() >= budget {
            break;
        }
        keep.insert(index);
    }

    let omitted: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|(index, _)| !keep.contains(index))
        .map(|(_, line)| *line)
        .collect();
    if omitted.is_empty() {
        return None;
    }
    let kept: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|(index, _)| keep.contains(index))
        .map(|(_, line)| *line)
        .collect();

    let errors = omitted.iter().filter(|line| ERROR.is_match(line)).count();
    let warnings = omitted.iter().filter(|line| WARN.is_match(line)).count();
    // `0 ERROR, 0 WARN` is two different statements and only one of them is
    // supported. When the payload carried error or warning vocabulary, the
    // scorer looked, found it, and kept it, so zero in the omitted span means
    // "nothing wrong went behind the handle" — worth saying. When the selection
    // was driven by summary lines alone, the scorer never had error vocabulary
    // to exercise, and the same `0 ERROR` asserts the span is clean when all it
    // knows is that nothing matched words it recognises. A bare
    // `Segmentation fault (core dumped)` read as clean on exactly that path.
    // An absent claim is honest; a zero is not.
    let vocabulary_exercised = lines
        .iter()
        .any(|line| ERROR.is_match(line) || WARN.is_match(line));
    let note = if vocabulary_exercised {
        format!("; {errors} ERROR, {warnings} WARN")
    } else {
        String::new()
    };
    let marker = omission_marker("log", omitted.len(), &join_lines(&omitted), refs, &note);
    let mut out = join_lines(&kept);
    if !kept.is_empty() {
        out.push('\n');
    }
    out.push_str(&marker);
    Some(out)
}

/// The identity of a warning, for deciding whether it is the same one again.
///
/// Everything left of the first `:` or `=` is the message and is kept verbatim;
/// only the right-hand side has its addresses and counters normalized. Running
/// the full template over the line instead collapses distinct classes of
/// warning into one fingerprint, and then only the first of them scores as new.
fn warning_fingerprint(line: &str) -> String {
    let Some(position) = line.find([':', '=']) else {
        return line.to_string();
    };
    let (left, rest) = line.split_at(position);
    let (separator, right) = rest.split_at(1);
    let right = HEX_RUN.replace_all(right, "<addr>");
    let right = NUMBER.replace_all(&right, "<n>");
    format!("{left}{separator}{right}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::refs_in;
    use crate::types::Mode;

    fn options(mode: Mode, keep_tail: usize) -> EngineOptions {
        EngineOptions {
            mode,
            keep_tail_lines: keep_tail,
            ..EngineOptions::default()
        }
    }

    fn ctx() -> Context<'static> {
        Context::for_role("user")
    }

    fn recoverable() -> RefCollector {
        RefCollector::new(1 << 20)
    }

    /// Every handle in the collector must be named by the output; every handle
    /// in the output must be in the collector.
    fn assert_no_orphan_refs(out: &str, refs: &RefCollector) {
        for (id, _) in refs.pairs() {
            assert!(
                out.contains(&id.to_string()),
                "stashed fragment {id} is named by no marker in:\n{out}"
            );
        }
        for id in refs_in(out) {
            assert!(
                refs.pairs().iter().any(|(stashed, _)| *stashed == id),
                "marker names {id}, which nothing stashed"
            );
        }
    }

    #[test]
    fn a_repeated_line_folds_to_a_count() {
        let content = "boot sequence starting on node 4\n\
                       retry: connection refused by upstream host\n\
                       retry: connection refused by upstream host\n\
                       retry: connection refused by upstream host\n\
                       done\n";
        let mut refs = recoverable();
        let out = crush_log(
            content,
            &options(Mode::Recoverable, 0),
            &ctx(),
            &mut refs,
            Identity::Exact,
        )
        .expect("three identical lines fold");
        assert!(
            out.contains("retry: connection refused by upstream host [×3"),
            "got {out:?}"
        );
        assert!(out.starts_with("boot sequence starting on node 4\n") && out.ends_with("done"));
    }

    #[test]
    fn a_fold_of_identical_lines_carries_no_handle_in_any_mode() {
        // `[×3]` says the line occurred three times and the occurrences were
        // the same bytes, which is the whole fragment. Storing it anyway put
        // 241 of 275 fragments in one measured run behind markers that held 64
        // bytes each and were never once fetched.
        let line = "connection pool exhausted, queuing request";
        let content = format!("alpha\n{line}\n{line}\n{line}\nomega\n");
        for mode in [Mode::Recoverable, Mode::Aggressive] {
            let mut refs = if mode == Mode::Recoverable {
                recoverable()
            } else {
                RefCollector::disabled()
            };
            let out = crush_log(
                &content,
                &options(mode, 0),
                &ctx(),
                &mut refs,
                Identity::Exact,
            )
            .expect("folds");
            assert!(
                out.contains(&format!("{line} [×3]")),
                "{mode:?}: got {out:?}"
            );
            assert!(!out.contains("ref:"), "{mode:?}: got {out:?}");
            assert!(refs.is_empty(), "{mode:?} stored a fragment nothing needs");
        }
    }

    #[test]
    fn a_fold_of_lines_that_differ_carries_a_handle_in_recoverable_mode() {
        // Normalization is what makes this fold possible and also what loses
        // the per-line values, so here the handle is the only way back.
        let content = "2024-03-01T10:00:00Z GET /health 200\n\
                       2024-03-01T10:00:01Z GET /health 200\n\
                       2024-03-01T10:00:02Z GET /health 200\n\
                       done\n";
        let mut refs = recoverable();
        let recovered = crush_log(
            content,
            &options(Mode::Recoverable, 0),
            &ctx(),
            &mut refs,
            Identity::Values,
        )
        .expect("folds");
        // ...and so the marker says `similar`, not a bare repeat: the three
        // timestamps differ, and a model told they repeated has no reason to
        // reach for the only copy of them.
        assert!(
            recovered.contains("[×3 similar ref:k_"),
            "got {recovered:?}"
        );
        assert_eq!(refs.len(), 1, "one fragment for the 2nd..Nth occurrences");
        assert_eq!(
            refs.pairs()[0].1,
            "2024-03-01T10:00:01Z GET /health 200\n2024-03-01T10:00:02Z GET /health 200"
        );

        // Aggressive keeps no fragments, so there is nowhere for the differing
        // values to go — and `[×3]` against one of the three timestamps would
        // say that timestamp occurred three times. It occurred once. What left
        // is described instead of counted.
        let mut none = RefCollector::disabled();
        let aggressive = crush_log(
            content,
            &options(Mode::Aggressive, 0),
            &ctx(),
            &mut none,
            Identity::Values,
        )
        .expect("folds");
        assert!(
            aggressive.contains("similar lines omitted"),
            "got {aggressive:?}"
        );
        assert!(!aggressive.contains("[×"), "got {aggressive:?}");
        assert!(!aggressive.contains("ref:"));
        assert!(none.is_empty());
    }

    #[test]
    fn normalization_folds_lines_that_differ_only_in_their_timestamps() {
        let content = "2024-03-01T10:00:00Z GET /health 200\n\
                       2024-03-01T10:00:01Z GET /health 200\n\
                       2024-03-01T10:00:02Z GET /health 200\n";
        let mut refs = recoverable();
        let out = crush_log(
            content,
            &options(Mode::Recoverable, 0),
            &ctx(),
            &mut refs,
            Identity::Values,
        )
        .expect("one template, three lines");
        assert!(out.contains("[×3"), "got {out:?}");
        assert!(
            out.starts_with("2024-03-01T10:00:00Z GET /health 200"),
            "the first occurrence is kept verbatim, got {out:?}"
        );
    }

    #[test]
    fn folding_never_crosses_a_section_separator() {
        // the invoice case: three documents in one payload, each holding the
        // same line, and the count must not move it into the first of them
        let content = "===== doc1.pdf =====\n\
                       Invoice\n\
                       Line item: widget assembly, quantity 3\n\
                       Line item: widget assembly, quantity 3\n\
                       Total 100\n\
                       ===== doc2.pdf =====\n\
                       Invoice\n\
                       Total 200\n\
                       ===== doc3.pdf =====\n\
                       Invoice\n\
                       Total 300\n";
        let mut refs = recoverable();
        let out = crush_log(
            content,
            &options(Mode::Recoverable, 0),
            &ctx(),
            &mut refs,
            Identity::Exact,
        )
        .expect("the repeated noise line folds");
        assert_eq!(
            out.matches("Invoice").count(),
            3,
            "every document keeps its own line, got {out:?}"
        );
        assert!(!out.contains("Invoice ["), "no cross-section count");
        assert!(
            out.contains("Line item: widget assembly, quantity 3 [×2"),
            "within a section it still folds, got {out:?}"
        );
    }

    #[test]
    fn folding_never_crosses_an_icalendar_record_boundary() {
        let mut content = String::new();
        for _ in 0..3 {
            content.push_str(
                "BEGIN:VEVENT\nSUMMARY:Standup\n\
                 DESCRIPTION:daily standup for the platform team in room four\n\
                 DESCRIPTION:daily standup for the platform team in room four\n\
                 END:VEVENT\n",
            );
        }
        let mut refs = recoverable();
        let out = crush_log(
            &content,
            &options(Mode::Recoverable, 0),
            &ctx(),
            &mut refs,
            Identity::Exact,
        )
        .expect("the duplicated description inside a record folds");
        assert_eq!(out.matches("BEGIN:VEVENT").count(), 3, "got {out:?}");
        assert_eq!(out.matches("END:VEVENT").count(), 3, "got {out:?}");
        assert_eq!(
            out.matches("SUMMARY:Standup").count(),
            3,
            "three events keep three summaries, got {out:?}"
        );
        assert!(
            out.contains("DESCRIPTION:daily standup for the platform team in room four [×2"),
            "got {out:?}"
        );
    }

    #[test]
    fn a_diff_header_is_content_and_does_not_reset_the_fold() {
        // `--- a/file` does not close its run of dashes, so it is not a section
        let dup = "-  let timeout = Duration::from_secs(30); // the old value";
        let content = format!("{dup}\n--- a/src/x.rs\n{dup}\ntail\n");
        let mut refs = recoverable();
        let out = crush_log(
            &content,
            &options(Mode::Recoverable, 0),
            &ctx(),
            &mut refs,
            Identity::Exact,
        )
        .expect("the two payload lines fold across the header");
        assert!(out.contains(&format!("{dup} [×2")), "got {out:?}");
        assert!(out.contains("--- a/src/x.rs"));
    }

    #[test]
    fn nothing_folded_returns_none() {
        // rebuilding from splitlines would drop the trailing newline and the
        // blank line and pass a naive shorter-than test with no marker to show
        let content = "alpha\n\nbeta\ngamma\n";
        let mut refs = recoverable();
        assert!(
            crush_log(
                content,
                &options(Mode::Recoverable, 0),
                &ctx(),
                &mut refs,
                Identity::Exact
            )
            .is_none()
        );
        assert!(refs.is_empty(), "and nothing was stashed on the way out");
    }

    #[test]
    fn keep_tail_lines_of_zero_still_compresses() {
        // a negative slice would make the tail the entire payload here
        let line = "GET /api/v1/widgets 200 in 12ms from 10.0.0.7";
        let content = format!("{line}\n{line}\n{line}\n{line}\n");
        let mut refs = recoverable();
        let out = crush_log(
            &content,
            &options(Mode::Recoverable, 0),
            &ctx(),
            &mut refs,
            Identity::Exact,
        )
        .expect("keep_tail 0 folds everything");
        assert!(out.contains(&format!("{line} [×4")), "got {out:?}");
    }

    #[test]
    fn the_tail_is_kept_verbatim() {
        let line = "GET /api/v1/widgets 200 in 12ms from 10.0.0.7";
        let content = format!("{line}\n{line}\n{line}\n{line}\n{line}\n");
        let mut refs = recoverable();
        let out = crush_log(
            &content,
            &options(Mode::Recoverable, 2),
            &ctx(),
            &mut refs,
            Identity::Exact,
        )
        .expect("the body folds even though the tail does not");
        assert!(out.ends_with(&format!("{line}\n{line}")), "got {out:?}");
        assert!(out.contains(&format!("{line} [×3")), "got {out:?}");
    }

    #[test]
    fn a_payload_shorter_than_the_tail_is_left_alone() {
        let content = "one\ntwo\n";
        let mut refs = recoverable();
        assert!(
            crush_log(
                content,
                &options(Mode::Recoverable, 20),
                &ctx(),
                &mut refs,
                Identity::Exact
            )
            .is_none()
        );
    }

    #[test]
    fn lossless_folds_only_byte_identical_lines_and_keeps_blank_lines() {
        let content = "2024-03-01T10:00:00Z tick\n\
                       2024-03-01T10:00:01Z tick\n\
                       \n\
                       steady\n\
                       steady\n\
                       steady\n";
        let mut refs = RefCollector::disabled();
        // normalize is asked for and lossless must refuse it
        let out = crush_log(
            content,
            &options(Mode::Lossless, 0),
            &ctx(),
            &mut refs,
            Identity::Values,
        )
        .expect("the identical lines still fold");
        assert!(
            out.contains("2024-03-01T10:00:00Z tick\n2024-03-01T10:00:01Z tick"),
            "the two stamps differ byte-wise and must both survive, got {out:?}"
        );
        assert!(out.contains("steady [×3]"), "got {out:?}");
        assert!(
            out.contains("tick\n\nsteady"),
            "the blank line stays in place"
        );
    }

    #[test]
    fn the_harness_frame_is_re_attached_verbatim() {
        let line = "npm ERR! network request to registry.example failed";
        let content = format!(
            "Chunk ID: abc\nProcess exited with code 1\nOutput:\n{line}\n{line}\n{line}\n{line}\n"
        );
        let mut refs = recoverable();
        let out = crush_log(
            &content,
            &options(Mode::Recoverable, 0),
            &ctx(),
            &mut refs,
            Identity::Exact,
        )
        .expect("the body folds");
        assert!(
            out.starts_with("Chunk ID: abc\nProcess exited with code 1\nOutput:\n"),
            "got {out:?}"
        );
        assert!(out.contains(&format!("{line} [×4")), "got {out:?}");
    }

    #[test]
    fn a_framed_payload_whose_body_does_not_shrink_is_declined() {
        let content = "Chunk ID: abc\nOutput:\nalpha\nbeta\ngamma\n";
        let mut refs = recoverable();
        assert!(
            crush_log(
                content,
                &options(Mode::Recoverable, 0),
                &ctx(),
                &mut refs,
                Identity::Exact
            )
            .is_none()
        );
        assert!(refs.is_empty());
    }

    /// gcc echoing twelve distinct source lines: folding cannot touch them, so
    /// one marker per run is the smaller reading.
    fn distinct_diagnostics() -> String {
        let names = [
            "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india",
            "juliet", "kilo", "lima",
        ];
        let mut content = String::new();
        for (index, name) in names.iter().enumerate() {
            content.push_str(&format!(
                "src/{name}.c:{}:5: error: '{name}_handle' undeclared here\n",
                index + 7
            ));
            content.push_str(&format!(
                "   {} |     {name}_handle = open_{name}(config, timeout_ms, retries);\n",
                index + 7
            ));
            content.push_str(&format!("      |     ^{}\n", "~".repeat(name.len())));
        }
        content
    }

    #[test]
    fn the_diagnostic_echo_is_elided_when_that_is_the_smaller_reading() {
        let content = distinct_diagnostics();
        let opts = options(Mode::Recoverable, 0);
        let mut refs = recoverable();
        let out =
            crush_log(&content, &opts, &ctx(), &mut refs, Identity::Values).expect("the echo goes");
        assert!(out.contains("quoted source omitted"), "got {out:?}");
        assert!(
            out.contains("src/alpha.c:7:5: error:") && out.contains("src/lima.c:18:5: error:"),
            "every diagnostic itself survives, got {out:?}"
        );
        assert!(chars_len(&out) < chars_len(&content));
        assert_no_orphan_refs(&out, &refs);
    }

    #[test]
    fn the_arbitration_keeps_the_plain_fold_when_it_is_smaller_and_restashes_its_refs() {
        // identical echo lines under every diagnostic: normalization folds the
        // whole payload to a handful of lines, far below twelve echo markers
        let mut content = String::new();
        for index in 0..12 {
            content.push_str(&format!(
                "src/main.c:{}:5: warning: unused variable 'x' [-Wunused-variable]\n",
                index + 7
            ));
            content.push_str(&format!("   {} |     int x = {index};\n", index + 10));
            content.push_str("      |         ^~\n");
        }
        let opts = options(Mode::Recoverable, 0);
        let mut refs = recoverable();
        let out = crush_log(&content, &opts, &ctx(), &mut refs, Identity::Values)
            .expect("the payload folds");
        assert!(
            !out.contains("quoted source omitted"),
            "the elided reading was larger and must lose, got {out:?}"
        );
        assert!(out.contains("[×12"), "got {out:?}");
        assert_no_orphan_refs(&out, &refs);
    }

    #[test]
    fn the_arbitration_never_returns_more_than_the_plain_fold_would() {
        for content in [
            distinct_diagnostics(),
            "src/a.c:1:1: error: bad\n  1 | int x;\n    | ^\nsrc/a.c:2:1: error: worse\n  2 | int y;\n    | ^\n"
                .to_string(),
        ] {
            let opts = options(Mode::Recoverable, 0);
            let mut arbitrated = recoverable();
            let out = crush_log(&content, &opts, &ctx(), &mut arbitrated, Identity::Values);

            let mut plain_refs = recoverable();
            let lines = split_lines(&content);
            let plain = fold(&lines, &opts, Identity::Values, &mut plain_refs, 0);
            let plain_len = chars_len(plain.as_deref().unwrap_or(&content));

            let arbitrated_len = chars_len(out.as_deref().unwrap_or(&content));
            assert!(
                arbitrated_len <= plain_len,
                "arbitration returned {arbitrated_len} chars where the plain fold gives {plain_len}"
            );
        }
    }

    /// 60 lines of mostly uninteresting output with a few errors and warnings.
    fn scored_payload() -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        for index in 0..60 {
            lines.push(match index {
                8 => "ERROR unable to reach cache node 4".to_string(),
                9 => "  at cache::connect(cache.rs)".to_string(),
                30 => "WARNING: retry attempt 1 for job 77".to_string(),
                31 => "WARNING: retry attempt 2 for job 78".to_string(),
                _ => format!("INFO scanning widget number {index} of the shelf inventory"),
            });
        }
        lines
    }

    #[test]
    fn the_scored_path_keeps_error_context_and_reports_what_it_omitted() {
        let lines = scored_payload();
        let content = join_lines(&lines);
        let opts = options(Mode::Recoverable, 5);
        let mut refs = recoverable();
        let out = crush_log(&content, &opts, &ctx(), &mut refs, Identity::Values)
            .expect("60 lines score");

        assert!(
            out.contains("ERROR unable to reach cache node 4"),
            "got {out:?}"
        );
        assert!(
            out.contains("at cache::connect(cache.rs)"),
            "the frame under the error is context, got {out:?}"
        );
        for (offset, line) in lines.iter().enumerate().take(12).skip(5) {
            assert!(
                out.contains(line),
                "line {offset} is within three of the error and must be kept"
            );
        }
        assert!(out.contains("log lines omitted"), "got {out:?}");

        // the counts in the marker describe exactly the lines that went
        let missing: Vec<&String> = lines.iter().filter(|line| !out.contains(*line)).collect();
        let errors = missing.iter().filter(|line| ERROR.is_match(line)).count();
        let warnings = missing.iter().filter(|line| WARN.is_match(line)).count();
        assert!(
            out.contains(&format!("; {errors} ERROR, {warnings} WARN]")),
            "marker must report {errors} ERROR / {warnings} WARN, got {out:?}"
        );
        assert_no_orphan_refs(&out, &refs);
    }

    #[test]
    fn the_scored_path_never_drops_a_marker_an_earlier_stage_left() {
        let mut lines = scored_payload();
        // an uninteresting-looking line that is in fact a handle into the store
        let marker = "[... 9 rows omitted [ref:k_abcdefghijkl]]".to_string();
        lines[20] = marker.clone();
        let content = join_lines(&lines);
        let mut refs = recoverable();
        let out = crush_log(
            &content,
            &options(Mode::Recoverable, 5),
            &ctx(),
            &mut refs,
            Identity::Values,
        )
        .expect("60 lines score");
        assert!(
            out.contains(&marker),
            "dropping it would strand a ref nothing can retrieve, got {out:?}"
        );
    }

    #[test]
    fn a_payload_with_no_error_or_summary_word_falls_through_to_template_folding() {
        let lines: Vec<String> = (0..60)
            .map(|index| format!("tick {index} of the shelf inventory"))
            .collect();
        let content = join_lines(&lines);
        let mut refs = recoverable();
        let mut scored_refs = recoverable();
        assert!(
            crush_scored(
                &lines.iter().map(String::as_str).collect::<Vec<_>>(),
                &options(Mode::Recoverable, 5),
                &mut scored_refs
            )
            .is_none(),
            "nothing to score by"
        );
        let out = crush_log(
            &content,
            &options(Mode::Recoverable, 5),
            &ctx(),
            &mut refs,
            Identity::Values,
        )
        .expect("normalization folds `tick <n> …` into one line");
        assert!(out.contains("[×"), "got {out:?}");
        assert!(!out.contains("log lines omitted"));
    }

    #[test]
    fn a_warning_fingerprint_normalises_only_the_right_of_the_separator() {
        assert_eq!(
            warning_fingerprint("WARNING: retry attempt 1 for job 77"),
            warning_fingerprint("WARNING: retry attempt 2 for job 78"),
        );
        assert_ne!(
            warning_fingerprint("WARNING: disk full: /dev/sda1"),
            warning_fingerprint("WARNING: socket closed: peer 10"),
            "distinct classes of warning keep distinct fingerprints"
        );
        assert_eq!(
            warning_fingerprint("WARN: leak at 0xdeadbeefcafe"),
            "WARN: leak at <addr>"
        );
        assert_eq!(
            warning_fingerprint("no separator here"),
            "no separator here"
        );
    }

    #[test]
    fn a_trace_frame_outscores_the_ordinary_line_it_looks_like() {
        assert!(TRACE.is_match("  File \"app.py\", line 42, in handler"));
        assert!(TRACE.is_match("    at server::run(main.rs)"));
        assert!(TRACE.is_match("Caused by: io error"));
        assert!(!TRACE.is_match("at the end of the run"));
    }

    /// The shape that made this necessary: one apt line per package, differing
    /// in a name rather than a number.
    fn apt_transcript(packages: &[&str]) -> String {
        let mut out = String::from("Reading package lists...\nBuilding dependency tree...\n");
        for (n, package) in packages.iter().enumerate() {
            out.push_str(&format!(
                "Selecting previously unselected package {package}.\n\
                 Preparing to unpack .../{n:02}-{package}_1.0.0-1build1_amd64.deb ...\n\
                 Unpacking {package}:amd64 (1.0.0-1build1) ...\n\
                 Setting up {package}:amd64 (1.0.0-1build1) ...\n"
            ));
        }
        out
    }

    /// Names with no digits in them, so the value identity has nothing to
    /// normalise and the test isolates the one dimension it is about.
    fn many_packages() -> Vec<String> {
        const WORDS: [&str; 20] = [
            "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india",
            "juliet", "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo",
            "sierra", "tango",
        ];
        WORDS
            .iter()
            .flat_map(|w| [format!("lib{w}-dev"), format!("lib{w}-common")])
            .collect()
    }

    #[test]
    fn a_build_transcript_folds_lines_that_differ_only_in_a_package_name() {
        let names = many_packages();
        let content = apt_transcript(&names.iter().map(String::as_str).collect::<Vec<_>>());
        let opts = options(Mode::Recoverable, 0);
        let mut refs = opts.collector();
        let out = crush_log(&content, &opts, &ctx(), &mut refs, Identity::ValuesAndNames)
            .expect("an apt transcript folds");
        assert!(
            chars_len(&out) * 3 < chars_len(&content),
            "expected the four line shapes to collapse, got {} from {}",
            chars_len(&out),
            chars_len(&content)
        );
        assert!(out.contains("Selecting previously unselected package libalpha-dev."));
        assert!(out.contains("[×"), "the fold has to say how many it took");
        assert_no_orphan_refs(&out, &refs);
    }

    /// The same payload under the identity every other log gets: the names keep
    /// the lines apart, so there is nothing to fold and nothing is claimed.
    #[test]
    fn value_identity_leaves_the_same_transcript_alone() {
        let names = many_packages();
        let content = apt_transcript(&names.iter().map(String::as_str).collect::<Vec<_>>());
        let opts = options(Mode::Recoverable, 0);
        let mut refs = opts.collector();
        let folded = crush_log(&content, &opts, &ctx(), &mut refs, Identity::Values);
        let shrunk = folded.map_or(0, |out| chars_len(&content) - chars_len(&out));
        assert!(
            shrunk * 3 < chars_len(&content),
            "value identity should not be able to collapse names"
        );
    }

    /// Name folding may never merge two diagnostics: the file name is the field
    /// it erases, and it is the whole content of the message.
    #[test]
    fn two_errors_in_different_files_never_fold_into_one() {
        let mut content = String::new();
        for n in 0..60 {
            content.push_str(&format!("Setting up libthing{n}:amd64 (1.0-1) ...\n"));
        }
        content.push_str("src/alpha.c:7:5: error: 'alpha_handle' undeclared here\n");
        content.push_str("src/bravo.c:8:5: error: 'bravo_handle' undeclared here\n");
        let opts = options(Mode::Recoverable, 0);
        let mut refs = opts.collector();
        let out = crush_log(&content, &opts, &ctx(), &mut refs, Identity::ValuesAndNames)
            .expect("the bookkeeping still folds");
        assert!(out.contains("alpha_handle"), "first error survives: {out}");
        assert!(out.contains("bravo_handle"), "second error survives: {out}");
    }

    /// A payload with diagnostics keeps the scored reading's right of way, so a
    /// shorter fold can never displace the path that preserves errors.
    #[test]
    fn an_error_bearing_payload_still_takes_the_scored_path() {
        let mut content = String::new();
        for n in 0..120 {
            content.push_str(&format!(
                "2026-01-02T03:04:00Z INFO served id={n} status=200\n"
            ));
        }
        content.push_str("2026-01-02T03:05:00Z ERROR db connection refused attempt=1\n");
        let opts = options(Mode::Recoverable, 20);
        let mut refs = opts.collector();
        let out = crush_log(&content, &opts, &ctx(), &mut refs, Identity::ValuesAndNames)
            .expect("a long log compresses");
        assert!(
            out.contains("ERROR db connection refused"),
            "the error has to be in the output: {out}"
        );
    }

    /// Two markers differing only in their handle share a template once names
    /// are folded, and folding one away would leave its fragment unreachable.
    #[test]
    fn lines_carrying_a_handle_are_never_folded_together() {
        let mut content = String::new();
        for n in 0..60 {
            content.push_str(&format!(
                "[... {n} quoted source omitted [ref:k_AbCdEfGhIjK{}]]\n",
                char::from(b'a' + (n % 26) as u8)
            ));
        }
        let opts = options(Mode::Recoverable, 0);
        let mut refs = opts.collector();
        let folded = crush_log(&content, &opts, &ctx(), &mut refs, Identity::ValuesAndNames);
        if let Some(out) = folded {
            assert_no_orphan_refs(&out, &refs);
            for n in 0..60 {
                assert!(
                    out.contains(&format!(
                        "k_AbCdEfGhIjK{}",
                        char::from(b'a' + (n % 26) as u8)
                    )),
                    "handle {n} disappeared from {out}"
                );
            }
        }
    }

    #[test]
    fn crush_log_output_is_never_longer_than_its_input() {
        let scored = join_lines(&scored_payload());
        let payloads = [
            "boot\nretry\nretry\nretry\ndone\n".to_string(),
            "alpha\n\nbeta\ngamma\n".to_string(),
            "===== a =====\nx\nx\n===== b =====\nx\n".to_string(),
            distinct_diagnostics(),
            scored,
            "Chunk ID: abc\nOutput:\nsame\nsame\nsame\n".to_string(),
            "".to_string(),
            "\n\n\n".to_string(),
            "single line".to_string(),
        ];
        for content in payloads {
            for mode in [Mode::Recoverable, Mode::Aggressive, Mode::Lossless] {
                for identity in [Identity::Exact, Identity::Values, Identity::ValuesAndNames] {
                    for keep_tail in [0usize, 2, 20] {
                        let opts = options(mode, keep_tail);
                        let mut refs = opts.collector();
                        if let Some(out) = crush_log(&content, &opts, &ctx(), &mut refs, identity) {
                            assert!(
                                chars_len(&out) < chars_len(&content),
                                "{mode:?}/{identity:?}/{keep_tail} grew {content:?} into {out:?}"
                            );
                            assert_no_orphan_refs(&out, &refs);
                        } else {
                            assert!(
                                refs.is_empty(),
                                "a declined payload must leave nothing stashed"
                            );
                        }
                    }
                }
            }
        }
    }

    /// A six-thousand-step training log whose worker dies two thirds of the
    /// way through. `crash` is the three lines that announce it.
    fn training_log_with(crash: &[&str]) -> String {
        let mut lines: Vec<String> = vec!["Loading dataset /data/train.jsonl".to_string()];
        for step in 1..=6000 {
            lines.push(format!(
                "epoch 1 step {step}/6000 loss=1.8520 lr=0.000300 tok/s=50000"
            ));
            if step == 3000 {
                lines.extend(crash.iter().map(|line| (*line).to_string()));
            }
        }
        lines.push("Training complete".to_string());
        join_lines(&lines)
    }

    #[test]
    fn a_native_crash_survives_the_scored_path_without_a_python_header() {
        // `Fatal Python error:` would match on `fatal` and on `error`, so a
        // CPython-wrapped segfault was always kept. A process that dies under
        // any other runtime says none of those words, and was dropped out of
        // the middle of the log while the marker reported `0 ERROR`.
        let content = training_log_with(&[
            "Segmentation fault (core dumped)",
            "Received signal 11 SIGSEGV, address not mapped to object",
            "Restarting worker 3 from checkpoint step-2999",
        ]);
        let mut refs = recoverable();
        let out = crush_log(
            &content,
            &options(Mode::Recoverable, 5),
            &ctx(),
            &mut refs,
            Identity::Values,
        )
        .expect("6000 lines score");

        assert!(
            out.contains("Segmentation fault (core dumped)"),
            "the crash must survive, got {out:?}"
        );
        assert!(
            out.contains("SIGSEGV"),
            "the signal line must survive, got {out:?}"
        );
        assert_no_orphan_refs(&out, &refs);
    }

    #[test]
    fn the_marker_makes_no_error_claim_when_it_had_no_error_vocabulary() {
        // `0 ERROR, 0 WARN` over a span the scorer never searched for errors is
        // a claim it cannot support: it means "nothing matched words I know",
        // and it reads as "nothing is wrong in here". Only a payload that
        // exercised the vocabulary earns the count.
        let lines: Vec<String> = (0..200)
            .map(|index| format!("tick {index} of the shelf inventory"))
            .chain(std::iter::once("Summary: 200 ticks".to_string()))
            .collect();
        let content = join_lines(&lines);
        let mut refs = recoverable();
        let out = crush_scored(
            &split_lines(&content),
            &options(Mode::Recoverable, 5),
            &mut refs,
        )
        .expect("the summary line scores");

        assert!(out.contains("omitted"), "it must still omit, got {out:?}");
        assert!(
            !out.contains("ERROR"),
            "no error vocabulary was exercised, so the marker must not count \
             errors it never looked for: {out:?}"
        );
    }

    #[test]
    fn the_marker_still_reports_counts_when_the_vocabulary_was_exercised() {
        // The other half of the rule: when the payload did carry error words,
        // the scorer found them and kept them, so a zero in the omitted span
        // means "nothing wrong went behind the handle" and is worth saying.
        let content = training_log_with(&[
            "Fatal Python error: Segmentation fault",
            "Segmentation fault (core dumped)",
            "Restarting worker 3 from checkpoint step-2999",
        ]);
        let mut refs = recoverable();
        let out = crush_log(
            &content,
            &options(Mode::Recoverable, 5),
            &ctx(),
            &mut refs,
            Identity::Values,
        )
        .expect("6000 lines score");
        assert!(
            out.contains("ERROR, ") && out.contains(" WARN]"),
            "the count is supported here and must be reported, got {out:?}"
        );
    }

    /// Every term the vocabulary claims to recognise, asserted one by one.
    ///
    /// This table exists because the alternation was once written across
    /// several source lines with a trailing `\` on each, which inside a Rust
    /// *raw* string is a literal backslash and a newline rather than a line
    /// continuation. Five terms silently came to require a trailing newline and
    /// matched nothing; the compression gates did not move, because no corpus
    /// payload contained them. Only naming each term catches that.
    #[test]
    fn the_error_vocabulary_recognises_every_term_it_claims() {
        for line in [
            "ERROR unable to reach cache node 4",
            "build failed after 3 retries",
            "FAILURE: task :app:compile",
            "Fatal Python error: Segmentation fault",
            "thread 'main' panicked at src/main.rs:4",
            "Uncaught exception in handler",
            "CRITICAL disk controller offline",
            "Segmentation fault (core dumped)",
            "segfault at 0 ip 00007f sp 00007f error 4",
            "Received signal 11 SIGSEGV, address not mapped to object",
            "SIGABRT received, dumping core",
            "double free or corruption (out)",
            "*** stack smashing detected ***",
            "CUDA out of memory. Tried to allocate 2.00 GiB",
            "Out of memory: oom-killed process 1234",
            "terminate called after throwing an instance of 'std::bad_alloc'",
            "Aborted (core dumped)",
            "ls: cannot open directory '/srv/x': Permission denied",
            "mount: /mnt: access denied",
            "chown: changing ownership: Operation not permitted",
            "ConnectionRefusedError: [Errno 111] Connection refused",
            "upstream connection reset by peer",
            "write: broken pipe",
            "curl: (28) Operation timed out after 30001 milliseconds",
            "OSError: [Errno 28] No space left on device",
            "cannot write checkpoint: disk full",
            "touch: cannot touch '/x': Read-only file system",
            "accept: too many open files",
            "bind: address already in use",
            "deadlock detected while waiting on lock 7",
        ] {
            assert!(
                ERROR.is_match(line),
                "the vocabulary must recognise {line:?}"
            );
        }

        // …and the words deliberately left out, so a later widening is a
        // decision rather than an accident.
        for line in [
            "Killed",
            "killed 3 stale workers",
            "config: timeout=30",
            "no such file or directory",
            "W&B: run limit reached, metrics will be dropped",
        ] {
            assert!(
                !ERROR.is_match(line),
                "{line:?} is deliberately not error vocabulary"
            );
        }
    }
}
