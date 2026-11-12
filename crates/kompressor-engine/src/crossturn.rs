//! Cross-message verbatim de-duplication.
//!
//! Every compressor in this engine reads one payload at a time, so none of them
//! can see the largest redundancy in an agent conversation: the same bytes shown
//! again in a *later* message. A coding agent does `cat foo.py`, then
//! `sed -n '75,100p' foo.py`, then `git diff`, then `cat foo.py` again, and each
//! of those four blocks compresses well on its own while the conversation
//! carries the file three times over.
//!
//! Measured on 316,526 tool outputs from 3,283 terminal-bench trials, counting
//! only lines of 40 characters or more: **90 MiB of 549 MiB — 16.4% — is a line
//! some earlier output in the same trial already showed**. Repeats *inside* a
//! single payload, which the log fold already collapses, account for 3.3%. The
//! worst trials run 45–52%.
//!
//! # The two invariants
//!
//! **Prefix-monotonic.** Messages are processed in order and a message is only
//! ever matched against text from strictly earlier ones, so the output of
//! messages `0..k` is byte-identical whether or not message `k+1` exists.
//! Appending a turn never rewrites an earlier turn, which is what keeps the
//! provider's cache prefix stable — the same property that makes every other
//! decision in this engine refuse to read the conversation.
//!
//! **The original stays in context.** Only spans present verbatim in an earlier
//! message are folded, and the earliest occurrence is never rewritten, so the
//! text a marker points at is physically above it in the same prompt. That is
//! what makes the marker honest without a handle: the content was not stored
//! somewhere, it is still on screen.
//!
//! # Why it declines in lossless
//!
//! A pointer is not the bytes. The mode whose promise is that nothing is
//! dropped cannot drop a span and name where it used to be, however findable
//! that place is.

use std::collections::HashMap;

use crate::options::EngineOptions;
use crate::refs::refs_in;
use crate::text::{chars_len, is_blank, split_lines};
use crate::types::Mode;

/// A run must be at least this many lines to be worth a pointer.
///
/// Fragmenting a payload into pointers costs the reader more than it saves, and
/// a one-line pointer is rarely shorter than the line it replaces.
pub const MIN_RUN_LINES: usize = 3;

/// …and at least this many characters, so the marker is always a net win.
pub const MIN_RUN_CHARS: usize = 120;

/// A line shorter than this is not evidence of anything: `}`, `---`, a blank,
/// and `  return` repeat everywhere and match across unrelated files.
const MIN_ANCHOR_CHARS: usize = 24;

/// How many earlier positions one line may be tried from.
///
/// A hot line would otherwise make matching quadratic. Candidates are kept in
/// first-seen order and truncated, never sampled, so the choice is stable.
const MAX_ANCHOR_CANDIDATES: usize = 16;

/// Where a line was first seen: which message, and how far into it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Origin {
    message: usize,
    line: usize,
}

/// The text earlier messages have already emitted, indexed by line.
///
/// Built from what each message *ships*, not what it arrived as: a fold can
/// only point at bytes the model will actually see.
#[derive(Default)]
pub struct SeenText {
    /// Every earlier message's emitted lines, in order.
    messages: Vec<Vec<String>>,
    /// Line content to the places it has appeared, first-seen order.
    index: HashMap<u64, Vec<Origin>>,
}

fn line_key(line: &str) -> u64 {
    // FNV-1a: stable across runs and builds, which a `DefaultHasher` is not.
    // The engine's output bytes depend on which lines match, so the hash has to
    // be as deterministic as everything else here.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in line.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn is_anchorable(line: &str) -> bool {
    !is_blank(line) && chars_len(line) >= MIN_ANCHOR_CHARS
}

/// Whether `line` names a fragment the model can fetch back.
///
/// Such a line is never folded. The pointer this module writes is honest only
/// because the text it replaces is still on screen, but a handle is not text
/// the model reads; it is the one way back to what an earlier compression
/// elided. Folding it leaves a pointer to a pointer, and a model that does not
/// make the second hop has nothing left to act on.
///
/// Measured on two opencode bughunt runs (glm-5.3-flash, 2026-11-08), both cut
/// off by the 30-minute stage limit. `ledger.c` and `ledger.cpp` have the same
/// function bodies, so their elided reads differ only in the `#include` line.
/// The cpp read shipped as two lines and `[... 30 lines omitted; shown in full
/// earlier in this conversation]`, with all twelve of its handles under the
/// fold. The model guessed a handle that did not exist, then read the file 386
/// more times, each read folded to the same 126 characters. The second run did
/// the same with `cat ledger.py` 477 times, each read folded to a single line.
fn carries_handle(line: &str) -> bool {
    !refs_in(line).is_empty()
}

impl SeenText {
    /// Record what a message shipped, so later messages can point at it.
    pub fn push(&mut self, text: &str) {
        let ordinal = self.messages.len();
        let lines: Vec<String> = split_lines(text).into_iter().map(str::to_string).collect();
        for (offset, line) in lines.iter().enumerate() {
            if !is_anchorable(line) {
                continue;
            }
            let slot = self.index.entry(line_key(line)).or_default();
            if slot.len() < MAX_ANCHOR_CANDIDATES {
                slot.push(Origin {
                    message: ordinal,
                    line: offset,
                });
            }
        }
        self.messages.push(lines);
    }

    /// How many messages have been recorded.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Whether anything has been recorded.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// The longest run at `origin` that also runs forward from `lines[at]`.
    ///
    /// A run ends at the first line that carries a retrieve handle: see
    /// [`carries_handle`].
    fn run_length(&self, lines: &[&str], at: usize, origin: Origin) -> usize {
        let earlier = &self.messages[origin.message];
        let mut n = 0usize;
        while at + n < lines.len()
            && origin.line + n < earlier.len()
            && lines[at + n] == earlier[origin.line + n].as_str()
            && !carries_handle(lines[at + n])
        {
            n += 1;
        }
        n
    }

    /// Replace runs of `text` that an earlier message already showed.
    ///
    /// `None` when nothing was folded, which is also what a caller gets for a
    /// payload with no repeats — the input is then kept exactly, rather than
    /// rebuilt, so a message that gains nothing cannot lose a trailing newline
    /// to a round trip.
    pub fn fold(&self, text: &str, opts: &EngineOptions) -> Option<String> {
        if self.messages.is_empty() || opts.mode == Mode::Lossless {
            return None;
        }
        let lines = split_lines(text);
        let mut out: Vec<String> = Vec::with_capacity(lines.len());
        let mut at = 0usize;
        let mut folded = 0usize;

        while at < lines.len() {
            let line = lines[at];
            // The longest run wins, and *only* its length is used: the marker
            // carries a count, not a place. An earlier version also broke ties
            // toward the earliest occurrence, which read well and decided
            // nothing — equal-length runs produce identical bytes — so the
            // origin is no longer carried out of this search.
            //
            // Naming the message was considered and rejected. An ordinal costs
            // a dozen characters on every turn the marker survives into, and
            // the model cannot resolve one anyway: it sees a conversation, not
            // an indexed array. The content is a few thousand tokens above;
            // that is the whole address it needs.
            let best = if is_anchorable(line) {
                self.index
                    .get(&line_key(line))
                    .and_then(|origins| {
                        origins
                            .iter()
                            .map(|origin| self.run_length(&lines, at, *origin))
                            .max()
                    })
                    .filter(|run| *run >= MIN_RUN_LINES)
            } else {
                None
            };

            let Some(run) = best else {
                out.push(line.to_string());
                at += 1;
                continue;
            };
            let span: usize = lines[at..at + run].iter().map(|l| chars_len(l) + 1).sum();
            if span < MIN_RUN_CHARS {
                out.push(line.to_string());
                at += 1;
                continue;
            }
            out.push(format!(
                "[... {run} lines omitted; shown in full earlier in this conversation]"
            ));
            folded += run;
            at += run;
        }

        if folded == 0 {
            return None;
        }
        let mut rebuilt = out.join("\n");
        if text.ends_with('\n') {
            rebuilt.push('\n');
        }
        // A fold that did not shrink the message is a marker bought for nothing.
        (chars_len(&rebuilt) < chars_len(text)).then_some(rebuilt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Mode;

    fn opts() -> EngineOptions {
        EngineOptions {
            mode: Mode::Recoverable,
            ..Default::default()
        }
    }

    fn file(n: usize) -> String {
        (0..n)
            .map(|i| format!("    total_{i} = accumulate(rows, {i}, offset={})", i * 7))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_span_an_earlier_message_showed_is_folded() {
        let body = file(12);
        let mut seen = SeenText::default();
        seen.push(&body);
        let out = seen.fold(&body, &opts()).expect("a repeat is folded");
        assert!(out.contains("shown in full earlier"), "{out}");
        assert!(chars_len(&out) < chars_len(&body));
    }

    /// A source read the code compressor elided: every body is a marker line
    /// with its own handle, between the line numbers the read tool prints.
    fn elided_read(include: &str) -> String {
        let mut out = format!("Read file ledger, lines 1-404\n1: #include <{include}>\n2: \n");
        out.push_str("3: static long accumulate(const long *rows, int k, long off) {\n");
        out.push_str("4:     long s = off;\n5:     for (int i = 0; i <= k; i++) s += rows[i];\n");
        out.push_str("6:     return s;\n7: }\n8: \n");
        for i in 0..12 {
            out.push_str(&format!(
                " long reconcile{i:02}(const long *rows, int count) {{/* [... 32 body lines \
                 omitted from reconcile{i:02}; retrieve: ref:k_body{i:02}aaaaaa] */}}\n{}: \n",
                41 + 33 * i
            ));
        }
        out
    }

    #[test]
    fn a_fold_never_hides_a_handle() {
        // The opencode bughunt shape: two files whose elided reads differ only
        // in the include. Before this rule the second read shipped as two
        // lines and one pointer, and its twelve handles went under the fold.
        let mut seen = SeenText::default();
        seen.push(&elided_read("stddef.h"));
        let second = elided_read("cstddef");
        let out = seen
            .fold(&second, &opts())
            .unwrap_or_else(|| second.clone());
        assert_eq!(
            refs_in(&out),
            refs_in(&second),
            "every handle the read carries is still on screen:\n{out}"
        );
        // The shared prefix with no handle in it is still folded: the rule
        // stops a run at a handle, it does not switch the fold off.
        assert!(out.contains("shown in full earlier"), "{out}");
    }

    #[test]
    fn a_reread_keeps_its_handles() {
        // The same read again, which is what the model does when it cannot
        // find a body: each re-read used to come back as one pointer.
        let read = elided_read("cstddef");
        let mut seen = SeenText::default();
        seen.push(&read);
        let out = seen.fold(&read, &opts()).unwrap_or_else(|| read.clone());
        assert_eq!(refs_in(&out).len(), 12, "{out}");
    }

    #[test]
    fn the_first_showing_is_never_rewritten() {
        // Nothing has been seen yet, so the message that introduces the bytes
        // keeps them. Without this the marker would point at a place that does
        // not exist.
        let seen = SeenText::default();
        assert!(seen.fold(&file(12), &opts()).is_none());
    }

    #[test]
    fn appending_a_turn_never_changes_an_earlier_one() {
        // Prefix-monotonicity, the property the provider's cache depends on.
        // Folding message 1 against message 0 must give the same bytes whether
        // or not a message 2 exists.
        let body = file(12);
        let mut two = SeenText::default();
        two.push(&body);
        let first = two.fold(&body, &opts()).unwrap();

        let mut three = SeenText::default();
        three.push(&body);
        let again = three.fold(&body, &opts()).unwrap();
        three.push(&again);
        let _later = three.fold(&body, &opts());
        assert_eq!(first, again, "message 1 is not touched by what follows it");
    }

    #[test]
    fn a_short_or_trivial_repeat_is_left_alone() {
        // Two lines is under the run floor, and `}` repeats in every file ever
        // written — a pointer there costs more than it saves and fragments the
        // payload for nothing.
        let mut seen = SeenText::default();
        seen.push("}\n}\n}\n}\n}\n");
        assert!(seen.fold("}\n}\n}\n}\n}\n", &opts()).is_none());

        let two = "    a_line_long_enough_to_anchor_on = 1\n    another_line_long_enough_too = 2\n";
        let mut seen = SeenText::default();
        seen.push(two);
        assert!(
            seen.fold(two, &opts()).is_none(),
            "two lines is under the floor"
        );
    }

    #[test]
    fn lossless_declines() {
        let body = file(12);
        let mut seen = SeenText::default();
        seen.push(&body);
        let lossless = EngineOptions {
            mode: Mode::Lossless,
            ..Default::default()
        };
        assert!(
            seen.fold(&body, &lossless).is_none(),
            "a pointer is not the bytes"
        );
    }

    #[test]
    fn a_fold_that_would_not_shrink_the_message_is_refused() {
        // Three short lines: the run clears the line floor but the marker is
        // longer than what it replaces.
        let short = "aaaaaaaaaaaaaaaaaaaaaaaaaaaa\nbbbbbbbbbbbbbbbbbbbbbbbbbbbb\ncccccccccccccccccccccccccccc\n";
        let mut seen = SeenText::default();
        seen.push(short);
        let out = seen.fold(short, &opts());
        if let Some(out) = out {
            assert!(chars_len(&out) < chars_len(short), "{out}");
        }
    }

    #[test]
    fn matching_is_stable_across_runs() {
        // The output bytes depend on which lines match, so the hash must not be
        // seeded per process the way `DefaultHasher` is.
        let body = file(12);
        let mut a = SeenText::default();
        a.push(&body);
        let mut b = SeenText::default();
        b.push(&body);
        assert_eq!(a.fold(&body, &opts()), b.fold(&body, &opts()));
    }
}
