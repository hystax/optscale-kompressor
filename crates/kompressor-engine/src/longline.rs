//! Clipping lines that are too long for any line-oriented compressor to help.
//!
//! Every compressor in this engine selects, folds or groups **lines**. A payload
//! whose content sits inside one enormous line therefore defeats all of them at
//! once, and does it quietly: the detector classifies it, a compressor claims
//! it, finds nothing it can drop — there is only one line, and dropping it would
//! drop everything — and the payload is returned whole.
//!
//! On a corpus of 8,617 real agent tool results this was the single largest
//! remaining loss. 179 payloads that nothing else could touch carried 718,425
//! characters in lines over 512 characters long: a 21,488-character mount table
//! on one line, `openssl` progress meters, FASTA and protein sequences, base64
//! blobs, and `grep` hits whose matched line is a minified bundle.
//!
//! What they have in common is that the *interesting* part is at the front. A
//! reader needs to know the line is there and roughly what it is; the remaining
//! twenty kilobytes are almost never what the agent was looking for, and in
//! `recoverable` mode they are one tool call away regardless.
//!
//! # Why this runs last
//!
//! It is a fallback, not a stage. Clipping is blunt — it cuts mid-structure by
//! definition — so it only ever sees payloads that every real compressor has
//! already declined. That ordering is what makes it safe: it cannot take a
//! payload away from a compressor that understood it, and it cannot make a
//! result that was already good worse.

use crate::markers::clip_to;
use crate::options::EngineOptions;
use crate::preamble::split_harness_preamble;
use crate::refs::RefCollector;
use crate::text::{chars_len, split_lines};
use crate::types::Mode;

/// A line must exceed this to be worth clipping, unless configured otherwise.
///
/// The marker costs roughly 30 characters plus a handle, so a limit much below
/// this would spend more on saying what left than it recovers. 512 also happens
/// to be the size gate, which keeps one number in the reader's head.
pub const DEFAULT_MAX_LINE_CHARS: usize = 512;

/// A long line is only clipped when it contains an unbroken non-whitespace run
/// at least this long.
///
/// Length alone is the wrong test, and the corpus says so. A 1,200-character
/// line of English is long because it is a paragraph; clipping it throws away
/// readable content the model may well need, which is the same mistake the
/// listing floor exists to prevent. A 1,200-character line containing a single
/// 3,000-character token is long because something opaque is embedded in it —
/// a path list, a base64 blob, a FASTA sequence, a minified bundle — and the
/// tail of that token is almost never what the agent was looking for.
///
/// Prose has no 120-character words. Measured over the untouched payloads of a
/// 8,617-payload corpus, requiring the run gives up 0.77pp of the 2.99pp that
/// clipping on length alone would take, and in exchange it never cuts a
/// sentence.
const MIN_OPAQUE_RUN: usize = 120;

/// The longest unbroken run of non-whitespace in `line`, in characters.
fn longest_opaque_run(line: &str) -> usize {
    line.split_whitespace()
        .map(|token| token.chars().count())
        .max()
        .unwrap_or(0)
}

/// Fewest tokens a line needs before it can read as a token list.
const MIN_LIST_TOKENS: usize = 64;

/// Share of a token list's tokens that must be machine-shaped.
///
/// The second way a line is opaque: no single token is long, but almost every
/// one is a symbol, a number pair or a path. `nm` symbol tables, `addr:count`
/// dumps and opcode histograms came to 19% of the untouched bytes on the Claude
/// Code replay corpus, one 17,932-character symbol list on two lines re-sent 38
/// times. Measured over every long line in both corpora, prose scores 0.00 to
/// 0.01; the highest-scoring lines that must survive are C++ template
/// diagnostics (0.52 to 0.65) and an HTML row (0.68). 0.8 clears all of them.
const MIN_MACHINE_SHARE: f32 = 0.8;

/// A token a person would not write in a sentence: it carries a digit or one
/// of `_ : / = $`, or a dot that is not closing punctuation.
fn machine_token(token: &str) -> bool {
    token
        .bytes()
        .any(|b| b.is_ascii_digit() || matches!(b, b'_' | b':' | b'/' | b'=' | b'$'))
        || token
            .trim_end_matches(['.', ',', ';', ':', '!', '?', ')'])
            .contains('.')
}

/// Whether `line` is a long run of short machine tokens.
fn token_list(line: &str) -> bool {
    let (mut tokens, mut machine) = (0usize, 0usize);
    for token in line.split_whitespace() {
        tokens += 1;
        machine += usize::from(machine_token(token));
    }
    tokens >= MIN_LIST_TOKENS && machine as f32 >= tokens as f32 * MIN_MACHINE_SHARE
}

/// Whether `line` is both long enough and opaque enough to clip.
fn worth_clipping(line: &str, limit: usize) -> bool {
    chars_len(line) > limit && (longest_opaque_run(line) >= MIN_OPAQUE_RUN || token_list(line))
}

/// Clip every line longer than `opts.max_line_chars`.
///
/// Returns `None` when there is nothing over the limit, when the result is not
/// actually shorter, or in `lossless` mode — a clip drops bytes and is not
/// reversible from the output, whatever handle it leaves behind.
pub fn clip_long_lines(
    content: &str,
    opts: &EngineOptions,
    refs: &mut RefCollector,
) -> Option<String> {
    if opts.mode == Mode::Lossless {
        return None;
    }
    let limit = opts.max_line_chars;
    if limit == 0 {
        return None;
    }

    // The harness frame is short by construction and carries the outcome, so it
    // is held out rather than measured: nothing in it can be over the limit, and
    // a clip landing there would cut the exit code off the payload.
    let (preamble, body) = split_harness_preamble(content);

    let lines = split_lines(body);
    if !lines.iter().any(|line| worth_clipping(line, limit)) {
        return None;
    }

    let mut out = Vec::with_capacity(lines.len());
    for line in &lines {
        if worth_clipping(line, limit) {
            out.push(clip_to(line, limit, refs));
        } else {
            out.push((*line).to_string());
        }
    }
    let mut joined = out.join("\n");
    if body.ends_with('\n') {
        joined.push('\n');
    }
    let result = format!("{preamble}{joined}");
    (chars_len(&result) < chars_len(content)).then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Mode;

    fn opts(mode: Mode) -> EngineOptions {
        EngineOptions {
            mode,
            ..Default::default()
        }
    }

    fn run(content: &str, mode: Mode) -> Option<String> {
        let o = opts(mode);
        let mut refs = o.collector();
        clip_long_lines(content, &o, &mut refs)
    }

    #[test]
    fn a_long_line_of_prose_is_never_clipped() {
        // 1,035 characters, and every word is short: this is a paragraph, not a
        // blob, and cutting it would throw away content the model can read.
        let prose = "the quick brown fox jumps over the lazy dog. ".repeat(23);
        assert!(prose.len() > 1000);
        assert!(
            run(&format!("{prose}\n"), Mode::Aggressive).is_none(),
            "length alone must not be enough to clip"
        );
    }

    #[test]
    fn a_prose_line_carrying_one_opaque_token_is_clipped() {
        // The same sentence with a 3,000-character blob embedded: the blob is
        // what the clip is for, and the sentence in front of it survives.
        let line = format!("see attached payload {} for details", "A".repeat(3000));
        let out = run(&format!("{line}\n"), Mode::Aggressive).expect("clipped");
        assert!(
            out.starts_with("see attached payload "),
            "the readable head stays"
        );
        assert!(out.len() < line.len() / 2);
    }

    #[test]
    fn the_opaque_run_is_measured_in_characters_not_bytes() {
        // 119 two-byte characters is 238 bytes but only 119 chars: under the
        // run threshold, so a line made of them is left alone.
        let short_run = "é".repeat(119);
        let line = format!("{} {}", short_run, "word ".repeat(120));
        assert!(line.len() > 512);
        assert!(
            run(&format!("{line}\n"), Mode::Aggressive).is_none(),
            "119 characters is under the run threshold however many bytes it is"
        );
    }

    #[test]
    fn a_long_list_of_short_symbols_is_clipped() {
        // `nm` output: no token is long, every token is a symbol.
        let symbols: String = (0..400).map(|i| format!("__mips16_op{i}sf3 ")).collect();
        let line = format!("/usr/lib/gcc-cross/mipsel-linux-gnu/12/libgcc.a\n{symbols}\n");
        let out = run(&line, Mode::Aggressive).expect("clipped");
        assert!(
            out.starts_with("/usr/lib/gcc-cross/"),
            "the short header line stays"
        );
        assert!(out.contains("__mips16_op0sf3"), "the list's head stays");
        assert!(out.len() < line.len() / 2);
        // and `addr:count` pairs, the other measured shape
        let pairs: String = (0..300)
            .map(|i| format!("{}:{} ", 3597 + i, 1500 + i))
            .collect();
        assert!(run(&format!("{pairs}\n"), Mode::Aggressive).is_some());
    }

    #[test]
    fn a_long_template_diagnostic_is_never_clipped() {
        // The most machine-like line that must survive: a C++ instantiation
        // trace scores ~0.6, and an agent fixing the build needs all of it.
        let line = "/usr/include/Eigen/src/Core/Redux.h:243:63:   required from ‘static \
            Eigen::internal::redux_impl<Func, Evaluator, 3, 0>::Scalar with Func = \
            Eigen::internal::scalar_sum_op<double, double> and Evaluator = the \
            evaluator of the product of the left hand side and the right hand side ’ "
            .repeat(4);
        assert!(line.len() > 512 && line.split_whitespace().count() >= MIN_LIST_TOKENS);
        assert!(run(&format!("{line}\n"), Mode::Aggressive).is_none());
    }

    #[test]
    fn a_line_under_the_limit_is_left_alone() {
        let content = "short\nlines\nonly\n";
        assert!(
            run(content, Mode::Aggressive).is_none(),
            "nothing over the limit means nothing to do"
        );
    }

    #[test]
    fn one_enormous_line_is_clipped_and_announced() {
        let content = format!("before\n{}\nafter\n", "x".repeat(4000));
        let out = run(&content, Mode::Aggressive).expect("clipped");
        assert!(out.starts_with("before\n"), "neighbours are untouched");
        assert!(out.ends_with("after\n"));
        assert!(
            out.contains(" chars]") || out.contains(" chars ref:"),
            "the clip has to say how much left: {out}"
        );
        assert!(out.len() < content.len());
    }

    #[test]
    fn the_clipped_tail_is_retrievable() {
        let o = opts(Mode::Recoverable);
        let mut refs = o.collector();
        let content = format!("{}\n", "y".repeat(3000));
        let out = clip_long_lines(&content, &o, &mut refs).expect("clipped");
        assert!(out.contains("ref:k_"), "recoverable mode mints a handle");
        let pairs = refs.pairs();
        assert_eq!(pairs.len(), 1, "one clip, one fragment");
        assert!(
            pairs[0].1.len() > 2000,
            "the fragment is the tail that left, not a summary"
        );
    }

    #[test]
    fn lossless_refuses_because_a_clip_drops_bytes() {
        let content = format!("{}\n", "z".repeat(4000));
        assert!(
            run(&content, Mode::Lossless).is_none(),
            "the one mode that promises to drop nothing must drop nothing"
        );
    }

    #[test]
    fn the_harness_frame_is_never_cut() {
        // The frame carries the exit code, which is often the single most
        // load-bearing line in a tool result.
        let content = format!(
            "Chunk ID: abc\nProcess exited with code 1\nOutput:\n{}\n",
            "q".repeat(4000)
        );
        let out = run(&content, Mode::Aggressive).expect("clipped");
        assert!(
            out.starts_with("Chunk ID: abc\nProcess exited with code 1\nOutput:\n"),
            "the frame survives verbatim: {}",
            &out[..80.min(out.len())]
        );
    }

    #[test]
    fn several_long_lines_are_each_clipped() {
        let content = format!("{}\nshort\n{}\n", "a".repeat(2000), "b".repeat(2000));
        let out = run(&content, Mode::Aggressive).expect("clipped");
        assert_eq!(out.matches(" chars").count(), 2, "one marker per long line");
        assert!(out.contains("\nshort\n"), "the short line is untouched");
    }

    #[test]
    fn a_clip_that_would_not_shrink_is_refused() {
        // A limit above everything present: nothing is over it, so nothing is
        // rewritten and no cache-busting change is shipped.
        let o = EngineOptions {
            mode: Mode::Aggressive,
            max_line_chars: 100_000,
            ..Default::default()
        };
        let mut refs = o.collector();
        let content = format!("{}\n", "x".repeat(4000));
        assert!(clip_long_lines(&content, &o, &mut refs).is_none());
    }

    #[test]
    fn disabling_it_turns_it_off_completely() {
        let o = EngineOptions {
            mode: Mode::Aggressive,
            max_line_chars: 0,
            ..Default::default()
        };
        let mut refs = o.collector();
        let content = format!("{}\n", "x".repeat(4000));
        assert!(clip_long_lines(&content, &o, &mut refs).is_none());
    }

    #[test]
    fn the_line_structure_is_preserved_exactly() {
        let content = "a\nb\nc\n";
        // nothing over the limit -> None, so structure cannot drift; and with a
        // tiny limit every line is clipped but the count stays the same
        let o = EngineOptions {
            mode: Mode::Aggressive,
            max_line_chars: 1,
            ..Default::default()
        };
        let mut refs = o.collector();
        assert!(
            clip_long_lines(content, &o, &mut refs).is_none(),
            "clipping one-character lines cannot shrink anything"
        );
    }
}
