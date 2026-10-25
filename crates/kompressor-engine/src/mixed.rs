//! Offset-preserving compression of the structured spans inside a mixed body.
//!
//! An agent message is rarely one clean payload. It is a paragraph of
//! explanation, a fenced config, another paragraph, a JSON response, a shell
//! transcript. Handing the whole thing to the compressor its dominant type
//! selects makes everything else collateral damage: a diff embedded in prose
//! routed to `crush_diff`, which returned the diff and silently dropped every
//! surrounding sentence.
//!
//! So nothing here rewrites the body. It locates spans, asks the matching
//! compressor about each one, and splices the answers back in by byte offset.
//! **Every byte outside a recognised span comes through untouched** — blank
//! lines and the trailing newline included, which a rebuild-by-joining-lines
//! quietly eats: a mutation with no marker to account for it.
//!
//! Three stages run in order — fences, then embedded JSON, then plain sections
//! — and the first that produces any replacement is the answer. A body whose
//! fences compressed is not then re-scanned for loose JSON inside them.

use std::collections::{BTreeSet, HashSet};
use std::ops::Range;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

use crate::code::crush_code;
use crate::configs::crush_config;
use crate::crusher::crush_json;
use crate::detect::{detect_content, looks_like_log_line, looks_like_search_line};
use crate::diff::crush_diff;
use crate::listing::crush_listing;
use crate::logs::crush_log;
use crate::options::EngineOptions;
use crate::refs::{Mark, RefCollector};
use crate::search::crush_search;
use crate::tabular::crush_tabular;
use crate::text::{chars_len, is_blank, split_lines, split_lines_keepends};
use crate::types::{ContentType, Context, Kind, Language};

static FENCE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?ms)^(?P<open>```(?P<language>[A-Za-z0-9_+-]*)[^\n]*\n)(?P<body>.*?)(?P<close>^```\s*$)",
    )
    .unwrap()
});

/// A delimited row: at least three comma- or tab-separated fields, none of them
/// carrying whitespace. Deliberately narrow — prose with a comma in it must not
/// read as a CSV row.
static DELIMITED_LINE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[^\s,\t]+(?:[,\t][^\s,\t]*){2,}$").unwrap());
/// A bare path or name entry. A run of these inside compound shell output — an
/// `ls` or `find` result between `--- section ---` headers — is a listing.
static LISTING_LINE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/?[\w.@+~-]+(?:/[\w.@+~-]+)*/?$").unwrap());
/// A machine-dump row (objdump, readelf, xxd, `strings -tx`): a hex offset at
/// the start of the line. Compound dumps interleave these runs with header
/// lines, so without a kind of their own nothing segments and the whole field is
/// blind-clipped.
static OFFSET_ROW: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*[0-9a-f]{3,16}:?\s").unwrap());
/// The row's final field names something: it carries a dot or a slash and no
/// code punctuation, which is what separates `… 2025-07-03_api.log` from
/// `… input + 0;`.
static NAMED_TAIL: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:^|\s)[\w@+~-]*[./][\w.@+~/-]*$").unwrap());
/// `<count>` and `{value}` cells mark a template the caller wants emitted
/// verbatim.
static PLACEHOLDER: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)<[a-z_]+>|\{[a-z_]+\}").unwrap());
/// An INI or TOML section header alone on its line.
static CONFIG_SECTION: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\s*\[[A-Za-z_][\w.:-]*\]\s*$").unwrap());
/// A settings assignment: an unindented lower-case dotted key, then `:` or `=`.
///
/// Narrow on purpose, in three ways that each exclude a real false positive
/// seen while building this. The key may not be indented, which keeps the body
/// of a function out. It may not be capitalised, which keeps `Note: this
/// matters` prose and Python's `CONSTANT = 1` out. And it may not contain a
/// space, which keeps an English sentence with a colon in it out.
static CONFIG_ASSIGN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[a-z][a-z0-9_.-]*\s*[:=](?:\s|$)").unwrap());

/// How many assignments stand in for a section header.
///
/// A region qualifies on one `[header]`, because nothing but configuration
/// writes those. Without one it needs a crowd: a handful of `key = value` lines
/// is what the top of many source files looks like, and reading those as
/// configuration would hand the segmenter a payload the code compressor
/// understands far better.
const CONFIG_MIN_ASSIGNMENTS: usize = 8;

const MIN_SECTION_LINES: usize = 5;
const ALIGNED_MIN_FIELDS: usize = 3;
const LISTING_UNIQUE_RATIO: f64 = 0.6;
const PLACEHOLDER_RATIO: f64 = 0.3;
/// Bound on JSON decode attempts, so brace-heavy prose cannot turn the scan
/// quadratic. A failed decode is cheap, but it is not free.
const MAX_JSON_SCAN_ATTEMPTS: usize = 256;

/// Whether `content` holds a structured span with something else around it.
///
/// A body that is *entirely* one JSON document or one log is not mixed: the
/// whole-message route compresses it better and without a second scan.
pub fn has_mixed_content(content: &str, opts: &EngineOptions) -> bool {
    if FENCE.is_match(content) {
        return true;
    }
    for (start, end) in json_spans(content, opts.min_nested_chars) {
        if !is_blank(&content[..start]) || !is_blank(&content[end..]) {
            return true;
        }
    }
    for (start, end, _) in plain_sections(content, opts) {
        if !is_blank(&content[..start]) || !is_blank(&content[end..]) {
            return true;
        }
    }
    false
}

/// Compress only the structured spans of `content`.
///
/// Returns the rewritten body and the sorted distinct kinds actually shrunk, so
/// analytics can unfold one `mixed` message into the real types it contained
/// (`[diff, json, log]` rather than the opaque `mixed`). `None` means nothing
/// was compressed and the caller keeps the payload byte for byte.
pub fn crush_mixed(
    content: &str,
    opts: &EngineOptions,
    ctx: &Context<'_>,
    refs: &mut RefCollector,
) -> Option<(String, Vec<Kind>)> {
    let entry = refs.mark();
    let mut rewrite = Rewrite::default();

    for caps in FENCE.captures_iter(content) {
        let body_match = caps
            .name("body")
            .expect("the pattern always captures a body");
        let body = body_match.as_str();
        let language = caps
            .name("language")
            .map(|m| m.as_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let mark = refs.mark();
        let (compressed, kind) = match language.as_str() {
            "py" | "python" => (
                crush_code(body, opts, ctx, refs, Some(Language::Python)),
                Some(Kind::SourceCode),
            ),
            "yaml" | "yml" | "toml" | "ini" => {
                (crush_config(body, opts, ctx, refs), Some(Kind::Config))
            }
            // An info string we have no mapping for says nothing reliable about
            // the body, so the body is detected on its own terms. The fence
            // itself is never normalized: only the span between the two ``` runs
            // is ever replaced.
            _ => crush_section(body, opts, ctx, refs),
        };
        rewrite.record(body_match.range(), body, compressed, kind, refs, mark);
    }

    if rewrite.is_empty() {
        for (start, end) in json_spans(content, opts.min_nested_chars) {
            let body = &content[start..end];
            let mark = refs.mark();
            let compressed = crush_json(body, opts, ctx, refs);
            rewrite.record(start..end, body, compressed, Some(Kind::Json), refs, mark);
        }
    }

    if rewrite.is_empty() {
        // A configuration span inside a body the detector *already* calls
        // configuration is not a second reading of the payload — it is the same
        // compressor, run on slightly fewer bytes. Fifteen recorded payloads
        // are a harness frame (`Chunk ID:`, `Wall time:`, `Output:`) wrapping a
        // `.cfg`: whole-message routing has always handled those, and letting
        // the segmenter claim them changed nothing but the name in the
        // analytics. Computed once, and only for a body that has such a span.
        let mut whole_is_config: Option<bool> = None;
        for (start, end, kind) in plain_sections(content, opts) {
            if kind == LineKind::Config
                && *whole_is_config.get_or_insert_with(|| {
                    detect_content(content).content_type == ContentType::Config
                })
            {
                continue;
            }
            let body = &content[start..end];
            let mark = refs.mark();
            let (compressed, applied) = crush_section_as(kind, body, opts, ctx, refs);
            rewrite.record(start..end, body, compressed, applied, refs, mark);
        }
    }

    if rewrite.is_empty() {
        refs.rollback(entry);
        return None;
    }

    let out = rewrite.apply(content);
    if chars_len(&out) >= chars_len(content) {
        refs.rollback(entry);
        return None;
    }
    Some((out, rewrite.kinds()))
}

/// The spans accepted so far, and the kinds they were accepted as.
#[derive(Default)]
struct Rewrite {
    replacements: Vec<(usize, usize, String)>,
    applied: BTreeSet<Kind>,
}

impl Rewrite {
    fn is_empty(&self) -> bool {
        self.replacements.is_empty()
    }

    /// Accept `compressed` for `span` when it is genuinely shorter, and
    /// otherwise undo whatever its compressor stashed on the way there.
    ///
    /// The rollback has to be per span. A rejected span keeps its original
    /// bytes, so the fragments behind it have no marker pointing at them, and
    /// the caller's whole-message rollback cannot clean that up: it fires only
    /// when *no* span shrank, and here another span may have.
    fn record(
        &mut self,
        span: Range<usize>,
        body: &str,
        compressed: Option<String>,
        kind: Option<Kind>,
        refs: &mut RefCollector,
        mark: Mark,
    ) {
        match compressed {
            Some(out) if chars_len(&out) < chars_len(body) => {
                self.replacements.push((span.start, span.end, out));
                if let Some(kind) = kind {
                    self.applied.insert(kind);
                }
            }
            _ => refs.rollback(mark),
        }
    }

    /// Splice every accepted span into a copy of `content`.
    ///
    /// Descending order, so each splice leaves the offsets of the ones still to
    /// come exactly where they were measured.
    fn apply(&self, content: &str) -> String {
        let mut out = content.to_string();
        for (start, end, replacement) in self.replacements.iter().rev() {
            out.replace_range(*start..*end, replacement);
        }
        out
    }

    /// The analytics vocabulary, in a stable alphabetical order.
    fn kinds(self) -> Vec<Kind> {
        let mut kinds: Vec<Kind> = self.applied.into_iter().collect();
        kinds.sort_by_key(|kind| kind.as_str());
        kinds
    }
}

// ---------------------------------------------------------------------------
// routing
// ---------------------------------------------------------------------------

/// Route a fenced body by what it *is*, not by what its fence claims.
///
/// `None` for the kind means the content has no compressor here — HTML, a
/// listing, free text — and is left byte for byte.
fn crush_section(
    body: &str,
    opts: &EngineOptions,
    ctx: &Context<'_>,
    refs: &mut RefCollector,
) -> (Option<String>, Option<Kind>) {
    let detection = detect_content(body);
    let kind = Some(detection.content_type.kind());
    match detection.content_type {
        ContentType::JsonArray => (crush_json(body, opts, ctx, refs), kind),
        ContentType::SearchResults => (crush_search(body, opts, ctx, refs), kind),
        ContentType::BuildOutput => (
            crush_log(body, opts, ctx, refs, crate::logs::build_identity(opts)),
            kind,
        ),
        ContentType::GitDiff => (crush_diff(body, opts, ctx, refs), kind),
        ContentType::Tabular => (crush_tabular(body, opts, ctx, refs), kind),
        ContentType::Config => (crush_config(body, opts, ctx, refs), kind),
        ContentType::SourceCode => (
            crush_code(body, opts, ctx, refs, detection.meta.language),
            kind,
        ),
        // Listing is absent on purpose: a *fenced* block the detector reads as
        // a listing is left alone. Only the section path below samples listings,
        // and only behind `worth_sampling`.
        ContentType::Listing | ContentType::Html | ContentType::PlainText => (None, None),
    }
}

/// Compress a plain (unfenced) section whose kind the segmenter already knows.
fn crush_section_as(
    kind: LineKind,
    body: &str,
    opts: &EngineOptions,
    ctx: &Context<'_>,
    refs: &mut RefCollector,
) -> (Option<String>, Option<Kind>) {
    match kind {
        LineKind::Search => (crush_search(body, opts, ctx, refs), Some(Kind::Search)),
        LineKind::Build => (
            crush_log(body, opts, ctx, refs, crate::logs::build_identity(opts)),
            Some(Kind::Log),
        ),
        LineKind::Diff => (crush_diff(body, opts, ctx, refs), Some(Kind::Diff)),
        LineKind::Config => (crush_config(body, opts, ctx, refs), Some(Kind::Config)),
        LineKind::Tabular | LineKind::Listing if !worth_sampling(body, opts) => (None, None),
        LineKind::Tabular => (crush_tabular(body, opts, ctx, refs), Some(Kind::Tabular)),
        LineKind::Listing => (crush_listing(body, opts, ctx, refs), Some(Kind::Listing)),
    }
}

/// Whether dropping rows from this section can be justified.
///
/// Sampling is the one destructive thing the segmenter does to an *embedded*
/// span, and two shapes must never suffer it:
///
/// * a **specification** — a delimited block inside an instruction, marked by
///   `<placeholder>` or `{placeholder}` cells. On one benchmark run a task
///   prompt's 15-row CSV template lost 11 rows, the agent had to guess the
///   period definitions, and the trial failed with correct structure and wrong
///   counts. The prize for that risk was 245 characters.
/// * a **short block**, where there is little redundancy to exploit anyway:
///   sampling 15 rows down to a 10-row budget saves 5 lines. Requiring several
///   times the budget means we only cut where cutting pays.
fn worth_sampling(body: &str, opts: &EngineOptions) -> bool {
    let lines: Vec<&str> = split_lines(body)
        .into_iter()
        .filter(|line| !is_blank(line))
        .collect();
    if lines.len() < MIN_SECTION_LINES.max(3 * opts.max_array_items) {
        return false;
    }
    let placeholders = lines
        .iter()
        .filter(|line| PLACEHOLDER.is_match(line))
        .count();
    (placeholders as f64) / (lines.len() as f64) < PLACEHOLDER_RATIO
}

// ---------------------------------------------------------------------------
// embedded JSON
// ---------------------------------------------------------------------------

/// Byte offsets of every complete JSON value in `content` at least `min_chars`
/// code points long, non-overlapping and in source order.
///
/// This replaced a prefaced-JSON rule that fired only when the blob was the
/// entire trailing suffix *and* no brace appeared before it — so `prose + blob +
/// one more sentence`, and any message carrying two blobs, compressed not at
/// all: 0% measured, against ~97% once the body is segmented. Small inline
/// objects (`{"retries": 3}` in a sentence) stay below `min_chars` and are left
/// alone.
fn json_spans(content: &str, min_chars: usize) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let bytes = content.as_bytes();
    let mut index = 0usize;
    let mut attempts = 0usize;
    while index < bytes.len() && attempts < MAX_JSON_SCAN_ATTEMPTS {
        // Stepping a byte at a time is safe because neither brace can appear
        // inside a multi-byte code point, so every candidate is a boundary.
        if bytes[index] != b'[' && bytes[index] != b'{' {
            index += 1;
            continue;
        }
        attempts += 1;
        match value_len(&content[index..]) {
            Some(len) if chars_len(&content[index..index + len]) >= min_chars => {
                spans.push((index, index + len));
                index += len;
            }
            _ => index += 1,
        }
    }
    spans
}

/// Length in bytes of the JSON value `tail` opens with, or `None` if it does not
/// open with one.
fn value_len(tail: &str) -> Option<usize> {
    let mut stream = serde_json::Deserializer::from_str(tail).into_iter::<Value>();
    match stream.next() {
        Some(Ok(_)) => Some(stream.byte_offset()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// plain sections
// ---------------------------------------------------------------------------

/// What a line, read in the context of its neighbours, belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LineKind {
    Diff,
    Build,
    Search,
    Tabular,
    Listing,
    Config,
}

/// Contiguous runs of at least [`MIN_SECTION_LINES`] lines sharing one kind,
/// as byte ranges into `content` — everything between them is prose and is
/// never touched.
fn plain_sections(content: &str, opts: &EngineOptions) -> Vec<(usize, usize, LineKind)> {
    let lines = split_lines_keepends(content);
    let kinds = line_kinds(&lines, opts);
    let mut sections: Vec<(usize, usize, LineKind)> = Vec::new();
    let mut offset = 0usize;
    let mut run_start = 0usize;
    let mut run_end = 0usize;
    let mut run_kind: Option<LineKind> = None;

    for (line, kind) in lines.iter().zip(kinds.iter()) {
        if kind.is_some() && *kind == run_kind {
            run_end = offset + line.len();
        } else {
            flush(content, run_start, run_end, run_kind, &mut sections);
            run_kind = *kind;
            run_start = offset;
            run_end = offset + line.len();
        }
        offset += line.len();
    }
    flush(content, run_start, run_end, run_kind, &mut sections);
    sections
}

fn flush(
    content: &str,
    start: usize,
    end: usize,
    kind: Option<LineKind>,
    sections: &mut Vec<(usize, usize, LineKind)>,
) {
    let Some(kind) = kind else { return };
    if end <= start || line_count(&content[start..end]) < MIN_SECTION_LINES {
        return;
    }
    // Trailing blank lines belong to the prose that follows, not to the section:
    // leaving them inside would hand the compressor a span whose end it is free
    // to move.
    sections.push((start, trim_trailing_whitespace(content, start, end), kind));
}

fn line_count(value: &str) -> usize {
    split_lines(value).len()
}

fn trim_trailing_whitespace(content: &str, start: usize, mut end: usize) -> usize {
    while end > start {
        let last = content[start..end]
            .chars()
            .next_back()
            .expect("the range is non-empty");
        if !last.is_whitespace() {
            break;
        }
        end -= last.len_utf8();
    }
    end
}

/// Per-line kinds, with diff-region awareness and aligned-run promotion.
///
/// Logs and search results classify line by line — every line of the run matches
/// on its own. A unified diff cannot: its hunk bodies are arbitrary text and
/// match nothing, so a diff embedded in prose used to break into 4-line header
/// fragments, never form a section, and fall through to whole-message
/// `crush_diff`, which dropped the surrounding prose. A diff is tracked as a
/// *region* instead: it opens on a header line and absorbs hunk-body and
/// extended-header lines until something unrelated ends it.
fn line_kinds(lines: &[&str], opts: &EngineOptions) -> Vec<Option<LineKind>> {
    let mut kinds: Vec<Option<LineKind>> = Vec::with_capacity(lines.len());
    let mut in_diff = false;
    for line in lines {
        let kind = line_kind(line, opts);
        if kind == Some(LineKind::Diff) {
            in_diff = true;
            kinds.push(Some(LineKind::Diff));
        } else if in_diff && continues_diff(line) {
            kinds.push(Some(LineKind::Diff));
        } else {
            in_diff = false;
            kinds.push(kind);
        }
    }
    mark_aligned_runs(lines, &mut kinds);
    if opts.enable_config_sections {
        mark_config_regions(lines, &mut kinds);
    }
    kinds
}

fn line_kind(line: &str, opts: &EngineOptions) -> Option<LineKind> {
    // Ahead of the `[` guard below, not after it. That guard is there to keep
    // the opening bracket of a JSON array from classifying as anything, and it
    // returns on every line starting with `[` — an INI section header
    // included, which is the one such line that is not JSON at all. Tested
    // first, the whole of a `.cfg` was invisible to the segmenter: every
    // `[section]` came back unclassified, the runs between them never reached
    // the assignment count that stands in for a header, and a 12 KB file
    // compressed by nothing. The pattern is narrow enough to tell the two
    // apart: `[1, 2]` does not start with an identifier.
    if opts.enable_config_sections && CONFIG_SECTION.is_match(line.trim_end()) {
        return Some(LineKind::Config);
    }
    let lstripped = line.trim_start();
    if lstripped.starts_with('{') || lstripped.starts_with('[') {
        return None;
    }
    if line.starts_with("diff --git ")
        || line.starts_with("@@ ")
        || line.starts_with("--- a/")
        || line.starts_with("+++ b/")
    {
        return Some(LineKind::Diff);
    }
    // Both tests live in the detector: a line must not be able to read as a log
    // to `detect_content` and as a grep hit to the segmenter.
    if looks_like_log_line(line) {
        return Some(LineKind::Build);
    }
    if looks_like_search_line(line) {
        return Some(LineKind::Search);
    }
    // After the log and grep tests and before the shape ones. A log line that
    // happens to read `level: info` is a log, and a `[section]` header matches
    // neither of the shape patterns below, so the order costs nothing and the
    // precedence is the one that keeps whole-payload logs whole.
    if opts.enable_config_sections && CONFIG_ASSIGN.is_match(line) {
        return Some(LineKind::Config);
    }
    // These two patterns end in `$`, which anchors at the very end of the
    // haystack — a trailing newline would be part of the match and defeat them,
    // so the terminator has to come off first.
    let body = line.strip_suffix('\n').unwrap_or(line);
    if DELIMITED_LINE.is_match(body) {
        return Some(LineKind::Tabular);
    }
    if LISTING_LINE.is_match(body) {
        return Some(LineKind::Listing);
    }
    if OFFSET_ROW.is_match(line) {
        return Some(LineKind::Listing);
    }
    None
}

/// Extended git diff header lines, which sit between `diff --git` and the hunk.
const DIFF_META: [&str; 13] = [
    "index ",
    "old mode ",
    "new mode ",
    "new file mode ",
    "deleted file mode ",
    "copy from ",
    "copy to ",
    "rename from ",
    "rename to ",
    "similarity index ",
    "dissimilarity index ",
    "Binary files ",
    "GIT binary patch",
];

/// Whether `line` extends an already-open diff region: a hunk-body line
/// (context, addition, deletion, no-newline marker) or an extended header.
///
/// A blank line matches nothing here, which ends the region — and a diff is
/// conventionally separated from the prose after it by exactly that, so the
/// trailing prose stays outside.
fn continues_diff(line: &str) -> bool {
    matches!(line.as_bytes().first(), Some(b' ' | b'+' | b'-' | b'\\'))
        || DIFF_META.iter().any(|meta| line.starts_with(meta))
}

/// Promote runs of aligned columnar rows (`ls -l`, `ps aux`) to a listing.
///
/// [`line_kind`] classifies one line at a time, and an aligned row is only
/// recognisable *relative to its neighbours* — which is why a terminal pane
/// (shell prompts, an `ls -la` block, a CSV result) used to form no listing
/// section at all: the rows contain spaces, so the bare-name pattern missed
/// them, the whole pane was then sampled as one listing, its head budget went on
/// shell startup noise and the CSV result was buried in a ref.
///
/// Only lines matching the run's *dominant* field count are promoted, so the
/// surrounding prompts and the `total 6132` header stay outside the section and
/// survive verbatim.
fn mark_aligned_runs(lines: &[&str], kinds: &mut [Option<LineKind>]) {
    for (start, end) in unclassified_runs(kinds) {
        let counts: Vec<usize> = lines[start..end]
            .iter()
            .map(|line| line.split_whitespace().count())
            .collect();
        let eligible: Vec<usize> = counts
            .iter()
            .copied()
            .filter(|count| *count >= ALIGNED_MIN_FIELDS)
            .collect();
        if eligible.len() < MIN_SECTION_LINES {
            continue;
        }
        let dominant = dominant_count(&eligible);
        let mut block_start: Option<usize> = None;
        // `offset` indexes the per-run counts and `index` the lines themselves;
        // the extra step past the end is what closes a block that runs to it
        #[allow(clippy::needless_range_loop)]
        for offset in 0..=(end - start) {
            let index = start + offset;
            let matches = offset < end - start && counts[offset] == dominant;
            if matches {
                block_start.get_or_insert(index);
            } else if let Some(block) = block_start.take() {
                promote(lines, kinds, block, index);
            }
        }
    }
}

/// The most frequent field count, ties going to the narrower row.
fn dominant_count(eligible: &[usize]) -> usize {
    let mut distinct: Vec<usize> = eligible.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    distinct
        .into_iter()
        .max_by_key(|candidate| eligible.iter().filter(|c| *c == candidate).count())
        .expect("the caller checked the run is non-empty")
}

/// Mark `[start, end)` as a listing when it is long enough and names things.
fn promote(lines: &[&str], kinds: &mut [Option<LineKind>], start: usize, end: usize) {
    let block: Vec<&str> = lines[start..end]
        .iter()
        .map(|line| line.trim_end_matches('\n'))
        .collect();
    if block.len() < MIN_SECTION_LINES {
        return;
    }
    let distinct: HashSet<&str> = block.iter().copied().collect();
    if (distinct.len() as f64) / (block.len() as f64) < LISTING_UNIQUE_RATIO {
        // repeated lines belong to duplicate folding, not to sampling
        return;
    }
    // A listing's rows NAME things: the last field is a path or a filename.
    // Field count alone is not enough — uniformly generated source (`const
    // value_0: number = input + 0;`) is just as regular, and promoting that
    // hands a code file to the entry sampler. The detector avoids this by
    // checking listings last, after the code patterns; the segmenter has no such
    // ordering, so it needs the stronger signal.
    let named = block
        .iter()
        .filter(|line| NAMED_TAIL.is_match(line))
        .count();
    if (named as f64) / (block.len() as f64) < LISTING_UNIQUE_RATIO {
        return;
    }
    for kind in &mut kinds[start..end] {
        *kind = Some(LineKind::Listing);
    }
}

/// Grow each run of configuration anchors over the commentary around it.
///
/// A `.cfg` is mostly comments. `[mailman]` and `site_owner: x` classify on
/// their own; the twenty lines of explanation between them classify as nothing,
/// and without absorbing those a 278-line file breaks into two-line runs, never
/// reaches [`MIN_SECTION_LINES`], and forms no section at all. This is the same
/// shape as the diff-region rule above and exists for the same reason.
///
/// Only lines no other kind claimed are absorbed, so a log that happens to
/// carry `key: value` rows stays a log, and the backward walk stops at the
/// first line belonging to something else — which is what keeps the region from
/// eating the tail of the file in front of it.
fn mark_config_regions(lines: &[&str], kinds: &mut [Option<LineKind>]) {
    let mut index = 0usize;
    while index < lines.len() {
        if kinds[index] != Some(LineKind::Config) {
            index += 1;
            continue;
        }
        let mut cursor = index;
        let mut last_anchor = index;
        let mut headers = 0usize;
        let mut assignments = 0usize;
        while cursor < lines.len() {
            match kinds[cursor] {
                Some(LineKind::Config) => {
                    if CONFIG_SECTION.is_match(lines[cursor].trim_end()) {
                        headers += 1;
                    } else {
                        assignments += 1;
                    }
                    last_anchor = cursor;
                    cursor += 1;
                }
                None if is_commentary(lines[cursor]) => cursor += 1,
                _ => break,
            }
        }
        // The commentary after the last setting is claimed too, symmetrically
        // with the backward walk below. Leaving it out looks tidier — a comment
        // at the end reads just as well as the preamble of whatever follows —
        // and it is wrong, because a file that is *nothing but* configuration
        // then has its closing comments outside the only section, which is
        // exactly the test `has_mixed_content` uses to decide a body is
        // heterogeneous. Sixty recorded vectors moved: whole `.cfg` payloads
        // stopped going to the config compressor and were segmented instead,
        // one of them 173 characters worse for it.
        let end = cursor.saturating_sub(1);
        let mut start = index;
        while start > 0 && kinds[start - 1].is_none() && is_commentary(lines[start - 1]) {
            start -= 1;
        }
        if headers > 0 || assignments >= CONFIG_MIN_ASSIGNMENTS {
            for kind in &mut kinds[start..=end] {
                *kind = Some(LineKind::Config);
            }
        } else {
            // Not configuration after all. Give the anchors back, so a couple of
            // `key = value` lines in a source file cannot form a five-line run
            // on their own.
            for kind in &mut kinds[index..=last_anchor] {
                if *kind == Some(LineKind::Config) {
                    *kind = None;
                }
            }
        }
        index = cursor.max(index + 1);
    }
}

/// A line that carries no setting but belongs to the file holding them.
fn is_commentary(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';')
}

fn unclassified_runs(kinds: &[Option<LineKind>]) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    for (index, kind) in kinds.iter().enumerate() {
        match (kind, start) {
            (None, None) => start = Some(index),
            (Some(_), Some(open)) => {
                runs.push((open, index));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(open) = start {
        runs.push((open, kinds.len()));
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> EngineOptions {
        EngineOptions::default()
    }

    fn ctx() -> Context<'static> {
        Context::for_role("user")
    }

    fn refs() -> RefCollector {
        EngineOptions::default().collector()
    }

    /// An INI body of `count` settings, with the commentary a real one carries.
    fn config_block(count: usize) -> String {
        let mut out = String::from("[mailman]\n");
        for i in 0..count {
            out.push_str(&format!(
                "# Explains what setting {i} is for, at the length these files\n\
                 # actually run to, which is several lines of prose per key.\n\
                 setting_{i}: value-{i}\n\n"
            ));
        }
        out
    }

    /// A source file: top-level constants and a few functions.
    fn python_block() -> String {
        let mut out = String::from("import os\nimport sys\n\nDOMAIN = \"local\"\n\n");
        for i in 0..6 {
            out.push_str(&format!(
                "def handler_{i}(request):\n    \"\"\"Handle request {i}.\"\"\"\n\
                 {}    return None\n\n",
                "    value = request.get('x')\n".repeat(8)
            ));
        }
        out
    }

    /// A tool result holding a source file and then a configuration file, with
    /// nothing between them saying where one ends.
    ///
    /// Measured on the payload this came from — `cat eval.py; ls …; cat
    /// mailman.cfg` in one shell call, 19,141 characters — the whole message
    /// compressed by **0.0%**. Each half alone compressed: the Python by 55%,
    /// the configuration by 74%. Neither reading shipped, because the segmenter
    /// had no kind for either half and whole-message routing handed all 19 KB
    /// to the Python compressor, which declined.
    #[test]
    fn a_config_file_after_a_source_file_is_found_and_compressed() {
        let content = format!("{}{}", python_block(), config_block(30));
        let mut refs = refs();
        let (out, kinds) =
            crush_mixed(&content, &opts(), &ctx(), &mut refs).expect("the configuration half");
        assert!(kinds.contains(&Kind::Config), "kinds were {kinds:?}");
        assert!(
            chars_len(&out) * 2 < chars_len(&content),
            "{} -> {}, expected the configuration half to go",
            chars_len(&content),
            chars_len(&out)
        );
        // The source half is outside every recognised span, so it is still
        // there byte for byte. That is the segmenter's contract, not an
        // oversight: whole-message routing is the reading that compresses code.
        assert!(out.contains("def handler_0(request):"));
    }

    /// The section header is the one `[`-leading line that is not JSON, and it
    /// is what a configuration region qualifies on.
    #[test]
    fn a_section_header_is_not_mistaken_for_a_json_array() {
        let lines = split_lines_keepends("[mailman]\n");
        assert_eq!(line_kinds(&lines, &opts())[0], Some(LineKind::Config));
        let lines = split_lines_keepends("[1, 2, 3]\n");
        assert_eq!(line_kinds(&lines, &opts())[0], None);
    }

    /// Without a header a region needs a crowd, because the top of a source
    /// file looks like this and the code compressor reads it far better.
    #[test]
    fn a_few_assignments_in_a_source_file_are_not_configuration() {
        let content = format!(
            "{}\n{}",
            "retries = 3\ntimeout = 30\nverbose = False\nprefix = '/tmp'\nlimit = 10\n",
            python_block()
        );
        let lines = split_lines_keepends(&content);
        let kinds = line_kinds(&lines, &opts());
        assert!(
            kinds.iter().all(|kind| *kind != Some(LineKind::Config)),
            "five assignments formed a configuration section"
        );
    }

    /// The kill switch returns the pre-feature answer.
    #[test]
    fn config_sections_can_be_switched_off() {
        let content = format!("{}{}", python_block(), config_block(30));
        let off = EngineOptions {
            enable_config_sections: false,
            ..EngineOptions::default()
        };
        assert!(crush_mixed(&content, &off, &ctx(), &mut refs()).is_none());
    }

    /// 60 timestamped lines, which the log compressor folds into a template.
    fn log_block() -> String {
        (0..60)
            .map(|i| {
                format!(
                    "2026-01-01T09:{:02}:00 INFO worker handled request {i}\n",
                    i % 60
                )
            })
            .collect()
    }

    /// An `ls -la` body: 9 whitespace-separated fields, last one a filename.
    fn ls_rows(count: usize) -> String {
        (0..count)
            .map(|i| {
                format!(
                    "-rw-r--r--  1 vlad vlad   {:5} Jan  1 09:00 report_{i:03}.csv\n",
                    1024 + i
                )
            })
            .collect()
    }

    fn json_blob(items: usize) -> String {
        let body: Vec<String> = (0..items)
            .map(|i| {
                format!(
                    "{{\"id\": {i}, \"name\": \"service-alpha-{i:03}\", \"state\": \"running\"}}"
                )
            })
            .collect();
        format!("{{\"items\": [{}]}}", body.join(", "))
    }

    #[test]
    fn prose_around_a_compressed_span_survives_byte_for_byte() {
        let head = "Here is what the worker did overnight.\n\n";
        let tail = "\n\nAll 60 requests completed without an error.\n";
        let content = format!("{head}{}{tail}", log_block().trim_end_matches('\n'));
        let (out, _) = crush_mixed(&content, &opts(), &ctx(), &mut refs()).expect("a log section");
        assert!(
            out.starts_with(head),
            "leading prose was rewritten: {out:?}"
        );
        assert!(out.ends_with(tail), "trailing prose was rewritten: {out:?}");
    }

    #[test]
    fn a_fenced_python_block_is_compressed_and_its_fence_is_left_intact() {
        let mut body = String::from("def collect(rows):\n    total = 0\n");
        for i in 0..40 {
            body.push_str(&format!("    total += rows[{i}] * {i}\n"));
        }
        body.push_str("    return total\n");
        let content = format!("The helper is this:\n\n```python\n{body}```\n\nIt sums the rows.\n");
        let result = crush_mixed(&content, &opts(), &ctx(), &mut refs());

        #[cfg(feature = "code-aware")]
        {
            let (out, kinds) = result.expect("a python fence is routed to the code compressor");
            assert_eq!(kinds, vec![Kind::SourceCode]);
            assert!(
                out.contains("```python\n"),
                "the opening fence moved: {out:?}"
            );
            assert!(out.contains("\n```\n"), "the closing fence moved: {out:?}");
            assert!(out.starts_with("The helper is this:\n\n"));
            assert!(out.ends_with("\n\nIt sums the rows.\n"));
        }
        #[cfg(not(feature = "code-aware"))]
        {
            // no grammar pack: the block declines and the body is untouched
            assert!(result.is_none(), "without a grammar this must fail open");
        }
    }

    #[test]
    fn a_json_blob_followed_by_one_more_sentence_is_still_found() {
        // the case the old prefaced-JSON rule could not reach: it required the
        // blob to be the entire trailing suffix
        let content = format!(
            "The API answered:\n{}\nEverything above is the raw response.\n",
            json_blob(30)
        );
        let spans = json_spans(&content, opts().min_nested_chars);
        assert_eq!(spans.len(), 1, "expected exactly one blob: {spans:?}");
        let (start, end) = spans[0];
        assert!(content[start..end].starts_with("{\"items\""));
        assert!(content[start..end].ends_with("]}"));
        assert!(has_mixed_content(&content, &opts()));
    }

    #[test]
    fn a_json_span_is_compressed_in_place_with_its_prose_intact() {
        let head = "The API answered:\n";
        let tail = "\nEverything above is the raw response.\n";
        let content = format!("{head}{}{tail}", json_blob(30));
        let (out, kinds) = crush_mixed(&content, &opts(), &ctx(), &mut refs()).expect("a blob");
        assert_eq!(kinds, vec![Kind::Json]);
        assert!(
            out.starts_with(head),
            "leading prose was rewritten: {out:?}"
        );
        assert!(out.ends_with(tail), "trailing prose was rewritten: {out:?}");
    }

    #[test]
    fn two_json_blobs_in_one_message_are_both_found() {
        let content = format!(
            "First call:\n{}\nSecond call:\n{}\nBoth succeeded.\n",
            json_blob(30),
            json_blob(25)
        );
        let spans = json_spans(&content, opts().min_nested_chars);
        assert_eq!(spans.len(), 2, "both blobs must be segmented: {spans:?}");
        assert!(spans[0].1 <= spans[1].0, "spans must not overlap");
    }

    #[test]
    fn a_small_inline_object_stays_below_the_threshold() {
        let content = "Retry policy is {\"retries\": 3, \"backoff\": \"exponential\"} for now.";
        assert!(json_spans(content, opts().min_nested_chars).is_empty());
        assert!(!has_mixed_content(content, &opts()));
    }

    #[test]
    fn the_json_scan_is_bounded_on_brace_heavy_prose() {
        let content = "{ not json at all ".repeat(1000);
        // the bound is on decode attempts, so this returns rather than grinding
        assert!(json_spans(&content, 512).is_empty());
    }

    #[test]
    fn a_body_that_is_nothing_but_one_blob_is_not_mixed() {
        let blob = json_blob(30);
        assert!(
            !has_mixed_content(&blob, &opts()),
            "the whole-message route owns this"
        );
        assert!(!has_mixed_content(&format!("\n{blob}\n  "), &opts()));
    }

    #[test]
    fn a_diff_embedded_in_prose_forms_one_region_and_the_prose_survives() {
        let content = "\
I changed the retry handling:

diff --git a/app/worker.py b/app/worker.py
index 1234567..89abcde 100644
--- a/app/worker.py
+++ b/app/worker.py
@@ -10,7 +10,7 @@ def run(job):
     attempts = 0
-    while attempts < 3:
+    while attempts < MAX_ATTEMPTS:
         attempts += 1
     return job.result

Let me know if the bound should be configurable.
";
        let sections = plain_sections(content, &opts());
        assert_eq!(
            sections.len(),
            1,
            "the diff must be one region: {sections:?}"
        );
        let (start, end, kind) = sections[0];
        assert_eq!(kind, LineKind::Diff);
        assert!(content[..start].starts_with("I changed the retry handling:"));
        assert!(content[start..end].starts_with("diff --git "));
        assert!(
            content[start..end].ends_with("return job.result"),
            "a blank line ends the region: {:?}",
            &content[start..end]
        );
        assert!(content[end..].contains("Let me know if the bound"));
    }

    #[test]
    fn an_ls_block_inside_a_terminal_pane_is_promoted_to_a_listing() {
        let content = format!(
            "$ cd /srv/app\n$ ls -la\ntotal 6132\n{}$ echo done\ndone\n",
            ls_rows(60)
        );
        let sections = plain_sections(&content, &opts());
        assert_eq!(sections.len(), 1, "one listing, not one pane: {sections:?}");
        let (start, end, kind) = sections[0];
        assert_eq!(kind, LineKind::Listing);
        assert!(
            content[..start].ends_with("total 6132\n"),
            "the header has the wrong field count and stays outside"
        );
        assert!(content[end..].starts_with("\n$ echo done"));

        let (out, kinds) = crush_mixed(&content, &opts(), &ctx(), &mut refs()).expect("sampled");
        assert_eq!(kinds, vec![Kind::Listing]);
        for prompt in [
            "$ cd /srv/app\n",
            "$ ls -la\n",
            "total 6132\n",
            "$ echo done\n",
        ] {
            assert!(out.contains(prompt), "the shell pane lost {prompt:?}");
        }
    }

    #[test]
    fn uniformly_generated_source_is_not_promoted_to_a_listing() {
        let content: String = (0..30)
            .map(|i| format!("const value_{i}: number = input + {i};\n"))
            .collect();
        assert!(
            plain_sections(&content, &opts()).is_empty(),
            "field count alone must not hand a code file to the entry sampler"
        );
    }

    #[test]
    fn a_csv_template_carrying_placeholder_cells_is_never_sampled() {
        let rows: String = (0..40)
            .map(|i| format!("<period_{i}>,<start_date>,<end_date>,{{count}}\n"))
            .collect();
        assert!(!worth_sampling(&rows, &opts()));
        let content = format!("Fill in one row per reporting period:\n\n{rows}\nThen save it.\n");
        let sections = plain_sections(&content, &opts());
        assert_eq!(sections.len(), 1, "it is still recognised as a table");
        assert_eq!(sections[0].2, LineKind::Tabular);
        assert!(
            crush_mixed(&content, &opts(), &ctx(), &mut refs()).is_none(),
            "a specification is emitted verbatim; the prize for sampling it was 245 chars"
        );
    }

    #[test]
    fn a_short_table_is_not_worth_sampling() {
        let rows: String = (0..15).map(|i| format!("row-{i},alpha,{i},ok\n")).collect();
        assert!(
            !worth_sampling(&rows, &opts()),
            "15 rows against a 10-row budget saves 5 lines"
        );
        let mut wide = rows.clone();
        wide.push_str(
            &(15..40)
                .map(|i| format!("row-{i},alpha,{i},ok\n"))
                .collect::<String>(),
        );
        assert!(worth_sampling(&wide, &opts()), "40 rows clears the floor");
    }

    #[test]
    fn a_rejected_span_rolls_back_its_own_fragments() {
        let mut refs = refs();
        refs.stash("a fragment an accepted span is using");
        let kept = refs.len();
        let mut rewrite = Rewrite::default();

        let mark = refs.mark();
        refs.stash("a fragment the rejected span stashed");
        rewrite.record(
            0..4,
            "body",
            Some("a compressed form that is longer".to_string()),
            Some(Kind::Listing),
            &mut refs,
            mark,
        );
        assert!(
            rewrite.is_empty(),
            "a longer candidate is not a replacement"
        );
        assert_eq!(refs.len(), kept, "the rejected span left a fragment behind");
        assert!(rewrite.kinds().is_empty(), "a rejected span claims no kind");
    }

    #[test]
    fn no_handle_outlives_the_marker_that_names_it() {
        let content = format!(
            "Deploy log:\n\n{}\n\nAnd the files it wrote:\n\n{}\nDone.\n",
            log_block().trim_end_matches('\n'),
            ls_rows(60)
        );
        let mut refs = refs();
        let (out, _) = crush_mixed(&content, &opts(), &ctx(), &mut refs).expect("a section shrank");
        for (id, _) in refs.pairs() {
            assert!(
                out.contains(&id.to_string()),
                "the store holds {id} but no marker names it"
            );
        }
    }

    #[test]
    fn the_returned_kinds_are_the_unfolded_real_types() {
        let content = format!("Overnight run:\n\n{}", log_block());
        let (_, kinds) = crush_mixed(&content, &opts(), &ctx(), &mut refs()).expect("a log");
        // `build` is the detector's word; analytics says `log`, the same word a
        // whole message of the same content reports.
        assert_eq!(
            kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
            vec!["log"]
        );
        assert!(!kinds.contains(&Kind::Mixed), "mixed never reports itself");
    }

    #[test]
    fn a_body_with_nothing_structured_returns_none() {
        let content = "Just a paragraph of ordinary prose, with a comma, and a\n\
                       second line that says nothing a compressor could shorten.\n";
        assert!(!has_mixed_content(content, &opts()));
        assert!(crush_mixed(content, &opts(), &ctx(), &mut refs()).is_none());
    }

    #[test]
    fn the_output_is_never_longer_than_the_input() {
        for content in [
            format!("Log:\n\n{}", log_block()),
            format!("$ ls -la\ntotal 4\n{}$ exit\n", ls_rows(60)),
            "nothing structured here at all\n".to_string(),
        ] {
            if let Some((out, _)) = crush_mixed(&content, &opts(), &ctx(), &mut refs()) {
                assert!(
                    chars_len(&out) < chars_len(&content),
                    "a replacement that does not shrink is not a replacement"
                );
            }
        }
    }

    #[test]
    fn rewriting_by_offset_preserves_blank_lines_and_the_trailing_newline() {
        let head = "Report\n\n\nlines follow:\n\n";
        let tail = "\n\n\nend of report\n";
        let content = format!("{head}{}{tail}", log_block().trim_end_matches('\n'));
        let (out, _) = crush_mixed(&content, &opts(), &ctx(), &mut refs()).expect("a log section");
        assert!(out.starts_with(head), "blank lines before the span moved");
        assert!(
            out.ends_with(tail),
            "the blank run and the trailing newline after the span moved: {:?}",
            &out[out.len().saturating_sub(24)..]
        );
    }

    #[test]
    fn a_run_shorter_than_five_lines_is_not_a_section() {
        let content = "intro\n\
                       2026-01-01T09:00:00 INFO one\n\
                       2026-01-01T09:00:01 INFO two\n\
                       2026-01-01T09:00:02 INFO three\n\
                       2026-01-01T09:00:03 INFO four\n\
                       outro\n";
        assert!(
            plain_sections(content, &opts()).is_empty(),
            "four lines is not a run"
        );
    }

    #[test]
    fn a_section_span_ends_at_its_last_non_blank_byte() {
        let content = format!("intro\n{}\n\n\noutro\n", log_block().trim_end_matches('\n'));
        let (_, end, _) = plain_sections(&content, &opts())[0];
        assert_eq!(&content[end..end + 4], "\n\n\no");
    }

    #[test]
    fn a_handle_is_only_minted_for_a_span_that_shrank() {
        // the short-table gate declines, so nothing is stashed at all
        let rows: String = (0..15).map(|i| format!("row-{i},alpha,{i},ok\n")).collect();
        let mut refs = refs();
        assert!(crush_mixed(&format!("Table:\n\n{rows}"), &opts(), &ctx(), &mut refs).is_none());
        assert!(
            refs.is_empty(),
            "a declined body must leave the store empty"
        );
    }
}
