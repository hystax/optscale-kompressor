//! The line-number column a tool prints in front of a payload's real content.
//!
//! `cat -n`, `nl`, `grep -n`, `sed -n '=;p'` and most agent file-reading tools
//! prefix every line with its number. That column is a few characters wide and
//! it defeats the entire detector chain, because every routing rule in it is a
//! regex anchored at the start of a line. With a gutter in front:
//!
//! * `    75\tdef __init__(self, points):` is not a Python definition,
//! * `696:I20260914 22:08:03.659045  7430 solver.cpp:397]` has no leading
//!   timestamp, so it is not a log line,
//! * and a tree-sitter grammar reports a syntax error on the first byte.
//!
//! So the payload is classified `text`, routed to passthrough, and returned
//! whole. Over 1,336 payloads of real agent traffic this happened to 85 of them
//! carrying 219,451 characters — code and logs the engine compresses well once
//! it can see them.
//!
//! The fix is to lift the column off, route the body normally, and put the
//! numbers back on the lines that survived. Re-attachment is what makes this
//! worth doing rather than simply stripping: line numbers are how an agent
//! addresses an edit, and a compressor that silently renumbers a file it was
//! asked to read has taken away more than it saved.
//!
//! ## What re-attachment can and cannot do
//!
//! Compressors *select* lines and *insert* markers; they do not usually rewrite
//! the lines they keep. So a kept line can be matched back to the input line it
//! came from and given its number again, while a marker — which corresponds to
//! no input line — is emitted bare, which also reads correctly: the gap in the
//! numbering is exactly what the marker is announcing.
//!
//! When a compressor *does* rewrite (the log template fold replaces the varying
//! parts of a line with placeholders), almost nothing matches, the output comes
//! back unnumbered, and that is the honest result — a folded template does not
//! belong to any single line.
//!
//! A blank line is never given its number back, because an empty line matches
//! every other empty line and letting one claim a number would let it claim the
//! wrong one. The gap it leaves is cosmetic — the numbering around it is still
//! correct, and a blank line's number carries nothing.
//!
//! One fidelity cost is real and worth stating: fragments retained for
//! `recoverable` retrieval are cut from the *stripped* body, so a fragment
//! fetched back arrives without its line numbers. The compressed view keeps
//! them; the recovered text does not.

use std::collections::HashMap;

use crate::text::{is_blank, split_lines};

/// Too few lines for a numeric prefix to be a column rather than a coincidence.
const MIN_GUTTER_LINES: usize = 5;

/// Share of non-blank lines that must carry the prefix. Not 1.0: real `grep -n`
/// output interleaves `--` separators between match groups, and a `cat -n` of a
/// file with a trailing banner still has a gutter.
const MIN_GUTTER_SHARE: f32 = 0.8;

/// The same share for an arrow column, which is unambiguous enough not to need
/// the majority the other separators do — and never arrives as one. Still a
/// floor rather than zero, so a single stray `1\u{2192}` in a long payload does
/// not put the whole thing through the lift.
const MIN_ARROW_GUTTER_SHARE: f32 = 0.05;

/// Widest number treated as a line number. Seven digits is past any file anyone
/// reads; beyond that a leading integer is data, not a gutter.
const MAX_DIGITS: usize = 7;

/// A payload split into its line-number column and the content underneath.
pub struct Gutter {
    /// The stripped body: every line with its numeric prefix removed.
    pub body: String,
    /// `prefix` for each body line, in order. `None` for a line that carried no
    /// number, so re-attachment leaves it exactly as it was.
    prefixes: Vec<Option<String>>,
    /// Body line text, for matching output lines back to their origin.
    lines: Vec<String>,
}

/// Split the numeric prefix off `line`, as `(prefix, rest)`.
///
/// The separator is part of the prefix, so `rest` is the content byte for byte
/// and re-attachment is a concatenation.
fn split_prefix(line: &str) -> Option<(&str, &str)> {
    let start = line.len() - line.trim_start_matches([' ', '\t']).len();
    let digits = line[start..].bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || digits > MAX_DIGITS {
        return None;
    }
    let sep = start + digits;
    // `\t` is `cat -n` and `nl`; `:` is a `grep -n` match; `-` is a `grep -C`
    // *context* line, which is the same gutter with a different separator and
    // is mixed freely with `:` in one payload; `|` is a boxed listing; and
    // `\u{2192}` is what claude-code's file reader writes.
    //
    // A space is deliberately excluded: every numbered list in prose would
    // match. `-` is the one that looks risky — `0-10` / `11-20` in a range table
    // parses as a gutter too — and it is safe for the same reason the whole
    // check is: the numbers must be non-decreasing across the payload, and the
    // lift is arbitrated, so a misread costs a second routing attempt and never
    // a byte of output.
    // `\u{2192}` (a right arrow) is claude-code's `Read` tool: it numbers every
    // line as `   123\u{2192}def f():`. Measured over a 316k-payload corpus of
    // real agent traffic, 3,212 payloads carrying 14 MiB arrived that way, and
    // the arrow defeated the same detector chain the other separators do: they
    // classified `text`/passthrough and came back whole. Lifting the column
    // takes them from 13.26% to 36.12% removed and re-routes them from nothing
    // to `source_code`.
    let arrow = line[sep..].starts_with('\u{2192}');
    if !arrow && !matches!(line.as_bytes().get(sep), Some(b'\t' | b':' | b'-' | b'|')) {
        return None;
    }
    let width = if arrow { '\u{2192}'.len_utf8() } else { 1 };
    Some(line.split_at(sep + width))
}

/// Recognise a uniform line-number column, or return `None`.
///
/// Requires the numbers to be **strictly increasing** across the payload. Every
/// tool that numbers lines emits them in order and never twice, so this is the
/// gutter's defining property — and it is what separates one from a column of
/// data that merely happens to be numeric.
///
/// Strictly, not merely non-decreasing, and the difference is load-bearing: a
/// timestamped log line begins `2026-01-01T00:00:00Z …`, which parses as the
/// number 2026 followed by the `-` separator on *every line of the payload*.
/// Non-decreasing accepts that, strips the year off a log, and hands the
/// remainder to whichever compressor the mangled text now looks like. Requiring
/// an increase rejects it, because a log repeats its year and a line number
/// never repeats.
pub fn split_line_gutter(content: &str) -> Option<Gutter> {
    let raw = split_lines(content);
    let nonblank = raw.iter().filter(|l| !is_blank(l)).count();
    if nonblank < MIN_GUTTER_LINES {
        return None;
    }

    let mut prefixes = Vec::with_capacity(raw.len());
    let mut lines = Vec::with_capacity(raw.len());
    let mut numbered = 0usize;
    // Whether every prefix seen so far ended in an arrow. See the share test
    // below for why that earns a lower bar than the other separators.
    let mut all_arrows = true;
    let mut last: u64 = 0;
    for line in &raw {
        match split_prefix(line) {
            Some((prefix, rest)) => {
                all_arrows &= prefix.ends_with('\u{2192}');
                let n: u64 = prefix
                    .trim_start_matches([' ', '\t'])
                    .trim_end_matches([':', '-', '|', '\t', '\u{2192}'])
                    .parse()
                    .ok()?;
                if numbered > 0 && n <= last {
                    // Out of order, or the same number twice: a data column or
                    // a repeated date, not a gutter.
                    return None;
                }
                last = n;
                numbered += 1;
                prefixes.push(Some(prefix.to_string()));
                lines.push(rest.to_string());
            }
            None => {
                prefixes.push(None);
                lines.push((*line).to_string());
            }
        }
    }

    // The share bar exists because a bare number and a `:`/`-`/`|` is something
    // ordinary text does by accident, so a gutter has to dominate the payload
    // to be believed. An arrow prefix is not ambiguous in that way — nothing
    // else writes `123\u{2192}` at the start of a line — and it does not arrive
    // as a dense column: the agent file readers in a 316k-payload corpus number
    // roughly every tenth line as a landmark, 3,001 payloads carrying 13.4 MiB
    // at a share near 0.1. At the full bar those were all refused, the sparse
    // numbers went on breaking detection for the lines they sat on, and the
    // payloads came back as `text`.
    let bar = if all_arrows && numbered >= MIN_GUTTER_LINES {
        MIN_ARROW_GUTTER_SHARE
    } else {
        MIN_GUTTER_SHARE
    };
    if (numbered as f32) < bar * nonblank as f32 {
        return None;
    }
    let mut body = lines.join("\n");
    // Carried rather than inferred, so `reattach(&body)` reproduces the input
    // exactly and a compressor that returns its input unchanged cannot lose a
    // byte on the way back.
    if content.ends_with('\n') {
        body.push('\n');
    }
    Some(Gutter {
        body,
        prefixes,
        lines,
    })
}

impl Gutter {
    /// How many characters the column itself occupies.
    pub fn width(&self) -> usize {
        self.prefixes
            .iter()
            .flatten()
            .map(|p| p.chars().count())
            .sum()
    }

    /// Put the numbers back on the lines of `compressed` that came from the body.
    ///
    /// A line gets its own number or none — never another line's. That needs
    /// more than "the first identical line ahead": once the compressor has
    /// omitted something, an identical line (`}`, `return None`, a repeated log
    /// line) may sit among what was omitted, and taking the first match gave a
    /// kept line the number of one that is no longer there.
    ///
    /// So the output is read in three parts, split at the lines that match
    /// nothing in the body — markers and rewritten templates, which are where
    /// lines went missing:
    ///
    /// - before the first such line nothing has been omitted, and lines are
    ///   matched forwards, each to the first identical line not yet claimed;
    /// - after the last one, lines are matched backwards from the end of the
    ///   body, since what follows the last omission is the body's tail;
    /// - in between, a line is numbered only when it is the single candidate
    ///   left, or directly continues the line numbered just before it.
    ///
    /// Anything else is emitted exactly as the compressor wrote it.
    pub fn reattach(&self, compressed: &str) -> String {
        let mut index: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, line) in self.lines.iter().enumerate() {
            index.entry(line.as_str()).or_default().push(i);
        }
        let output = split_lines(compressed);
        let unmatched = |line: &str| !line.is_empty() && !index.contains_key(line);
        let first_gap = output.iter().position(|line| unmatched(line));
        let last_gap = output.iter().rposition(|line| unmatched(line));
        let mut origin: Vec<Option<usize>> = vec![None; output.len()];

        // The tail, backwards. `limit` ends as the lowest body line it claimed,
        // which nothing before it may reach.
        let mut limit = self.lines.len();
        if let Some(last) = last_gap {
            for at in (last + 1..output.len()).rev() {
                let line = output[at];
                if line.is_empty() {
                    continue;
                }
                if let Some(i) = index
                    .get(line)
                    .and_then(|hits| hits.iter().rev().copied().find(|&i| i < limit))
                {
                    origin[at] = Some(i);
                    limit = i;
                }
            }
        }

        let head_end = first_gap.unwrap_or(output.len());
        let middle_end = last_gap.unwrap_or(output.len());
        let mut cursor = 0usize;
        // The body line the previous output line was matched to, while the run
        // is unbroken.
        let mut previous: Option<usize> = None;
        for at in 0..middle_end.max(head_end) {
            let line = output[at];
            if line.is_empty() {
                // A blank line carries no number; it continues a run only when
                // the body has a blank line in the same place.
                previous = previous
                    .map(|i| i + 1)
                    .filter(|&i| i < limit && self.lines[i].is_empty());
                if let Some(i) = previous {
                    cursor = i + 1;
                }
                continue;
            }
            let hits = index.get(line).map(Vec::as_slice).unwrap_or(&[]);
            let mut ahead = hits.iter().copied().filter(|&i| i >= cursor && i < limit);
            let found = if at < head_end {
                ahead.next()
            } else {
                let continues = previous
                    .map(|i| i + 1)
                    .filter(|&i| i < limit && self.lines[i] == line);
                continues.or_else(|| match (ahead.next(), ahead.next()) {
                    (Some(only), None) => Some(only),
                    _ => None,
                })
            };
            origin[at] = found;
            previous = found;
            if let Some(i) = found {
                cursor = i + 1;
            }
        }

        let mut out = Vec::with_capacity(output.len());
        for (line, origin) in output.iter().zip(&origin) {
            match origin.and_then(|i| self.prefixes[i].as_ref()) {
                Some(prefix) => out.push(format!("{prefix}{line}")),
                None => out.push((*line).to_string()),
            }
        }
        let mut joined = out.join("\n");
        if compressed.ends_with('\n') {
            joined.push('\n');
        }
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_file_reader_numbers_with_an_arrow() {
        // claude-code's `Read`. The arrow was in neither separator set, so the
        // column stayed on and the payload routed to text/passthrough.
        let content = "   1\u{2192}fn main() {\n   2\u{2192}    body();\n   3\u{2192}    more();\n                       \u{20}  4\u{2192}}\n   5\u{2192}// end\n";
        let g = split_line_gutter(content).expect("an arrow gutter");
        assert_eq!(g.body, "fn main() {\n    body();\n    more();\n}\n// end\n");
        assert_eq!(g.reattach(&g.body), content, "the column goes back on");
    }

    #[test]
    fn a_sparse_arrow_column_is_still_a_column() {
        // The shape that actually arrives: a landmark every tenth line rather
        // than a number on each one. Measured at a share near 0.1 over 3,001
        // payloads, which the 0.8 bar refused — and the stray numbers went on
        // breaking detection for the lines that carried them.
        let mut content = String::new();
        for i in 0..70 {
            if i % 10 == 0 {
                content.push_str(&format!("{}→value_{i}\tb\tc\n", i + 1));
            } else {
                content.push_str(&format!("value_{i}\tb\tc\n"));
            }
        }
        let g = split_line_gutter(&content).expect("a sparse arrow gutter");
        assert!(
            !g.body.contains('→'),
            "every landmark comes off: {:?}",
            &g.body[..60]
        );
        assert_eq!(g.reattach(&g.body), content, "and goes back on");
    }

    #[test]
    fn a_sparse_column_is_refused_for_the_ambiguous_separators() {
        // The low bar is the arrow's alone. A colon on one line in ten is what
        // ordinary prose looks like, and lifting it would strip real content.
        let mut content = String::new();
        for i in 0..70 {
            if i % 10 == 0 {
                content.push_str(&format!("{}: value_{i}\n", i + 1));
            } else {
                content.push_str(&format!("value_{i}\n"));
            }
        }
        assert!(split_line_gutter(&content).is_none());
    }

    #[test]
    fn one_stray_arrow_does_not_lift_a_whole_payload() {
        let mut content = String::from("1→first line\n");
        for i in 0..60 {
            content.push_str(&format!("ordinary line {i}\n"));
        }
        assert!(split_line_gutter(&content).is_none());
    }

    #[test]
    fn cat_n_output_is_recognised() {
        let content =
            "    1\tfn main() {\n    2\t    body();\n    3\t    more();\n    4\t}\n    5\t// end\n";
        let g = split_line_gutter(content).expect("a gutter");
        assert_eq!(g.body, "fn main() {\n    body();\n    more();\n}\n// end\n");
    }

    #[test]
    fn grep_n_output_is_recognised() {
        let content = "12:hit one\n48:hit two\n99:hit three\n120:hit four\n200:hit five\n";
        let g = split_line_gutter(content).expect("a gutter");
        assert_eq!(g.body, "hit one\nhit two\nhit three\nhit four\nhit five\n");
    }

    #[test]
    fn grep_context_lines_are_the_same_gutter_with_a_different_separator() {
        // `grep -C` marks matches with `:` and context with `-`, mixed in one
        // payload. Rejecting `-` left every `grep -C` result classified as plain
        // text: 16 payloads and 40,070 characters on a real corpus.
        let content =
            "457-    let a = 1;\n458-    let b = 2;\n462:  fn hit() {\n463-    body();\n464-  }\n";
        let g = split_line_gutter(content).expect("a gutter");
        assert_eq!(
            g.body,
            "    let a = 1;\n    let b = 2;\n  fn hit() {\n    body();\n  }\n"
        );
        assert_eq!(g.reattach(&g.body), content, "round trip stays exact");
    }

    #[test]
    fn a_range_table_is_lifted_but_reattaches_exactly() {
        // `0-10` parses as a gutter and the numbers are ordered, so this *is*
        // lifted. That is acceptable precisely because re-attachment is exact
        // and the lift is arbitrated: a misread costs a second routing attempt,
        // never a byte.
        let content = "0-10 apples\n11-20 pears\n21-30 plums\n31-40 figs\n41-50 dates\n";
        let g = split_line_gutter(content).expect("ordered, so it parses");
        assert_eq!(
            g.reattach(&g.body),
            content,
            "whatever it decided, the bytes must come back"
        );
    }

    /// The regression that `-` introduced: `2026-01-01T00:00:00Z INFO …` parses
    /// as the number 2026 plus a `-` separator on every line, so a timestamped
    /// log read as a fully numbered payload, lost its year, and routed as
    /// whatever the remainder resembled.
    #[test]
    fn a_timestamped_log_is_not_a_line_number_gutter() {
        let content = "2026-01-01T00:00:00Z INFO worker heartbeat ok\n".repeat(200);
        assert!(
            split_line_gutter(&content).is_none(),
            "a repeated year is not a line-number column"
        );
    }

    #[test]
    fn a_date_that_rolls_over_is_still_not_a_gutter() {
        // Different days, same year: still a repeat, still rejected.
        let content = "2026-01-01 ok\n2026-01-02 ok\n2026-01-03 ok\n2026-01-04 ok\n2026-01-05 ok\n";
        assert!(split_line_gutter(content).is_none());
    }

    #[test]
    fn a_repeated_line_number_is_not_a_gutter_either() {
        let content = "5:a\n5:b\n5:c\n5:d\n5:e\n";
        assert!(
            split_line_gutter(content).is_none(),
            "a line number never repeats"
        );
    }

    #[test]
    fn an_unordered_numeric_column_is_data_not_a_gutter() {
        // a `sort | uniq -c`-shaped tally: numeric, aligned, and meaningless to
        // strip
        let content = "413:alpha\n12:beta\n900:gamma\n7:delta\n55:epsilon\n";
        assert!(split_line_gutter(content).is_none());
    }

    #[test]
    fn a_numbered_prose_list_is_not_a_gutter() {
        // separated by a space, which is deliberately not a gutter separator
        let content = "1. first\n2. second\n3. third\n4. fourth\n5. fifth\n";
        assert!(split_line_gutter(content).is_none());
    }

    #[test]
    fn a_timestamp_is_not_a_line_number() {
        // `20260914:` parses as a number but is nine digits wide
        let content = "202609140:a\n202609141:b\n202609142:c\n202609143:d\n202609144:e\n";
        assert!(split_line_gutter(content).is_none());
    }

    #[test]
    fn reattachment_restores_the_numbers_of_surviving_lines() {
        let content = "    1\tkeep me\n    2\tdrop me\n    3\tdrop me too\n    4\tkeep me also\n    5\tlast\n";
        let g = split_line_gutter(content).unwrap();
        let compressed = "keep me\n[… 2 lines omitted …]\nkeep me also\nlast\n";
        assert_eq!(
            g.reattach(compressed),
            "    1\tkeep me\n[… 2 lines omitted …]\n    4\tkeep me also\n    5\tlast\n"
        );
    }

    #[test]
    fn reattaching_an_untouched_body_reproduces_the_input() {
        for content in [
            "    1\tfn a() {}\n    2\tfn b() {}\n    3\tfn c() {}\n    4\tfn d() {}\n    5\tfn e() {}\n",
            "1:x\n2:y\n3:z\n4:w\n5:v",
            "  10|a\n  11|b\n  12|c\n  13|d\n  14|e\n",
        ] {
            let g = split_line_gutter(content).expect("a gutter");
            assert_eq!(g.reattach(&g.body), content, "round trip must be exact");
        }
    }

    #[test]
    fn a_duplicated_line_never_takes_a_number_twice() {
        // both output copies must map to *different* input lines, in order
        let content = "  1\tsame\n  2\tsame\n  3\tsame\n  4\tsame\n  5\tsame\n";
        let g = split_line_gutter(content).unwrap();
        assert_eq!(g.reattach("same\nsame\n"), "  1\tsame\n  2\tsame\n");
    }

    #[test]
    fn blank_lines_do_not_need_the_prefix() {
        let content = "  1\talpha\n\n  3\tbeta\n\n  5\tgamma\n  6\tdelta\n  7\tepsilon\n";
        let g = split_line_gutter(content).expect("blanks are not counted against it");
        assert_eq!(g.reattach(&g.body), content);
    }

    #[test]
    fn the_width_is_what_the_column_costs() {
        let content = "  1\ta\n  2\tb\n  3\tc\n  4\td\n  5\te\n";
        let g = split_line_gutter(content).unwrap();
        assert_eq!(g.width(), 20, "five lines of four characters each");
    }
}
