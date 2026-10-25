//! Ordered, bounded content classification.
//!
//! One function decides what a payload *is*, and everything downstream follows
//! from that answer. **The order is load-bearing.** Nearly every rule below
//! exists because a looser rule above it claimed payloads it then compressed
//! badly, and the fix was never to make the losing compressor cleverer — it was
//! to stop it being asked. A weak signal must not be allowed to claim a whole
//! payload.
//!
//! First confident hit wins. Detection reads at most the first million
//! characters, so a pathological payload cannot make classification unbounded.

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

use crate::configs::config_flavor;
use crate::echo::{DIAGNOSTIC, echoed_line_count, quoted_source_runs};
use crate::options::EngineOptions;
use crate::preamble::split_harness_preamble;
use crate::text::{char_prefix, chars_len, is_blank, split_lines};
use crate::types::{ContentType, Detection, DetectionMeta, Language, Strategy};

/// How much of a payload classification looks at.
const SAMPLE_CHARS: usize = 1_000_000;

// ---------------------------------------------------------------------------
// patterns
// ---------------------------------------------------------------------------

static DIFF: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^(?:diff --git |@@ -\d|--- a/|\+\+\+ b/)").unwrap());

static HTML: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)<(?:!doctype|html|body|div|table|script)\b").unwrap());
/// A tag named in a comment or a string does not make a payload markup. The tag
/// match alone once claimed a Python script for the comment `# remove <script>
/// tags`, and because HTML has no compressor that payload passed through at 0%
/// and was reported as `html` in analytics — while its sibling payload from the
/// same file compressed 77% as source code. Real markup shows structure: a
/// document root, or tags on a meaningful share of lines. The offending file
/// carried one tag across 9% of its lines; genuine payloads measured 21-100%.
static HTML_ROOT: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)<!doctype\s+html|<html\b").unwrap());
static HTML_TAG_LINE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"</?[A-Za-z][\w:-]*(?:\s[^<>]*)?/?>").unwrap());
const HTML_RATIO: f32 = 0.2;

/// A grep hit. The path must begin with a path-like character after an optional
/// Windows drive, which deliberately excludes timestamp prefixes such as
/// `2026-01-01` that otherwise look like `path-line-content`.
static SEARCH: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?:[A-Za-z]:)?(?P<path>[A-Za-z_./\\][^:\n]*?)[:\-]\d+[:\-].+").unwrap()
});
/// Grep output is grep output nearly line for line, so a handful of matching
/// lines in a mostly-unrelated payload is a false positive, not a weak signal.
/// An `apt-get install` log supplies the shape from hyphenated package versions
/// (`Unpacking cpp-14-x86-64-linux-gnu (14.2.0-19)` reads as ripgrep's
/// `file-line-text` context form), and grouping such a log "by file" invents
/// file names and saves almost nothing: 10.3% against 96.6% for the log
/// compressor.
const SEARCH_RATIO: f32 = 0.6;
/// A clock reading with whitespace in front of it makes the line a log entry,
/// not a grep hit. `SEARCH` only needs the line to start with a path-like
/// character, which glog and syslog do, so `I20260805 07:39:31.047616 13780
/// solver.cpp:239]` parsed as path `I20260805 07` and a whole training log
/// counted as grep output.
///
/// The tell is in the *path* group alone: an hour ends with whitespace then one
/// or two digits, whereas a filename does not. Testing only the path keeps a
/// genuine hit whose match text happens to carry a clock on the search path.
static CLOCK_PATH: Lazy<Regex> = Lazy::new(|| Regex::new(r"[ \t]\d{1,2}$").unwrap());

/// A timestamped or level-prefixed log line.
///
/// The date must be followed by a time separator: a bare `\d{4}-\d\d-\d\d` also
/// starts a filename like `2025-07-03_api.log`, and folding a directory of dated
/// logs as log templates destroyed 20 of 25 distinct dates on a task whose whole
/// job was date ranges. glog and syslog are listed explicitly because without
/// them the payloads freed from the search path above fall through to plain
/// text, where only byte-identical lines fold — and every line of a training log
/// differs by its iteration number, so a 95% compression becomes 0%.
static LOG: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"(?m)^\s*(?:\d{4}-\d\d-\d\d[T ]",
        r"|(?:TRACE|DEBUG|INFO|WARN|ERROR|FATAL)\b",
        r"|[IWEFD]\d{4,8}[ \t]+\d{1,2}:\d{2}:\d{2}",
        r"|[A-Z][a-z]{2}[ \t]+\d{1,2}[ \t]+\d{1,2}:\d{2}:\d{2})",
    ))
    .unwrap()
});

/// Command output with no timestamp and no level keyword: package managers and
/// build tools report progress with a status verb instead. `LOG` cannot claim
/// these without also claiming filename listings, so this anchors on the verb.
/// A real 27.6K apt install log compressed 14.7% as plain text against 96.6%
/// routed here, keeping every `Setting up <pkg> (<version>)` line verbatim.
///
/// The indent allowance is `[ \t]*` rather than `\s*` on purpose: under
/// multi-line mode `\s` also matches the newline, so a `\s*` prefix would let a
/// verb on one line satisfy the anchor of the line above it. pip is listed
/// separately because its vocabulary shares nothing with apt's and it indents
/// sub-steps under the package they belong to, so a column-0 anchor missed them
/// twice over — a 15K `pip install` log fell through to plain text at ~0%.
static BUILD_LINE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"(?m)^[ \t]*(?:Setting up |Unpacking |Selecting previously unselected",
        r"|Preparing to unpack",
        r"|Processing triggers|Reading package lists|Reading state information",
        r"|Reading database|Building dependency tree|Get:|Hit:|Fetched |Need to get",
        r"|After this operation|update-alternatives:|make(?:\[\d+\])?:|Compiling |Linking ",
        r"|Installing |Downloading |added \d+ packages|npm (?:WARN|notice|info)",
        r"|Collecting |Using cached |Requirement already satisfied",
        r"|Preparing metadata|Building wheels? |Created wheel |Stored in directory",
        r"|Successfully installed |Successfully built |Attempting uninstall",
        r"|Uninstalling )",
    ))
    .unwrap()
});
/// Lower than the timestamped floor: even a pure build log spends most of its
/// lines on compiler output, progress bars and blanks, so the status verbs are a
/// minority. Measured across 143 real payloads this claimed exactly one — the
/// install log it is meant for — so the count and the verb list carry the weight.
const BUILD_RATIO: f32 = 0.3;

static TABLE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^\s*\|.+\|\s*$").unwrap());

/// How much of a payload must be table rows before it is a table.
///
/// This rule had a count and no ratio, alone among its neighbours — `search`,
/// `log` and `listing` all require a majority, `build` a third — so three pipe
/// rows anywhere claimed the whole payload. Measured on the shipped corpus that
/// was nine payloads with **6% to 23%** of their lines matching: dataset cards
/// and READMEs with a small table in them, curl progress, rustc errors. All nine
/// were handed to a compressor that declines prose, and all nine compressed at
/// roughly nothing.
///
/// A majority, like its neighbours: a payload that *is* a table is mostly rows,
/// and the ones above top out at 23% with room to spare. A table embedded in
/// prose is the mixed segmenter's job, which is where these now go.
const TABLE_RATIO: f32 = 0.5;

/// A path or name entry. The leading `/?` matters: the first character class
/// excludes `/`, so an *absolute* path never matched and `find /app …` — the
/// command agents reach for most — fell through to plain text. One 2.6K payload
/// of 79 absolute paths scored 0% where the listing sampler gives 88%.
static LISTING_LINE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/?[\w.@+~-]+(?:/[\w.@+~-]+)*/?$").unwrap());
const LISTING_RATIO: f32 = 0.6;
/// A machine dump row: a hex offset at line start. `objdump`, `readelf`, `nm`,
/// `xxd` and `strings -tx` all emit these, and every row is addressable data —
/// so they are sampled verbatim like a listing, never normalised, because a
/// folded address is useless to an agent reversing a binary.
static OFFSET_ROW: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*[0-9a-f]{3,16}:?\s").unwrap());

/// `rg` without `-n` emits `path:matching text`, which `SEARCH` cannot claim for
/// want of a line number — and those lines all share the path, so they read as
/// one template with a varying tail. They are not: they are the matches an agent
/// asked for. A real file path in grep output carries an extension, which is what
/// separates it from the `checking cst:` shape the uniform-stream rule is for.
static PATHLESS_GREP: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[\w./+@~-]*[\w/+@~-]\.[A-Za-z]\w*:.").unwrap());
const PATHLESS_GREP_RATIO: f32 = 0.6;

/// Consecutive lines sharing a long literal prefix: a proof checker naming
/// constants, a build naming targets, a migration naming tables. Every line is
/// *unique*, so neither exact nor template folding reaches them, and none is a
/// path, so the listing rules miss them too — the largest untouched payloads in
/// the corpus were this shape (13,786 lines of `checking cst:compcert.…`,
/// compressed 0%).
const UNIFORM_PREFIX_MIN: usize = 12;
/// The shared part has to carry real content. Source code shares indentation
/// freely, and a run of twelve spaces is not a template — it is a nested block.
const UNIFORM_PREFIX_MIN_VISIBLE: usize = 8;
const UNIFORM_RATIO: f32 = 0.6;
/// Mirrors the listing compressor's own floor: below that it hands the payload
/// back whole, so claiming a shorter one only takes it away from duplicate
/// folding and returns it at 0%.
const UNIFORM_MIN_LINES: usize = 50;
/// Below this the varying part is so short that the lines are near-duplicates,
/// which folding already handles better — it keeps one line verbatim instead of
/// sampling.
const UNIFORM_MIN_SUFFIX: usize = 3;
/// The share of a uniform stream's lines that may carry error signal before the
/// signal is read as part of the template rather than as an event.
///
/// A crash is rare by nature: three lines in six thousand. A column named
/// `error=` is on every line, and diverting such a stream to the scored path
/// buys nothing — it would keep an arbitrary hundred of them and lose the head
/// and tail that make a progress log readable. A word on more than a tenth of
/// the lines belongs to the template.
const UNIFORM_ERROR_MAX_RATIO: f32 = 0.1;

static CPP_INCLUDE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?m)^\s*#\s*include\s*[<"]"#).unwrap());
/// Which of the two C grammars to hand it to. Both parse a plain C file, but only
/// the C++ one reads templates, classes and `::`-qualified definitions — and the C
/// grammar turns those into error nodes, so the per-function gate skips every body
/// inside them. Measured over 297 `#include` payloads: the 48 matching this gain
/// 11,470 characters on the C++ grammar while the other 249 *lose* 3,318 on it, so
/// the choice has to be conditional rather than "prefer cpp".
static CPP_ONLY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"(?m)^\s*(?:template\s*<|using namespace |namespace\s+\w+|public:|private:|protected:)",
        r"|::|#\s*include\s*<(?:iostream|vector|string|map|set|memory|algorithm|sstream|utility)>",
    ))
    .unwrap()
});
static CLASS_BRACE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^\s*class\s+\w+.*\{").unwrap());
/// JavaScript's own idiom, which Java has no spelling of. `class X {` sent the
/// MIPS VM an agent `cat`ted 53 times in one task to the Java grammar, which
/// found no methods in it and returned all 4,786 characters.
static JS_ONLY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"\brequire\(|\bmodule\.exports\b|\bconsole\.log\(|=>",
        r"|(?m)^\s*(?:const|let)\s+[\w{\[]",
        r"|(?m)^\s*(?:async\s+)?function\b",
    ))
    .unwrap()
});
/// Java's, so a JavaScript marker inside a Java file does not move it.
static JAVA_ONLY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?m)^\s*(?:public|private|protected)\s|^import java|\bSystem\.out\b").unwrap()
});
/// Syntax only Rust has here. A C file read from its middle shows no
/// `#include`, and `struct ` at line start then sent it to the Rust grammar: the
/// OCaml runtime's 10,950-character `major_gc.c` came back whole.
static RUST_ONLY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"(?m)^\s*(?:pub(?:\([^)]*\))?\s|fn\s|impl\b|mod\s|use\s+\w+::|#!?\[)",
        r"|\blet\s+(?:mut\s+)?\w+\s*[:=]",
    ))
    .unwrap()
});
/// C declarations Rust cannot spell.
static C_DECLARATION: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"(?m)^\s*(?:typedef\s|extern\s|#\s*define\s)",
        r"|^\s*(?:static\s+|const\s+)*(?:unsigned\s+|signed\s+)?",
        r"(?:void|int|char|long|short|double|float|size_t|u?int\d+_t|struct\s+\w+)\s*\**\s*\w+\s*[\[(=;]",
    ))
    .unwrap()
});
/// A line that opens source whatever the language: a shebang, a block
/// comment, a docstring, a `'use strict'`, an include.
static CODE_OPENER: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"^\s*(?:#!|/\*|"""|'use strict'|#\s*include\b)"#).unwrap());
static VAR_NS_USING: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b(?:var|namespace|using)\b").unwrap());

/// Language signatures, scanned in order.
///
/// C and C++ come before the languages whose keywords they share, and that
/// ordering is the whole fix for a real mis-routing: `struct ` at line start sent
/// C to the Rust grammar, `namespace `/`using ` sent C++ to C#, and `class ` sent
/// JavaScript to Java. Each then parsed under a grammar that found no functions,
/// so the payload came back whole.
///
/// Only the unambiguous markers are promoted. `#include` and `#ifndef` exist in
/// no other language here; `#define`, `#endif` and `#pragma` are C# too, so they
/// stay in the weak tier at the bottom where C# keeps first refusal.
static CODE_TABLE: Lazy<Vec<(Language, Regex)>> = Lazy::new(|| {
    vec![
        (
            Language::Python,
            Regex::new(r"(?m)^(?:def |class |import |from .+ import )").unwrap(),
        ),
        (
            Language::TypeScript,
            Regex::new(r"(?m)^(?:export |interface |(?:async )?function |const .+=>)").unwrap(),
        ),
        (
            // `package name` with nothing after it but a comment: Java's
            // `package vm;` matched the bare prefix and went to the Go grammar.
            Language::Go,
            Regex::new(r"(?m)^(?:package \w+[ \t]*(?://.*)?$|func |import \()").unwrap(),
        ),
        (
            Language::Cpp,
            Regex::new(r"(?m)^\s*(?:template\s*<|using namespace |#\s*include\s*<c?[a-z_]+>)")
                .unwrap(),
        ),
        (
            Language::C,
            Regex::new(r"(?m)^\s*#\s*(?:include|ifndef)\b").unwrap(),
        ),
        (
            // `pub ` matters more than it looks: a library file declares almost
            // everything public, so a crate's own source opened with `pub fn`
            // on every definition and matched none of the bare forms. It fell
            // through the language table entirely and was claimed by the
            // listing rule — 15 KB of Rust routed to the listing sampler
            // instead of the code compressor.
            Language::Rust,
            Regex::new(r"(?m)^(?:pub(?:\([^)]*\))? )?(?:fn |struct |enum |trait |impl |use |mod )")
                .unwrap(),
        ),
        (
            Language::Java,
            Regex::new(r"(?m)^(?:public |private |class |package )").unwrap(),
        ),
        (
            Language::CSharp,
            Regex::new(r"(?m)^(?:namespace |public class |using )").unwrap(),
        ),
        // The weak C tier. Left as text these reached duplicate folding, and a
        // repeated `#endif` is structure, not redundancy: a real header came back
        // with `#endif [x37]` and its include guards merged.
        (
            Language::C,
            Regex::new(r"(?m)^#\s*(?:ifdef|define|endif|pragma)\b").unwrap(),
        ),
    ]
});

static FIELD: Lazy<Regex> = Lazy::new(|| Regex::new(r"\S+").unwrap());
const DELIMITED_RATIO: f32 = 0.6;
const ALIGNED_MIN_FIELDS: usize = 3;
const LISTING_UNIQUE_RATIO: f32 = 0.6;
/// How wide an all-decimal offset has to be before it reads as an address rather
/// than a line number: `nl`/`cat -n`/`awk` number source in 3-6 columns, while
/// `xxd` and `objdump` zero-pad to 8 or 16.
const ADDRESS_MIN_WIDTH: usize = 7;
/// A two-field row's tail, when the row is data rather than prose.
static PAIR_NUMBER: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[+-]?\d[\d.,]*(?:[KMGTPEi]*B|[KMGT]|%|m?s)?$").unwrap());
/// Mirrors the listing compressor's floor, for the same reason as the uniform
/// rule: claiming a shorter payload can only lose.
const PAIR_MIN_ROWS: usize = 50;

// ---------------------------------------------------------------------------
// shared line predicates
// ---------------------------------------------------------------------------

/// Whether `line` opens with a timestamp or a level word.
pub fn looks_like_log_line(line: &str) -> bool {
    LOG.is_match(line)
}

/// Whether `line` is a grep hit rather than a log entry.
///
/// A log line wins outright. Levels and locations coexist happily — `ERROR
/// src/app/main.py:42: connection refused` is both a level-prefixed log entry
/// and, to the search pattern, a `path:line:text` hit — and the clock guard
/// cannot separate them because there is no clock. Deciding it here means the
/// detector and the mixed segmenter cannot disagree about the same line, which
/// they previously could. The cost is a grep hit in a file named `ERROR.md`,
/// which routes to the log crusher instead: compressed differently, never
/// silently emptied.
pub fn looks_like_search_line(line: &str) -> bool {
    if looks_like_log_line(line) {
        return false;
    }
    match SEARCH.captures(line) {
        Some(caps) => !CLOCK_PATH.is_match(caps.name("path").map(|m| m.as_str()).unwrap_or("")),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// the chain
// ---------------------------------------------------------------------------

/// Classify `content`. First confident hit wins; see the module docs on order.
pub fn detect_content(content: &str) -> Detection {
    detect_content_with(content, true, true)
}

/// As [`detect_content`], with the gutter-aware step under a flag.
///
/// The flag exists because the recorded vectors were captured before this step
/// existed, and pinning it off there keeps them a check on everything else.
pub fn detect_content_with(content: &str, gutter_aware: bool, tabular_ratio: bool) -> Detection {
    let sample = char_prefix(content, SAMPLE_CHARS);
    let truncated = sample.len() < content.len();

    // Lift the line-number column first, because it defeats every rule below
    // it: they are regexes anchored at the start of a line, and a gutter puts
    // `    75\t` in front of every one. This used to sit in the *config* branch
    // alone, so it only ever rescued payloads that already looked like config.
    //
    // What that cost, measured: `cat -n` source -- which is what Claude Code's
    // `Read` returns -- was claimed by the tabular rule, because `N\tcontent`
    // is two columns. It then went to the listing sampler and had arbitrary
    // *rows dropped*, reporting 70.9% saved against 29.4% for the same file
    // unnumbered. A better ratio and the wrong kind of lossy: eliding a
    // function body behind a retrievable handle preserves the structure the
    // agent is about to reason about, and dropping every third line does not.
    //
    // Safe to run this early because the predicate is narrow -- strictly
    // increasing numbers over at least 80% of non-blank lines, separated by
    // `:`, `-`, `|`, tab or an arrow, none of which a CSV's comma matches --
    // and because `reattach` puts the numbers back on the lines that survived,
    // which is what makes lifting honest rather than a silent renumbering.
    if gutter_aware && let Some(gutter) = crate::gutter::split_line_gutter(content) {
        return detect_content_with(&gutter.body, gutter_aware, tabular_ratio);
    }

    if let Some(found) = try_json(content, sample, truncated) {
        return found;
    }
    // The same test again, with the harness frame held out.
    //
    // Every other rule below is a ratio over lines, and a few extra lines only
    // nudge one. The JSON test is a *parse*: it does not degrade with a frame in
    // front of it, it fails outright, and `try_json` gives up on the first byte
    // because a payload that opens `Chunk ID:` does not open `[`. So a complete,
    // valid JSON array from any harness that stamps a frame — which is every one
    // of them — was classified as plain text. Measured on a live codex session,
    // a 29 KB array detected as `text`, and as `json_array` the moment the five
    // frame lines came off.
    //
    // Narrow on purpose. `enable_preamble_aware_detection` re-runs the *whole*
    // chain on the body and loses 0.30pp of corpus coverage doing it, because
    // reclassifying is not the same as compressing better. This moves only the
    // one rule that cannot be argued with.
    let (preamble, body) = split_harness_preamble(content);
    if !preamble.is_empty() && !body.is_empty() {
        let body_sample = char_prefix(body, SAMPLE_CHARS);
        if let Some(found) = try_json(body, body_sample, body_sample.len() < body.len()) {
            return found;
        }
    }
    if DIFF.is_match(sample) {
        return Detection::new(ContentType::GitDiff, 0.95);
    }
    if HTML.is_match(sample) && looks_like_html(sample) {
        return Detection::new(ContentType::Html, 0.95);
    }

    let lines = split_lines(sample);

    let diagnostics = DIAGNOSTIC.find_iter(sample).count();
    if diagnostics >= 3 {
        return build(0.9, true, false);
    }
    if diagnostics >= 1 {
        // Three exists so a stray `foo.py:4: error:` quoted inside prose cannot
        // claim a payload, and it is the right floor while the diagnostic line is
        // the only evidence. A compiler that echoes the offending source and
        // points a caret at it corroborates itself, and two warnings out of one
        // gcc invocation are build output however few they are — a real
        // 2,334-character payload had exactly two and passed through at 0%.
        if echoed_line_count(&quoted_source_runs(&lines)) >= 2 {
            return build(0.85, true, false);
        }
    }

    let line_count = lines.len().max(1) as f32;

    let search_lines = lines.iter().filter(|l| looks_like_search_line(l)).count();
    let search_ratio = search_lines as f32 / line_count;
    if search_lines >= 3 && search_ratio >= SEARCH_RATIO {
        return Detection::new(ContentType::SearchResults, search_ratio.min(1.0));
    }

    let log_lines = LOG.find_iter(sample).count();
    if log_lines >= 5 && log_lines as f32 / line_count >= 0.6 {
        return build(0.85, false, false);
    }

    let build_lines = BUILD_LINE.find_iter(sample).count();
    if build_lines >= 5 && build_lines as f32 / line_count >= BUILD_RATIO {
        return build(0.8, false, true);
    }

    let table_lines = TABLE.find_iter(sample).count();
    let mostly_rows = !tabular_ratio || table_lines as f32 / line_count >= TABLE_RATIO;
    if (table_lines >= 3 && mostly_rows) || looks_delimited(sample) {
        return Detection::new(ContentType::Tabular, 0.8);
    }

    if CPP_INCLUDE.is_match(sample) {
        // Decided before the `class X {` heuristic and before the language table,
        // both of which hand a C++ translation unit to C#: the former picks C#
        // when the sample merely contains the word `using` or `namespace`, the
        // latter orders C# ahead of the C rule. `#include` is exclusive to C and
        // C++ — C# has `#if`/`#define`/`#region` but no `#include`, and Java has
        // no preprocessor — so it settles the question outright.
        let language = if CPP_ONLY.is_match(sample) {
            Language::Cpp
        } else {
            Language::C
        };
        return code(0.7, language);
    }
    if CLASS_BRACE.is_match(sample) {
        let language = if JS_ONLY.is_match(sample) && !JAVA_ONLY.is_match(sample) {
            Language::JavaScript
        } else if VAR_NS_USING.is_match(sample) {
            Language::CSharp
        } else {
            Language::Java
        };
        return code(0.8, language);
    }
    for (language, pattern) in CODE_TABLE.iter() {
        if pattern.is_match(sample) {
            let language = if *language == Language::Rust
                && !RUST_ONLY.is_match(sample)
                && C_DECLARATION.is_match(sample)
            {
                Language::C
            } else {
                *language
            };
            return code(0.7, language);
        }
    }

    let body_lines: Vec<&str> = lines.iter().copied().filter(|l| !is_blank(l)).collect();
    // After source code, never before: a file of similar declarations shares
    // prefixes too, and it belongs to the grammar-aware compressor. What reaches
    // here is machine-generated progress output, sampled verbatim for the same
    // reason a listing is — the varying tail is the entire content of the line.
    if looks_uniform_stream(&body_lines) {
        // …unless something in the stream went wrong. The sampler keeps a fixed
        // head and tail and scores nothing, so a crash two thirds of the way
        // through a training log is elided with the routine lines and the
        // marker gives no hint it is in there. That is the head/tail failure
        // mode the log path's scored selection exists to avoid, so hand it over
        // — it costs about a kilobyte on a payload of this shape.
        // Summaries are subtracted: `0 tests failed` carries error vocabulary
        // while reporting that there were none, and counting it diverted a
        // clean test log. A crash says `Segmentation fault`, not `0 failed`.
        let errors = body_lines
            .iter()
            .filter(|line| crate::logs::is_error_line(line) && !crate::logs::is_summary_line(line))
            .count();
        let error_signal =
            errors > 0 && (errors as f32 / body_lines.len() as f32) <= UNIFORM_ERROR_MAX_RATIO;
        let meta = DetectionMeta {
            uniform_stream: true,
            error_signal,
            ..Default::default()
        };
        return Detection::with_meta(ContentType::Listing, 0.8, meta);
    }
    // Last resort before prose: a listing claim is weak, so anything more
    // specific must get the chance to claim first.
    if looks_like_listing(&lines) {
        return Detection::new(ContentType::Listing, 0.75);
    }
    // After listing, not before: a `readelf` symbol table reads as YAML to the
    // config classifier (its header and its rows both match the mapping
    // pattern), and those rows are addressable data that must be sampled
    // verbatim rather than have their "comments" elided.
    if let Some(flavor) = config_flavor(sample) {
        // …and a line-number gutter reads as YAML for the same reason: `27:` is
        // a mapping key to this classifier, so `grep -n` over a C file is
        // "yaml" with 0.80 confidence. The config compressor then finds no
        // config and declines, and because routing already happened the gutter
        // lift never gets a turn either. Measured across seven Harbor runs: 33
        // payloads, 213 KB, returned whole.
        //
        // Classifying the *stripped* body puts it back on the route its content
        // deserves. One level only, and it cannot recurse: `split_line_gutter`
        // requires strictly increasing numbers, and the body it returns has
        // none left to strip.
        let meta = DetectionMeta {
            flavor: Some(flavor),
            ..Default::default()
        };
        return Detection::with_meta(ContentType::Config, 0.8, meta);
    }

    Detection::new(ContentType::PlainText, 0.1)
}

fn build(confidence: f32, diagnostics: bool, untimestamped: bool) -> Detection {
    let meta = DetectionMeta {
        diagnostics,
        untimestamped,
        ..Default::default()
    };
    Detection::with_meta(ContentType::BuildOutput, confidence, meta)
}

/// Where source code opens in `content`, as a byte offset, when something that
/// is not code comes first.
///
/// Agents read files with compound commands — `ls && cat a.py`, `find . ;
/// cat x` — and the listing in front makes the grammar fail on the first line,
/// so the whole payload ships at 0%. On the Claude Code replay corpus that was
/// the largest single loss: a 621-line interpreter behind 36 file names, sent
/// 30 times, compressed from 17,577 to 3,223 characters once the names were set
/// aside.
///
/// The opener is the first line any language signature matches, or a shebang
/// or a block comment. `None` when code opens on the first line, when nothing
/// opens it, or when the lead-in is more than half the payload — at that point
/// it is not a lead-in.
pub(crate) fn code_starts_at(content: &str) -> Option<usize> {
    let opens = |line: &str| {
        CODE_OPENER.is_match(line)
            || CLASS_BRACE.is_match(line)
            || CODE_TABLE.iter().any(|(_, pattern)| pattern.is_match(line))
    };
    let total = content.lines().count();
    let mut offset = 0;
    for (lines, line) in content.split_inclusive('\n').enumerate() {
        if opens(line.trim_end_matches(['\r', '\n'])) {
            return (lines > 0 && lines * 2 <= total).then_some(offset);
        }
        offset += line.len();
    }
    None
}

/// Which kind of file an opening line belongs to, for telling files apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileFamily {
    Python,
    CLike,
}

/// Lines only a Python file opens with. `class` needs its colon: C++ writes
/// `class Foo : public Bar {` and is not Python.
static PY_FILE_LINE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"^(?:"""|import \w|from [\w.]+ import |def \w+\(|class \w+(?:\([^)]*\))?:\s*$)"#)
        .unwrap()
});
/// Lines only a C-family file opens with.
static C_FILE_LINE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?:#\s*(?:include|define)\b|/\*|typedef\s|static\s+\w|package [\w.]+;)").unwrap()
});
/// A file opener glued to the end of the previous file, which is what `cat`
/// produces when a file has no trailing newline: `print(risk)/*`,
/// `}"""`, `]import numpy`. Quotes are not closers here, so `'import os'` in a
/// string literal is not a join.
static GLUED_FILE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"[)\]};](?P<opener>/\*|"""|#!|#\s*include\b|import \w|from [\w.]+ import |def \w+\(|class \w)"#,
    )
    .unwrap()
});

fn file_family(line: &str) -> Option<FileFamily> {
    if line.starts_with("#!") {
        return line.contains("python").then_some(FileFamily::Python);
    }
    if PY_FILE_LINE.is_match(line) {
        Some(FileFamily::Python)
    } else if C_FILE_LINE.is_match(line) {
        Some(FileFamily::CLike)
    } else {
        None
    }
}

/// Byte offsets where one file of a `cat a.py b.c c.py` ends and the next
/// begins. Empty for a single file.
///
/// `cat` writes the files back to back with nothing between them, so the
/// joins have to be read off the content. Two kinds are found: an opener glued
/// to the previous file's last byte, and an opening line whose language family
/// differs from the file before it — the moment a Python file gives way to C.
/// Two Python files joined on a newline are deliberately not split: they
/// usually still parse as one, and splitting what parses costs nothing but
/// risk. A shebang always starts a file.
///
/// Measured on a Claude Code task that `cat`s five files — Python, C, Python,
/// Python, Python, three of them glued mid-line — and re-sends the result on
/// every turn: the C grammar parsed none of it and all 9,871 characters
/// shipped.
pub(crate) fn file_joins(content: &str) -> Vec<usize> {
    let mut joins = Vec::new();
    let mut family: Option<FileFamily> = None;
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        let text = line.trim_end_matches(['\r', '\n']);
        if let Some(glued) = GLUED_FILE
            .captures(text)
            .and_then(|found| found.name("opener"))
        {
            // The text before the join stays with the file it ends.
            joins.push(offset + glued.start());
            family = file_family(glued.as_str()).or(family);
        } else if offset > 0 && text.starts_with("#!") {
            joins.push(offset);
            family = file_family(text);
        } else if let Some(this) = file_family(text) {
            if family.is_some_and(|current| current != this) && offset > 0 {
                joins.push(offset);
            }
            family = Some(this);
        }
        offset += line.len();
    }
    joins.dedup();
    joins
}

fn code(confidence: f32, language: Language) -> Detection {
    let meta = DetectionMeta {
        language: Some(language),
        ..Default::default()
    };
    Detection::with_meta(ContentType::SourceCode, confidence, meta)
}

fn try_json(content: &str, sample: &str, truncated: bool) -> Option<Detection> {
    if !matches!(
        sample.trim_start().as_bytes().first(),
        Some(b'[') | Some(b'{')
    ) {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<Value>(sample) {
        let meta = DetectionMeta {
            item_count: Some(match &value {
                Value::Array(items) => items.len(),
                _ => 1,
            }),
            ..Default::default()
        };
        return Some(Detection::with_meta(ContentType::JsonArray, 1.0, meta));
    }
    let lines: Vec<&str> = split_lines(sample)
        .into_iter()
        .filter(|l| !is_blank(l))
        .collect();
    if lines.len() > 1
        && lines
            .iter()
            .all(|l| serde_json::from_str::<Value>(l).is_ok())
    {
        let meta = DetectionMeta {
            concatenated_json: true,
            item_count: Some(lines.len()),
            ..Default::default()
        };
        return Some(Detection::with_meta(ContentType::JsonArray, 0.9, meta));
    }
    // A compact document larger than the sample window cannot parse from a
    // prefix, so the failure above says nothing about its validity — and it used
    // to drop such payloads all the way through to plain text and compress them
    // 0%, on exactly the biggest tool results. Match brackets instead of parsing
    // megabytes here; the crusher does the authoritative parse and hands the
    // content back untouched if it really is malformed.
    if truncated
        && matches!(
            content.trim_end().chars().next_back(),
            Some(']') | Some('}')
        )
    {
        let meta = DetectionMeta {
            sample_truncated: true,
            ..Default::default()
        };
        return Some(Detection::with_meta(ContentType::JsonArray, 0.6, meta));
    }
    None
}

/// Whether a tag match is structure or just a mention inside code.
fn looks_like_html(sample: &str) -> bool {
    if HTML_ROOT.is_match(sample) {
        return true;
    }
    let lines: Vec<&str> = split_lines(sample)
        .into_iter()
        .filter(|l| !is_blank(l))
        .collect();
    if lines.is_empty() {
        return false;
    }
    let tagged = lines.iter().filter(|l| HTML_TAG_LINE.is_match(l)).count();
    tagged as f32 / lines.len() as f32 >= HTML_RATIO
}

/// CSV/TSV: most sampled lines must share the first line's field count.
///
/// Three matching lines anywhere used to be enough, so a prose task prompt whose
/// first line happened to hold two commas — with about 23% of lines matching —
/// was called tabular for the whole message. Requiring a majority keeps real
/// delimited data (nearly 100% consistent) while leaving prose alone; an
/// embedded CSV block is picked up by the mixed segmenter instead.
fn looks_delimited(content: &str) -> bool {
    let lines: Vec<&str> = split_lines(content)
        .into_iter()
        .take(100)
        .filter(|l| !is_blank(l))
        .collect();
    if lines.len() < 3 {
        return false;
    }
    let delimiter = if lines[0].contains('\t') { '\t' } else { ',' };
    let columns = field_separators(lines[0], delimiter);
    if columns < 1 {
        return false;
    }
    let matching = lines
        .iter()
        .filter(|l| field_separators(l, delimiter) == columns)
        .count();
    matching >= 3 && matching as f32 / lines.len() as f32 >= DELIMITED_RATIO
}

/// Delimiters that actually separate fields: outside quotes, and outside any
/// bracket pair.
///
/// Counting every delimiter made the rule claim source code. An argument list
/// has a fixed arity, so `step_11 = compute(entries, 11, offset=154)` repeated
/// down a file is perfectly consistent in comma count — 91 of 100 lines
/// matching — and the payload was called `tabular` and handed to a compressor
/// that declines it. Measured on a live codex result: the half of a Python file
/// after a truncation marker declined at 0.00% for exactly this reason, where
/// the code compressor takes 91% of the other half.
///
/// A comma inside `(…)` is an argument separator, not a column separator. A
/// real row keeps its delimiters at depth zero, and a field that contains a
/// bracket keeps it balanced, so this leaves genuine data alone — including the
/// quoted fields the CSV dialect allows to contain the delimiter itself.
fn field_separators(line: &str, delimiter: char) -> usize {
    let mut depth = 0usize;
    let mut quoted = false;
    let mut count = 0usize;
    for c in line.chars() {
        match c {
            '"' => quoted = !quoted,
            _ if quoted => {}
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            c if c == delimiter && depth == 0 => count += 1,
            _ => {}
        }
    }
    count
}

fn shared_prefix(left: &str, right: &str) -> usize {
    left.chars()
        .zip(right.chars())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum()
}

/// Whether most consecutive lines are one template with a varying tail.
fn looks_uniform_stream(lines: &[&str]) -> bool {
    if lines.len() < UNIFORM_MIN_LINES {
        return false;
    }
    let grep_hits = lines.iter().filter(|l| PATHLESS_GREP.is_match(l)).count();
    if grep_hits as f32 / lines.len() as f32 >= PATHLESS_GREP_RATIO {
        return false;
    }
    let mut uniform = 0usize;
    for pair in lines.windows(2) {
        let (previous, line) = (pair[0], pair[1]);
        // measured against the PREVIOUS line rather than the whole payload:
        // these streams walk through a namespace, so the global common prefix
        // collapses to almost nothing while each neighbouring pair still shares
        // most of its text
        let shared = shared_prefix(previous, line);
        if chars_len(&line[..shared]) < UNIFORM_PREFIX_MIN {
            continue;
        }
        if line[..shared].trim().chars().count() < UNIFORM_PREFIX_MIN_VISIBLE {
            continue;
        }
        if chars_len(&line[shared..]) < UNIFORM_MIN_SUFFIX {
            continue;
        }
        uniform += 1;
    }
    uniform as f32 / (lines.len() - 1) as f32 >= UNIFORM_RATIO
}

/// A listing: bare path/name entries, or whitespace-aligned columns.
fn looks_like_listing(all_lines: &[&str]) -> bool {
    let lines: Vec<&str> = all_lines.iter().copied().filter(|l| !is_blank(l)).collect();
    if lines.len() < 5 {
        return false;
    }
    // A listing is DISTINCT entries — that is what makes each line payload. A
    // block of repeated lines is not a listing whatever its shape, and duplicate
    // folding serves it far better than sampling would.
    let distinct: std::collections::HashSet<&&str> = lines.iter().collect();
    if (distinct.len() as f32 / lines.len() as f32) < LISTING_UNIQUE_RATIO {
        return false;
    }
    let entries = lines.iter().filter(|l| LISTING_LINE.is_match(l)).count();
    if entries >= 5 && entries as f32 / lines.len() as f32 >= LISTING_RATIO {
        return true;
    }
    let offset_rows: Vec<&&str> = lines.iter().filter(|l| OFFSET_ROW.is_match(l)).collect();
    if offset_rows.len() >= 5
        && offset_rows.len() as f32 / lines.len() as f32 >= LISTING_RATIO
        && offsets_are_addresses(&offset_rows)
    {
        return true;
    }
    looks_aligned(&lines) || looks_aligned_pairs(&lines)
}

/// Whether offset-prefixed rows are a machine dump rather than numbered text.
///
/// `nl`, `cat -n`, `awk NR>=x` and `grep -n` produce the same `digits:` shape
/// over source code, and sampling those drops the very lines an agent asked for
/// by range: on a real run five payloads lost 31-89 lines of C at 74-88%
/// "savings", so each read had to be repeated. Addresses are hexadecimal, so
/// across dozens of rows they sweep through `a-f`; failing that they are
/// zero-padded wide. Decimal line numbers are neither.
fn offsets_are_addresses(rows: &[&&str]) -> bool {
    let mut widths = Vec::new();
    for row in rows {
        let digits: String = row
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect();
        if digits.is_empty() {
            continue;
        }
        if digits.chars().any(|c| ('a'..='f').contains(&c)) {
            return true;
        }
        widths.push(digits.len());
    }
    !widths.is_empty() && widths.iter().min().copied().unwrap_or(0) >= ADDRESS_MIN_WIDTH
}

/// Whether most lines share at least three field start offsets — real column
/// alignment. A paragraph's words start at arbitrary offsets, whereas columns
/// repeat the same ones, so requiring shared *offsets* (not merely a matching
/// field count) keeps uniform prose and indented code out.
fn looks_aligned(lines: &[&str]) -> bool {
    let sample = &lines[..lines.len().min(100)];
    let mut offsets: std::collections::HashMap<usize, usize> = Default::default();
    let mut widths: std::collections::HashMap<usize, usize> = Default::default();
    let mut columnar = 0usize;
    for line in sample {
        let starts: Vec<usize> = FIELD.find_iter(line).map(|m| m.start()).collect();
        if starts.len() < ALIGNED_MIN_FIELDS {
            continue;
        }
        columnar += 1;
        *widths.entry(starts.len()).or_default() += 1;
        for start in starts {
            *offsets.entry(start).or_default() += 1;
        }
    }
    if columnar == 0 || (columnar as f32 / sample.len() as f32) < LISTING_RATIO {
        return false;
    }
    // one field count must dominate: `ls -l` rows all have the same shape, while
    // a paragraph's lines each hold a different number of words
    let dominant = widths.values().max().copied().unwrap_or(0);
    if (dominant as f32 / columnar as f32) < LISTING_RATIO {
        return false;
    }
    let threshold = columnar as f32 * LISTING_RATIO;
    offsets.values().filter(|&&c| c as f32 >= threshold).count() >= ALIGNED_MIN_FIELDS
}

fn pair_tail_is_data(field: &str) -> bool {
    LISTING_LINE.is_match(field) || PAIR_NUMBER.is_match(field)
}

/// Two-field data rows: `wc -l *`, `du -sh */`, `sort | uniq -c`.
///
/// Consulted only after `looks_aligned` declines, and deliberately *not* folded
/// into it by lowering the minimum field count. That shortcut looks equivalent
/// and is not: counting two-field rows as columnar changes the dominant
/// field-count vote inside `looks_aligned`, so payloads that are already clean
/// three-field listings stop qualifying. Measured on the corpus that dropped 13
/// of them out of `listing` (95% saved) and into `text` (8%) — one went from 611
/// to 23,895 characters.
///
/// Two fields is also the shape of ordinary prose, so the shared-offset evidence
/// that carries three-field rows is not enough on its own here: the tail has to
/// say the row is data — a name, a path, or a quantity.
fn looks_aligned_pairs(lines: &[&str]) -> bool {
    let sample = &lines[..lines.len().min(100)];
    let mut pairs = 0usize;
    let mut offsets: std::collections::HashMap<usize, usize> = Default::default();
    for line in sample {
        let starts: Vec<usize> = FIELD.find_iter(line).map(|m| m.start()).collect();
        if starts.len() != 2 {
            continue;
        }
        let last = line.split_whitespace().next_back().unwrap_or("");
        if !pair_tail_is_data(last) {
            continue;
        }
        pairs += 1;
        for start in starts {
            *offsets.entry(start).or_default() += 1;
        }
    }
    if pairs < PAIR_MIN_ROWS || (pairs as f32 / sample.len() as f32) < LISTING_RATIO {
        return false;
    }
    let threshold = pairs as f32 * LISTING_RATIO;
    offsets.values().any(|&c| c as f32 >= threshold)
}

// ---------------------------------------------------------------------------
// routing
// ---------------------------------------------------------------------------

/// Detection with the size gate applied: below `min_compress_chars` the engine's
/// answer is "leave it alone", and nothing downstream should have to re-ask.
pub fn detect_result(content: &str, _role: &str, opts: &EngineOptions) -> Detection {
    if chars_len(content) < opts.min_compress_chars {
        let meta = DetectionMeta {
            skipped_size: true,
            ..Default::default()
        };
        return Detection::with_meta(ContentType::PlainText, 0.0, meta);
    }
    // The size gate is deliberately applied to the whole payload while the
    // classification looks only at the body: the question "is this worth
    // touching" is about what the caller is paying for, and the question "what
    // is this" is about the output the command produced.
    let gutter_aware = opts.enable_gutter_aware_detection;
    let tabular_ratio = opts.enable_tabular_ratio;
    if opts.enable_preamble_aware_detection {
        let (preamble, body) = crate::preamble::split_harness_preamble(content);
        if !preamble.is_empty() && !body.is_empty() {
            return detect_content_with(body, gutter_aware, tabular_ratio);
        }
    }
    detect_content_with(content, gutter_aware, tabular_ratio)
}

/// Which compressor a detected type routes to, honouring the enable flags.
pub fn strategy_for(detection: &Detection, opts: &EngineOptions) -> Strategy {
    match detection.content_type {
        ContentType::JsonArray if opts.enable_smart_crusher => Strategy::SmartCrusher,
        ContentType::SearchResults if opts.enable_search_compressor => Strategy::Search,
        ContentType::BuildOutput if opts.enable_log_compressor => Strategy::Log,
        ContentType::GitDiff if opts.enable_diff_compressor => Strategy::Diff,
        ContentType::Tabular if opts.enable_tabular_compressor => Strategy::Tabular,
        // before the sampler, and only for a uniform stream that carries error
        // signal: the type is still a listing, but the compressor that can see
        // the error has to be the one that gets it
        ContentType::Listing if detection.meta.error_signal && opts.enable_log_compressor => {
            Strategy::Log
        }
        ContentType::Listing if opts.enable_listing_compressor => Strategy::Listing,
        ContentType::SourceCode if opts.enable_code_aware => Strategy::CodeAware,
        ContentType::Config if opts.enable_config_compressor => Strategy::Config,
        // No HTML parser: generated markup repeats one element with a number
        // changed, which the log fold already handles. See the option's docs
        // for what that does and does not claim.
        ContentType::Html if opts.enable_html_compressor => Strategy::Log,
        _ => Strategy::Passthrough,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A uniform progress stream, optionally interrupted at step 3000.
    fn training_log(interruption: &[&str]) -> String {
        let mut lines: Vec<String> = vec!["Loading dataset /data/train.jsonl".to_string()];
        for step in 1..=6000 {
            lines.push(format!(
                "epoch 1 step {step}/6000 loss=1.8520 lr=0.000300 tok/s=50000"
            ));
            if step == 3000 {
                lines.extend(interruption.iter().map(|line| (*line).to_string()));
            }
        }
        lines.push("Training complete".to_string());
        lines.join("\n")
    }

    fn routed(content: &str) -> (Detection, Strategy) {
        let opts = EngineOptions::default();
        let detection = detect_result(content, "tool", &opts);
        let strategy = strategy_for(&detection, &opts);
        (detection, strategy)
    }

    /// `class X {` is JavaScript as often as Java in agent traffic, and the
    /// wrong grammar finds no methods to elide.
    #[test]
    fn a_javascript_class_is_not_routed_to_the_java_grammar() {
        let methods: String = (0..12)
            .map(|n| format!("  op{n}(rs, rt) {{\n    const v = this.regs[rs] + {n};\n    this.regs[rt] = v;\n  }}\n"))
            .collect();
        let js = format!(
            "const fs = require('fs');\n\nclass MIPSVM {{\n{methods}}}\n\nmodule.exports = MIPSVM;\n"
        );
        assert_eq!(routed(&js).0.meta.language, Some(Language::JavaScript));
        let methods: String = (0..12)
            .map(|n| format!("  public void op{n}(int rs) {{\n    pc += {n};\n    System.out.println(pc);\n  }}\n"))
            .collect();
        let java =
            format!("package vm;\n\npublic class Machine {{\n  private int pc;\n{methods}}}\n");
        assert_eq!(routed(&java).0.meta.language, Some(Language::Java));
    }

    /// A C file read from its middle has no `#include` to settle it, and
    /// `struct` at line start is Rust's signature too.
    #[test]
    fn c_read_from_its_middle_is_not_routed_to_the_rust_grammar() {
        let functions: String = (0..12)
            .map(|n| format!("static void mark_slice_{n}(intnat work)\n{{\n  while (work > {n}) {{ work--; }}\n}}\n\n"))
            .collect();
        let c = format!(
            "typedef unsigned int sizeclass;\n\nstruct global_heap_state caml_global_heap_state = {{\n  0 << HEADER_COLOR_SHIFT,\n}};\n\n{functions}"
        );
        assert_eq!(routed(&c).0.meta.language, Some(Language::C));
        let functions: String = (0..12)
            .map(|n| format!("    fn mark_{n}(&mut self) {{\n        let mut work = {n};\n        work += 1;\n    }}\n"))
            .collect();
        let rust = format!("struct Heap {{\n    colour: u8,\n}}\n\nimpl Heap {{\n{functions}}}\n");
        assert_eq!(routed(&rust).0.meta.language, Some(Language::Rust));
    }

    #[test]
    fn code_is_found_behind_a_listing_and_only_behind_a_short_one() {
        let lead_in = "interp.py\ntest\nREADME.md\n";
        let source = "#!/usr/bin/env python3\nimport sys\n\ndef main():\n    return 0\n";
        let both = format!("{lead_in}{source}");
        assert_eq!(code_starts_at(&both), Some(lead_in.len()));
        assert_eq!(
            code_starts_at(source),
            None,
            "code on the first line has no lead-in"
        );
        let long_lead = format!("{}{source}", "name.txt\n".repeat(20));
        assert_eq!(
            code_starts_at(&long_lead),
            None,
            "a listing longer than the code is not a lead-in"
        );
        assert_eq!(code_starts_at("a\nb\nc\n"), None, "nothing opens");
    }

    #[test]
    fn files_concatenated_by_cat_are_split_where_they_join() {
        let python =
            "import math\n\ndef risk(w):\n    return math.sqrt(sum(w))\n\nprint(risk([1]))";
        let c = "/*\n * extension\n */\n#include <math.h>\nstatic double risk(double w) {\n    return sqrt(w);\n}\n";
        let wrapper = "\"\"\"wrapper\"\"\"\n\ndef call(w):\n    return risk(w)\n";
        // `cat a.py b.c c.py`: the first file has no trailing newline, so the C
        // comment is glued to its last line; the third starts on a fresh line.
        let joined = format!("{python}{c}{wrapper}");
        assert_eq!(code_starts_at(python), None);
        assert_eq!(
            file_joins(&joined),
            vec![python.len(), python.len() + c.len()],
            "one glued join, one change of language"
        );

        let two_pythons = format!("{wrapper}import os\n\ndef other():\n    return os.sep\n");
        assert!(
            file_joins(&two_pythons).is_empty(),
            "same language on a newline: not split"
        );
        let cpp = "#include <vector>\nclass Foo : public Bar {\n  int x;\n};\n";
        assert!(
            file_joins(cpp).is_empty(),
            "C++ `class X : Base` is not Python"
        );
        let quoted = "#include <stdio.h>\nconst char *s = \"x\";\nputs('import os');\n";
        assert!(
            file_joins(quoted).is_empty(),
            "an opener inside a string is not a join"
        );
    }

    #[test]
    fn a_crash_inside_a_uniform_stream_goes_to_the_scored_path() {
        // The sampler keeps a fixed head and tail and scores nothing, so a
        // crash two thirds of the way through is elided with the routine lines
        // and the marker gives no hint it is in there.
        let (detection, strategy) = routed(&training_log(&[
            "Segmentation fault (core dumped)",
            "Received signal 11 SIGSEGV, address not mapped to object",
            "Restarting worker 3 from checkpoint step-2999",
        ]));
        assert!(detection.meta.uniform_stream, "still a uniform stream");
        assert!(detection.meta.error_signal, "and it carries a crash");
        assert_eq!(strategy, Strategy::Log, "so the scorer gets it");
    }

    #[test]
    fn a_clean_uniform_stream_still_goes_to_the_sampler() {
        // The diversion must not cost a clean progress log its head and tail.
        let (detection, strategy) = routed(&training_log(&[]));
        assert!(detection.meta.uniform_stream);
        assert!(!detection.meta.error_signal, "nothing went wrong in it");
        assert_eq!(strategy, Strategy::Listing);
    }

    #[test]
    fn a_metric_named_error_is_part_of_the_template_not_an_event() {
        // `error=` on every line is a column, not a crash. Diverting such a
        // stream buys nothing and loses the head and tail that make it
        // readable, so the rarity gate must hold it on the sampler.
        let mut lines: Vec<String> = Vec::new();
        for step in 1..=6000 {
            lines.push(format!(
                "epoch 1 step {step}/6000 loss=1.8520 error=0.0031 tok/s=50000"
            ));
        }
        let (detection, strategy) = routed(&lines.join("\n"));
        assert!(detection.meta.uniform_stream);
        assert!(
            !detection.meta.error_signal,
            "a word on every line belongs to the template"
        );
        assert_eq!(strategy, Strategy::Listing);
    }

    #[test]
    fn a_summary_reporting_no_failures_is_not_an_error() {
        // `0 tests failed` carries error vocabulary while announcing the
        // absence of errors. Counting it diverted a clean test log.
        let (detection, strategy) = routed(&training_log(&[
            "0 tests failed",
            "0 tests not started (parent test skipped or failed)",
        ]));
        assert!(
            !detection.meta.error_signal,
            "a summary of absence is not an event"
        );
        assert_eq!(strategy, Strategy::Listing);
    }
}
