//! Array-aware JSON compression.
//!
//! API responses and tool results are dominated by long homogeneous arrays and
//! by oversized string fields. The crusher walks the parsed document depth-first
//! and applies exactly two lossy transforms: a long array is reduced to position
//! anchors plus the items that carry information (errors, non-2xx statuses,
//! numeric outliers, shape changes) behind one omission marker, and an oversized
//! string is clipped — but only after it has been offered to the compressor for
//! whatever type it actually holds, because a long string value is far more often
//! a wrapped payload than prose.
//!
//! [`Mode::Lossless`] takes a different path entirely: nothing is dropped or
//! clipped, and the only transform is key factoring of homogeneous object
//! collections into a columnar envelope that is reversible from its own bytes.
//!
//! # Numbers are not rewritten
//!
//! A JSON round trip through a parser that normalizes numbers into `f64`
//! rewrites numeric literals: `1e5` comes back as `100000.0` and
//! `0.1000000000000000005` as `0.1`. That is silent data loss dressed up as
//! reformatting — and worse, it is invisible to the "every shrink is accounted
//! for" guard, which cannot tell a rewritten number from harmless re-indentation.
//! `serde_json`'s `arbitrary_precision` feature keeps every number as its source
//! digits instead, so precision, magnitude and trailing zeros all survive; the
//! only normalization left is the exponent's spelling (`1e5` and `1E5` both come
//! back as `1e+5`), which changes no value. `preserve_order` likewise keeps
//! object key order, so a re-serialized object is the one that went in.

use std::collections::{HashMap, HashSet};

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{Map, Value};

use crate::adaptive::adaptive_k;

use crate::options::EngineOptions;
use crate::preamble::split_harness_preamble;
use crate::refs::RefCollector;
use crate::text::{chars_len, is_blank, split_lines};
use crate::types::{ContentType, Context, Mode};

/// Substrings that mark an item as carrying a failure. Matched against the
/// item's canonical serialization, so a token buried in a nested value counts.
const ERROR_TOKENS: [&str; 12] = [
    "error",
    "fail",
    "failed",
    "failure",
    "exception",
    "fatal",
    "critical",
    "denied",
    "timeout",
    "refused",
    "panic",
    "traceback",
];

/// Keys whose integer value is an HTTP-shaped status.
const STATUS_KEYS: [&str; 4] = ["status", "status_code", "statuscode", "code"];

/// Fewer records than this and there are no repeated key names worth factoring.
const MIN_FACTOR_ITEMS: usize = 2;

const LITERAL_KEY: &str = "__literal__";
const KEYS_KEY: &str = "__keys__";
const COLS_KEY: &str = "__cols__";
const ROWS_KEY: &str = "__rows__";

/// Any key a factored envelope can carry. An object holding one of these is
/// never factored, so a consumer can never mistake an envelope for real data.
const ENVELOPE_KEYS: [&str; 4] = [KEYS_KEY, COLS_KEY, ROWS_KEY, LITERAL_KEY];

/// Compress a JSON document.
///
/// `None` means the document was left alone — it did not parse, or nothing the
/// crusher did made it shorter.
pub fn crush_json(
    content: &str,
    opts: &EngineOptions,
    ctx: &Context<'_>,
    refs: &mut RefCollector,
) -> Option<String> {
    // The harness frame comes off first and goes back on verbatim, the way the
    // log, code, diff and listing paths already do it. Without that a tool
    // result is a few lines of `Chunk ID:` followed by a document, `parse`
    // fails on the first byte, and the compressor declines a payload it could
    // have taken — which is what happened to every JSON result from every
    // harness that stamps a frame.
    let (preamble, body) = split_harness_preamble(content);
    let parsed = parse(body)?;
    let crushed = if opts.mode == Mode::Lossless {
        crush_lossless(parsed)
    } else {
        let sampled = crush_value(parsed, opts, ctx, refs, 0);
        if opts.enable_lossy_key_factoring {
            // Key factoring is reversible from its own envelope, so it is safe
            // in any mode. Confining it to the lossless path leaves map-shaped
            // JSON compressing at roughly nothing in the modes deployments
            // actually run; running the same pass over the sampled tree recovers
            // a measured 40-46% on API indexes and config dumps for no
            // information loss.
            //
            // Scoped to objects, which is where the gap is. Arrays already
            // have an answer in a lossy mode — the sampler — and factoring a
            // short one as well would rewrite payloads the sampler
            // deliberately leaves whole, churning prompt-cache bytes for a
            // transform nobody asked for. Objects have no such answer:
            // sampling never drops keys, so a map of records was passing
            // through at roughly nothing.
            factor_maps(sampled)
        } else {
            sampled
        }
    };
    let out = serde_json::to_string(&crushed).ok()?;
    let out = format!("{preamble}{out}");
    (chars_len(&out) < chars_len(content)).then_some(out)
}

/// Parse `content` as one document, or as NDJSON read as a single array.
fn parse(content: &str) -> Option<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(content) {
        return Some(value);
    }
    let mut items: Vec<Value> = Vec::new();
    for line in split_lines(content) {
        if is_blank(line) {
            continue;
        }
        items.push(serde_json::from_str::<Value>(line).ok()?);
    }
    // A single line that parsed here would already have parsed above, so one
    // item means the whole-document attempt failed on something else.
    (items.len() > 1).then_some(Value::Array(items))
}

/// Compact serialization, used wherever only a length matters.
fn dumps(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// lossy path
// ---------------------------------------------------------------------------

fn crush_value(
    value: Value,
    opts: &EngineOptions,
    ctx: &Context<'_>,
    refs: &mut RefCollector,
    depth: usize,
) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (key, item) in map {
                out.insert(key, crush_value(item, opts, ctx, refs, depth));
            }
            Value::Object(out)
        }
        Value::Array(items) => crush_array(items, opts, ctx, refs, depth),
        Value::String(text) => Value::String(crush_string(text, opts, ctx, refs, depth)),
        other => other,
    }
}

/// Compress an oversized string value.
///
/// A long string value is usually not prose — it is a wrapped payload (command
/// output, a source file, double-encoded JSON). Routing it to the compressor for
/// its own type keeps that type's guarantees: the log compressor's error lines,
/// the code compressor's parse validation, array sampling instead of character
/// truncation. Only when nothing claims it does the plain clip apply.
fn crush_string(
    text: String,
    opts: &EngineOptions,
    ctx: &Context<'_>,
    refs: &mut RefCollector,
    depth: usize,
) -> String {
    if chars_len(&text) <= opts.max_field_chars {
        return text;
    }
    // The clip threshold, not a separate size floor, is what gates routing:
    // every string about to be damaged gets the offer first.
    let (routed, structured) = route_nested(&text, opts, ctx, refs, depth);
    if let Some(out) = routed {
        return out;
    }
    if structured {
        // A structured payload whose own compressor could not shrink it — a diff
        // of few files that is nearly all changed lines, say. Clipping would
        // truncate mid-record and hand the model a corrupt patch, so it is kept
        // whole: an oversized valid payload beats a small invalid one.
        return text;
    }
    clip(&text, opts, refs)
}

/// Types whose bytes carry structure a mid-string clip would corrupt.
///
/// HTML is deliberately absent: it has no compressor, and an explicitly marked
/// clip is its long-standing behaviour.
fn is_structured(kind: ContentType) -> bool {
    matches!(
        kind,
        ContentType::JsonArray
            | ContentType::Config
            | ContentType::BuildOutput
            | ContentType::SearchResults
            | ContentType::GitDiff
            | ContentType::Tabular
            | ContentType::Listing
            | ContentType::SourceCode
    )
}

thread_local! {
    /// Whether a mixed-segmenter hop is already in flight on this thread.
    ///
    /// The segmenter compresses the JSON spans it finds by calling back into
    /// this module, so without a latch a field holding prose around JSON holding
    /// a field holding prose would bounce between the two forever. The reference
    /// expresses the same rule structurally — it injects the segmenter as a
    /// callable and the segmenter's own call omits it — which a fixed Rust
    /// signature cannot do. One segmenter hop per string either way.
    static SEGMENTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Clears the latch however the segmenter returns, including by unwinding: the
/// engine catches panics to fail open, and a stuck latch would silently disable
/// segmentation for every later request on the same worker thread.
struct SegmentLatch;

impl Drop for SegmentLatch {
    fn drop(&mut self) {
        SEGMENTING.with(|flag| flag.set(false));
    }
}

/// Compress `text` with the compressor for its detected type.
///
/// Returns `(compressed_or_none, is_structured)`. Results are used only when
/// shorter than the input, and refs stashed by a rejected attempt are rolled
/// back so the store never holds handles no marker points at.
fn route_nested(
    text: &str,
    opts: &EngineOptions,
    ctx: &Context<'_>,
    refs: &mut RefCollector,
    depth: usize,
) -> (Option<String>, bool) {
    if !opts.enable_nested_routing {
        return (None, false);
    }
    let detection = crate::detect::detect_content(text);
    let structured = is_structured(detection.content_type);
    if depth >= opts.max_nested_depth {
        // The depth budget bounds how much work is done, not what counts as
        // damage. Reporting `structured` here is what stops a diff or config
        // being clipped mid-record just because recursion stopped; the kill
        // switch above still returns the pre-feature answer.
        return (None, structured);
    }

    // Segmentation goes first, because a field is often prose *wrapping*
    // structure: a markdown note with a fenced code block detects as source
    // code, which cannot parse markdown. The segmenter only rewrites spans it
    // recognises and leaves every other byte alone, whereas a whole-field
    // compressor flat-samples across section boundaries — on a compound machine
    // dump that drops the section headers saying what the surviving rows are.
    if depth == 0 && !SEGMENTING.with(|flag| flag.get()) {
        let mark = refs.mark();
        let candidate = {
            SEGMENTING.with(|flag| flag.set(true));
            let _latch = SegmentLatch;
            crate::mixed::crush_mixed(text, opts, ctx, refs).map(|(out, _kinds)| out)
        };
        match candidate {
            Some(out) if chars_len(&out) < chars_len(text) => return (Some(out), structured),
            _ => refs.rollback(mark),
        }
    }

    let mark = refs.mark();
    match compress_as(text, &detection, opts, ctx, refs, depth) {
        Some(out) if chars_len(&out) < chars_len(text) => (Some(out), structured),
        _ => {
            refs.rollback(mark);
            fold_fallback(text, opts, ctx, refs, structured)
        }
    }
}

/// What the whole-message router does when the compressor it chose declines.
///
/// A compressor that claims a payload and then returns nothing leaves it
/// whole, even where something obvious was available; the top-level route has
/// had a fold fallback for that since the 14 payloads it was found on. A
/// nested field had none, so the same payload compressed or not depending on
/// whether a harness had wrapped it in a JSON envelope first — measured on a
/// codex sweep, where a 72,665-character YAML config crossed the wire at 0.00%
/// as the `output` field of a chunk envelope and at 99.13% as a bare file.
///
/// The guard is the top-level one, unchanged and for its own reasons: a
/// listing is distinct entries by construction and a real source file is
/// mostly distinct lines, so neither reaches this. What does reach it is
/// output that is the same handful of lines over and over.
fn fold_fallback(
    text: &str,
    opts: &EngineOptions,
    ctx: &Context<'_>,
    refs: &mut RefCollector,
    structured: bool,
) -> (Option<String>, bool) {
    // Structured only, which is narrower than the top-level rule and
    // deliberately so. This exists for the payload `crush_string` would
    // otherwise keep *whole* — `is_structured` returns it untouched rather
    // than clipping, because a mid-record cut corrupts it — and that is where
    // the 0.00% came from. An unstructured field already had an answer when
    // nothing claimed it: the marked clip, which is its long-standing
    // behaviour. Folding those as well is a real option and measured better
    // on some shapes and worse on others — a 47K markdown note went from
    // 99.18% clipped to 97.14% folded — so it is a separate decision with its
    // own measurement, not a side effect of this one.
    if !structured || !opts.enable_fold_fallback || !crate::pipeline::mostly_repeated_lines(text) {
        return (None, structured);
    }
    let mark = refs.mark();
    match crate::pipeline::arbitrate_text(text, opts, ctx, refs, crate::logs::Identity::Values) {
        Some(out) if chars_len(&out) < chars_len(text) => (Some(out), structured),
        _ => {
            refs.rollback(mark);
            (None, structured)
        }
    }
}

fn compress_as(
    text: &str,
    detection: &crate::types::Detection,
    opts: &EngineOptions,
    ctx: &Context<'_>,
    refs: &mut RefCollector,
    depth: usize,
) -> Option<String> {
    match detection.content_type {
        ContentType::JsonArray => {
            // Recurse through the crusher itself, so the inner payload gets
            // array sampling and its own nested strings routed one level further.
            let parsed = parse(text)?;
            Some(dumps(&crush_value(parsed, opts, ctx, refs, depth + 1)))
        }
        ContentType::BuildOutput => {
            crate::logs::crush_log(text, opts, ctx, refs, crate::logs::build_identity(opts))
        }
        ContentType::SearchResults => crate::search::crush_search(text, opts, ctx, refs),
        ContentType::GitDiff => crate::diff::crush_diff(text, opts, ctx, refs),
        ContentType::Tabular => crate::tabular::crush_tabular(text, opts, ctx, refs),
        ContentType::Listing => crate::listing::crush_listing(text, opts, ctx, refs),
        ContentType::SourceCode => {
            crate::code::crush_code(text, opts, ctx, refs, detection.meta.language)
        }
        // Gated exactly as the top-level router gates it, so the flag means the
        // same thing whether a config arrives as the message or inside a field
        // — and arbitrated the same way, which it was not.
        //
        // `crush_config` elides values one key at a time. That is the right
        // reading for a hand-written file and the wrong one for the generated
        // kind an agent cats, so the whole-message route has always weighed it
        // against a value-normalised fold and shipped the shorter. This arm
        // called the compressor bare, so a config that arrived inside a field
        // got only the reading the top-level router already knew was the weak
        // one. Measured on the same three fixtures, config compressor alone
        // against the arbitrated route: YAML 0.00% vs 99.13%, TOML 0.90% vs
        // 99.05%, INI 0.00% vs 99.00%.
        ContentType::Config if opts.enable_config_compressor => {
            let mark = refs.mark();
            let claimed = crate::configs::crush_config(text, opts, ctx, refs);
            crate::pipeline::arbitrate_config(text, claimed, opts, ctx, refs, mark)
        }
        _ => None,
    }
}

/// Truncate `text`, cutting on a token boundary and announcing the tail.
fn clip(text: &str, opts: &EngineOptions, refs: &mut RefCollector) -> String {
    crate::markers::clip_to(text, opts.max_field_chars, refs)
}

// ---------------------------------------------------------------------------
// array sampling
// ---------------------------------------------------------------------------

fn crush_array(
    items: Vec<Value>,
    opts: &EngineOptions,
    ctx: &Context<'_>,
    refs: &mut RefCollector,
    depth: usize,
) -> Value {
    if items.len() <= opts.max_array_items {
        return crush_all(items, opts, ctx, refs, depth);
    }
    let serialized: Vec<String> = items.iter().map(canonical).collect();
    let views: Vec<&str> = serialized.iter().map(String::as_str).collect();
    let budget = adaptive_k(&views, 3, opts.max_array_items.min(items.len()), 1.0);
    if items.len() <= budget {
        return crush_all(items, opts, ctx, refs, depth);
    }

    // Position anchors are deterministic; must-keeps then fill remaining slots
    // by score, and source order is restored at the end.
    let first = 1.max(round_half_even(budget as f64 * 0.30));
    let mut last = 1.max(round_half_even(budget as f64 * 0.15));
    while last > 0 && first + last > budget {
        last -= 1;
    }
    let count = items.len();
    let mut kept: HashSet<usize> = (0..first.min(count)).collect();
    kept.extend(count.saturating_sub(last)..count);

    let scores = score_items(&items, &serialized);
    // Must-keeps can push the kept set past the budget. Correctness is
    // preferable to sampling: an error row the caller never sees is worse than
    // a few extra rows it does.
    kept.extend(
        scores
            .iter()
            .enumerate()
            .filter(|&(_, &score)| score >= 1.0)
            .map(|(index, _)| index),
    );
    let mut ranked: Vec<usize> = (0..count).collect();
    ranked.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
    for index in ranked {
        if kept.len() >= budget {
            break;
        }
        kept.insert(index);
    }

    // Built before the items are consumed, but stashed at the marker's position
    // so the collector sees fragments in the order they appear in the output.
    let omitted: Vec<&Value> = items
        .iter()
        .enumerate()
        .filter(|(index, _)| !kept.contains(index))
        .map(|(_, item)| item)
        .collect();
    let omitted_count = omitted.len();
    let fields = omitted
        .iter()
        .find_map(|item| item.as_object())
        .map(|map| {
            let mut names: Vec<&str> = map.keys().map(String::as_str).collect();
            names.sort_unstable();
            format!(" (fields: {})", names.join(", "))
        })
        .unwrap_or_default();
    let fragment = serde_json::to_string(&omitted).unwrap_or_default();

    let mut out: Vec<Value> = Vec::with_capacity(kept.len() + 1);
    let mut marked = false;
    for (index, item) in items.into_iter().enumerate() {
        if kept.contains(&index) {
            out.push(crush_value(item, opts, ctx, refs, depth));
        } else if !marked {
            marked = true;
            // Not one of the bracketed `[... ]` markers: this one stands as an
            // array element in place of the items it replaces.
            let handle = match refs.stash(&fragment) {
                Some(id) => format!(" [ref:{id}]"),
                None => String::new(),
            };
            out.push(Value::String(format!(
                "... {omitted_count} similar items omitted{handle}{fields} ..."
            )));
        }
    }
    Value::Array(out)
}

fn crush_all(
    items: Vec<Value>,
    opts: &EngineOptions,
    ctx: &Context<'_>,
    refs: &mut RefCollector,
    depth: usize,
) -> Value {
    Value::Array(
        items
            .into_iter()
            .map(|item| crush_value(item, opts, ctx, refs, depth))
            .collect(),
    )
}

/// Halfway cases go to the even integer, where `f64::round` goes away from
/// zero. Both anchor fractions land on an exact `.5` for real budgets — 15
/// gives 4.5, 10 gives 1.5 — and a one-item difference there changes which
/// items survive *and* the bytes of the marker accounting for the rest, so the
/// tie-breaking rule is load-bearing rather than a rounding detail.
fn round_half_even(value: f64) -> usize {
    let rounded = value.round_ties_even();
    if rounded <= 0.0 { 0 } else { rounded as usize }
}

/// Compact serialization with keys sorted at every level — the form items are
/// compared and fingerprinted by, so two records differing only in key order
/// count as the same shape.
fn canonical(value: &Value) -> String {
    serde_json::to_string(&sorted(value)).unwrap_or_default()
}

fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = Map::with_capacity(keys.len());
            for key in keys {
                out.insert(key.clone(), sorted(&map[key]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// Error relevance, structural change and numeric outliers, per item.
fn score_items(items: &[Value], serialized: &[String]) -> Vec<f64> {
    let mut scores: Vec<f64> = items
        .iter()
        .map(|item| if is_anomalous(item) { 1.0 } else { 0.0 })
        .collect();

    // Per-key numeric distribution, so a row whose latency or size is far from
    // the rest of the payload survives even though it says nothing unusual.
    let mut numeric: HashMap<&str, Vec<f64>> = HashMap::new();
    for item in items {
        if let Some(map) = item.as_object() {
            for (key, value) in map {
                if let Some(number) = as_f64(value) {
                    numeric.entry(key.as_str()).or_default().push(number);
                }
            }
        }
    }
    let mut stats: HashMap<&str, (f64, f64)> = HashMap::new();
    for (key, values) in &numeric {
        // Two samples describe no distribution; an outlier needs a population.
        if values.len() < 3 {
            continue;
        }
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64;
        stats.insert(key, (mean, variance.sqrt()));
    }

    let mut previous_shape: Option<Vec<(&str, &'static str)>> = None;
    for (index, item) in items.iter().enumerate() {
        let text = serialized[index].to_lowercase();
        let hits = ERROR_TOKENS.iter().filter(|t| text.contains(**t)).count();
        scores[index] += 0.6f64.min(hits as f64 * 0.2);

        if let Some(map) = item.as_object() {
            let mut shape: Vec<(&str, &'static str)> = map
                .iter()
                .map(|(key, value)| (key.as_str(), type_name(value)))
                .collect();
            shape.sort_unstable();
            if previous_shape.as_ref().is_some_and(|prev| *prev != shape) {
                // The row where the payload changes shape is where the schema
                // changed, which is the one a reader needs to see.
                scores[index] = scores[index].max(0.9);
            }
            previous_shape = Some(shape);
            for (key, value) in map {
                let Some(&(mean, deviation)) = stats.get(key.as_str()) else {
                    continue;
                };
                if deviation == 0.0 {
                    continue;
                }
                if as_f64(value).is_some_and(|n| (n - mean).abs() / deviation > 2.0) {
                    scores[index] = scores[index].max(1.0);
                }
            }
        } else if index > 0 && word_shape(&serialized[index]) != word_shape(&serialized[index - 1])
        {
            scores[index] = scores[index].max(0.9);
        }
    }
    scores
}

static WORDS: Lazy<Regex> = Lazy::new(|| Regex::new(r"[A-Za-z_]+").unwrap());

/// The first 16 alphabetic runs of a serialized item: enough of its skeleton to
/// notice that the stream changed form, cheap enough to run per item.
fn word_shape(value: &str) -> Vec<&str> {
    WORDS
        .find_iter(value)
        .take(16)
        .map(|m| m.as_str())
        .collect()
}

/// The shape label a value carries in envelopes and markers.
///
/// `int` and `float` are deliberately distinct, so `{"n": 1}` and `{"n": 1.5}`
/// are different shapes: a columnar envelope is only safe to reverse if every
/// row it folded really did agree on its columns' types.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) => {
            if n.as_i64().is_some() || n.as_u64().is_some() {
                "int"
            } else {
                "float"
            }
        }
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// Numeric value of `value`, or `None`.
///
/// Booleans are their own `Value` variant and so are excluded for free, which is
/// worth stating because a language where `bool` is a subtype of `int` needs an
/// explicit guard at every site that does this.
fn as_f64(value: &Value) -> Option<f64> {
    value.as_number().and_then(serde_json::Number::as_f64)
}

/// Integer value of `value`, or `None` if it is not an integer literal.
fn as_int(value: &Value) -> Option<i128> {
    let number = value.as_number()?;
    number
        .as_i64()
        .map(i128::from)
        .or_else(|| number.as_u64().map(i128::from))
}

fn is_anomalous(item: &Value) -> bool {
    match item {
        Value::String(text) => has_error_token(text),
        Value::Object(map) => map.iter().any(|(key, value)| {
            // The key is only consulted alongside a string value, matching the
            // reference: `{"error_count": 3}` is a counter, not a failure, and
            // the error-token score still gives it weight.
            let flagged = match value {
                Value::String(text) => has_error_token(text) || has_error_token(key),
                _ => false,
            };
            flagged
                || (STATUS_KEYS.contains(&key.to_lowercase().as_str())
                    && as_int(value).is_some_and(|code| !(200..300).contains(&code)))
        }),
        _ => false,
    }
}

fn has_error_token(text: &str) -> bool {
    let lowered = text.to_lowercase();
    ERROR_TOKENS.iter().any(|token| lowered.contains(token))
}

/// Factor objects whose values are homogeneous records, leaving arrays alone.
///
/// The reversible half of the lossless transform, reused in the lossy modes.
/// Arrays are recursed into but never factored: in a lossy mode the sampler
/// already owns them, and rewriting a short array it chose to keep whole would
/// change bytes for no decision anyone made.
///
/// The reserved-shape escape comes along, and has to: this pass still *emits*
/// envelopes, so an input object shaped like one would be ambiguous without it.
fn factor_maps(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut crushed = Map::with_capacity(map.len());
            for (key, item) in map {
                crushed.insert(key, factor_maps(item));
            }
            if is_reserved_shape(&crushed) {
                let mut wrapped = Map::with_capacity(1);
                wrapped.insert(LITERAL_KEY.to_string(), Value::Object(crushed));
                return Value::Object(wrapped);
            }
            factor_map(crushed)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(factor_maps).collect()),
        other => other,
    }
}

// ---------------------------------------------------------------------------
// lossless path
// ---------------------------------------------------------------------------

/// Reversible-only transform: recurse, factoring homogeneous object collections
/// into a self-describing columnar envelope. Nothing is dropped or clipped.
///
/// A payload may itself contain an object shaped exactly like an envelope.
/// Declining to factor around it is not enough — left bare it still *reads* as
/// an envelope, so a consumer reverses it, gets different data, and lossless has
/// quietly stopped being lossless. Such an object is wrapped in `__literal__` to
/// say "this shape is data". The wrap applies only to objects that came from the
/// input, never to an envelope the factorer builds, and that asymmetry is what
/// keeps the two distinguishable.
fn crush_lossless(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut crushed = Map::with_capacity(map.len());
            for (key, item) in map {
                crushed.insert(key, crush_lossless(item));
            }
            if is_reserved_shape(&crushed) {
                let mut wrapped = Map::with_capacity(1);
                wrapped.insert(LITERAL_KEY.to_string(), Value::Object(crushed));
                // `__literal__` is itself a reserved shape, so escaping an
                // escape terminates after one wrap instead of regressing.
                return Value::Object(wrapped);
            }
            factor_map(crushed)
        }
        Value::Array(items) => {
            let crushed: Vec<Value> = items.into_iter().map(crush_lossless).collect();
            let views: Vec<&Value> = crushed.iter().collect();
            match common_columns(&views) {
                None => Value::Array(crushed),
                Some(cols) => {
                    let mut rows: Vec<Value> = Vec::with_capacity(crushed.len());
                    for item in crushed {
                        rows.push(row_for(item, &cols));
                    }
                    let mut envelope = Map::with_capacity(2);
                    envelope.insert(COLS_KEY.to_string(), columns_value(&cols));
                    envelope.insert(ROWS_KEY.to_string(), Value::Array(rows));
                    Value::Object(envelope)
                }
            }
        }
        other => other,
    }
}

/// The exact object shapes a consumer reads as *structure* rather than as data:
/// the two envelope forms, plus the escape itself.
fn is_reserved_shape(map: &Map<String, Value>) -> bool {
    match map.len() {
        1 => map.contains_key(LITERAL_KEY),
        2 => map.contains_key(COLS_KEY) && map.contains_key(ROWS_KEY),
        3 => map.contains_key(KEYS_KEY) && map.contains_key(COLS_KEY) && map.contains_key(ROWS_KEY),
        _ => false,
    }
}

/// Factor an object whose values are homogeneous objects — the map-of-records
/// shape of API indexes and config dumps, which otherwise repeats every record's
/// key names once per entry. Measured 40-46% on such bodies, which previously
/// compressed by nothing at all.
///
/// `__keys__` carries the original keys in order, so the object and its ordering
/// restore exactly.
fn factor_map(crushed: Map<String, Value>) -> Value {
    // Never emit an envelope a consumer could confuse with real data.
    if crushed.keys().any(|key| is_envelope_key(key)) {
        return Value::Object(crushed);
    }
    let values: Vec<&Value> = crushed.values().collect();
    let Some(cols) = common_columns(&values) else {
        return Value::Object(crushed);
    };
    if cols.iter().any(|col| is_envelope_key(col)) {
        return Value::Object(crushed);
    }
    let mut rows: Vec<Value> = Vec::with_capacity(crushed.len());
    for value in &values {
        rows.push(row_for((*value).clone(), &cols));
    }
    let mut factored = Map::with_capacity(3);
    factored.insert(
        KEYS_KEY.to_string(),
        Value::Array(crushed.keys().map(|k| Value::String(k.clone())).collect()),
    );
    factored.insert(COLS_KEY.to_string(), columns_value(&cols));
    factored.insert(ROWS_KEY.to_string(), Value::Array(rows));
    let factored = Value::Object(factored);
    // Applied only when it actually shrinks: for a couple of one-field records
    // the envelope costs more than the repeated key names it removes.
    let plain = Value::Object(crushed);
    if chars_len(&dumps(&factored)) < chars_len(&dumps(&plain)) {
        factored
    } else {
        plain
    }
}

fn is_envelope_key(key: &str) -> bool {
    ENVELOPE_KEYS.contains(&key)
}

fn columns_value(cols: &[String]) -> Value {
    Value::Array(cols.iter().map(|c| Value::String(c.clone())).collect())
}

/// One row: the record's values in column order. The key set was verified to
/// match `cols` exactly, so nothing is dropped and nothing is invented.
fn row_for(item: Value, cols: &[String]) -> Value {
    match item {
        Value::Object(mut map) => Value::Array(
            cols.iter()
                .map(|col| map.remove(col).unwrap_or(Value::Null))
                .collect(),
        ),
        other => other,
    }
}

/// Shared sorted key list for a homogeneous collection of objects, else `None`.
fn common_columns(items: &[&Value]) -> Option<Vec<String>> {
    if items.len() < MIN_FACTOR_ITEMS {
        return None;
    }
    let mut columns: Option<Vec<String>> = None;
    for item in items {
        let map = item.as_object()?;
        let mut keys: Vec<String> = map.keys().cloned().collect();
        keys.sort();
        match &columns {
            None => columns = Some(keys),
            Some(known) if *known != keys => return None,
            Some(_) => {}
        }
    }
    columns.filter(|keys| !keys.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::RefCollector;

    fn opts() -> EngineOptions {
        EngineOptions::default()
    }

    fn ctx() -> Context<'static> {
        Context::for_role("user")
    }

    fn crush(content: &str, opts: &EngineOptions) -> Option<String> {
        crush_json(content, opts, &ctx(), &mut opts.collector())
    }

    /// A homogeneous array of `count` request records, one per index.
    fn rows(count: usize) -> Vec<Value> {
        (0..count)
            .map(|i| serde_json::json!({"path": format!("/api/item/{i}"), "status": 200, "ms": 10}))
            .collect()
    }

    fn body(items: Vec<Value>) -> String {
        serde_json::to_string(&Value::Array(items)).unwrap()
    }

    fn marker_count(text: &str) -> usize {
        text.matches("similar items omitted").count()
    }

    #[test]
    fn a_long_homogeneous_array_is_sampled_behind_exactly_one_marker() {
        let input = body(rows(200));
        let out = crush(&input, &opts()).expect("200 uniform records must compress");
        assert_eq!(marker_count(&out), 1, "one marker, at the first gap: {out}");
        assert!(chars_len(&out) < chars_len(&input) / 2);
        assert!(out.contains("(fields: ms, path, status)"), "got {out}");
    }

    #[test]
    fn a_short_array_is_left_whole() {
        let input = body(rows(4));
        assert_eq!(crush(&input, &opts()), None, "nothing to sample or clip");
    }

    #[test]
    fn a_row_carrying_an_error_string_always_survives() {
        let mut items = rows(200);
        items[97] = serde_json::json!({
            "path": "/api/item/97", "status": 200, "ms": 10,
            "note": "connection refused by upstream"
        });
        let out = crush(&body(items), &opts()).unwrap();
        assert!(out.contains("connection refused by upstream"), "got {out}");
    }

    #[test]
    fn a_non_2xx_status_row_always_survives() {
        let mut items = rows(200);
        items[142] = serde_json::json!({"path": "/api/item/142", "status": 503, "ms": 10});
        let out = crush(&body(items), &opts()).unwrap();
        assert!(out.contains("\"status\":503"), "got {out}");
        // and a 2xx row with no other signal is still eligible for omission
        assert!(marker_count(&out) == 1);
    }

    #[test]
    fn a_numeric_outlier_survives_even_when_it_says_nothing_unusual() {
        let mut items = rows(200);
        items[63] = serde_json::json!({"path": "/api/item/63", "status": 200, "ms": 9000});
        let out = crush(&body(items), &opts()).unwrap();
        assert!(out.contains("9000"), "the slow request is the point: {out}");
    }

    #[test]
    fn a_change_of_shape_is_kept() {
        let mut items = rows(200);
        items[120] = serde_json::json!({"path": "/api/item/120", "status": 200, "elapsed": 10});
        let out = crush(&body(items), &opts()).unwrap();
        assert!(
            out.contains("elapsed"),
            "the schema change is the news: {out}"
        );
    }

    #[test]
    fn a_status_key_outside_the_success_range_is_anomalous_only_for_integers() {
        assert!(is_anomalous(&serde_json::json!({"status": 500})));
        assert!(is_anomalous(&serde_json::json!({"Status_Code": 301})));
        assert!(!is_anomalous(&serde_json::json!({"status": 204})));
        assert!(!is_anomalous(&serde_json::json!({"status": "500"})));
        assert!(!is_anomalous(&serde_json::json!({"status": 500.5})));
        assert!(!is_anomalous(&serde_json::json!({"code": true})));
    }

    #[test]
    fn the_error_score_saturates_at_three_distinct_tokens() {
        let items = vec![
            serde_json::json!({"a": 1}),
            serde_json::json!({"a": "timeout then panic then traceback then fatal"}),
        ];
        let serialized: Vec<String> = items.iter().map(canonical).collect();
        let scores = score_items(&items, &serialized);
        // anomalous (1.0) plus the capped token bonus (0.6)
        assert!((scores[1] - 1.6).abs() < 1e-9, "got {}", scores[1]);
    }

    #[test]
    fn an_oversized_string_is_clipped_on_a_boundary_and_counted_in_code_points() {
        let mut opts = opts();
        opts.enable_nested_routing = false;
        let text = format!("{}{}{}", "é".repeat(200), " ", "é".repeat(199));
        let input = serde_json::to_string(&serde_json::json!({"note": text})).unwrap();
        let out = crush(&input, &opts).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        let note = parsed["note"].as_str().unwrap();
        assert!(
            note.starts_with(&format!("{} ", "é".repeat(200))),
            "cut at the space"
        );
        // 400 code points, 799 bytes: the tail count must be the former
        assert!(note.contains("[+199 chars"), "got {note}");
    }

    #[test]
    fn a_clip_with_no_separator_in_range_cuts_exactly_at_the_limit() {
        let mut opts = opts();
        opts.enable_nested_routing = false;
        let text = "x".repeat(600);
        let input = serde_json::to_string(&serde_json::json!({"note": text})).unwrap();
        let out = crush(&input, &opts).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        let note = parsed["note"].as_str().unwrap();
        assert!(note.starts_with(&"x".repeat(opts.max_field_chars)));
        assert!(note.contains(&format!("[+{} chars", 600 - opts.max_field_chars)));
    }

    #[test]
    fn a_clip_stashes_the_tail_behind_the_handle_it_advertises() {
        let mut options = opts();
        options.enable_nested_routing = false;
        let text = "x".repeat(600);
        let input = serde_json::to_string(&serde_json::json!({"note": text})).unwrap();
        let mut refs = options.collector();
        let out = crush_json(&input, &options, &ctx(), &mut refs).unwrap();
        assert_eq!(refs.len(), 1);
        let (id, fragment) = &refs.pairs()[0];
        assert!(out.contains(&id.to_string()), "the marker names the handle");
        assert_eq!(chars_len(fragment), 600 - options.max_field_chars);
    }

    #[test]
    fn a_wrapped_log_field_is_compressed_by_its_own_compressor_not_clipped() {
        let mut lines: Vec<String> = (0..300)
            .map(|i| format!("2026-09-15T10:00:00Z INFO  worker {i} handled request ok"))
            .collect();
        lines[250] = "2026-09-15T10:00:00Z ERROR worker exploded on shard 7".to_string();
        let log = lines.join("\n");
        let input = serde_json::to_string(&serde_json::json!({"stdout": log})).unwrap();
        let out = crush(&input, &opts()).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        let stdout = parsed["stdout"].as_str().unwrap();
        // A blind clip keeps the first 256 characters and silently drops the
        // rest — including the one line anybody reading this log wants.
        assert!(
            stdout.contains("ERROR worker exploded on shard 7"),
            "got {stdout}"
        );
        assert!(chars_len(stdout) < chars_len(&log));
    }

    #[test]
    fn a_wrapped_config_field_is_folded_rather_than_returned_whole() {
        // The defect, in the shape it was found in. A harness wraps a tool
        // result in a chunk envelope -- `{"chunk_id":…,"output":"<the file>"}`
        // -- so the file arrives as one JSON string value. Nested routing sent
        // a config to `crush_config` bare, which elides values one key at a
        // time: the right reading for a hand-written file and nearly useless
        // on generated output. The whole-message route has always weighed that
        // against a value-normalised fold and shipped the shorter; this path
        // did not, so the same bytes compressed 99.13% as a file and 0.00% as
        // a field. Measured on a live codex sweep, where the field was 72,665
        // characters and 38.8% of everything that survived the run.
        let yaml: String = (0..700)
            .map(|i| {
                format!(
                    "service_{i}:\n  image: registry.example.com/app:{}\n  replicas: {}\n  \
                     port: {}\n  env: production\n",
                    i % 50,
                    i % 5 + 1,
                    8000 + i % 50
                )
            })
            .collect();
        let input = serde_json::to_string(&serde_json::json!({
            "chunk_id": "be089d",
            "exit_code": 0,
            "output": yaml,
        }))
        .unwrap();
        let out = crush(&input, &opts()).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        let output = parsed["output"].as_str().unwrap();
        assert!(
            chars_len(output) * 2 < chars_len(&yaml),
            "a config field must be folded, not handed back whole: {} -> {}",
            chars_len(&yaml),
            chars_len(output)
        );
        // Folded, not clipped: the fold names what it removed, where a clip
        // would simply stop. Either is recoverable, but only one of them is
        // the right answer for a payload whose own compressor declined.
        assert!(
            output.contains("service_0:"),
            "the fold keeps the lines it did not elide: {output}"
        );

        // And which switch is responsible, so a failure says where to look.
        // Off, this is the behaviour the sweep measured: a structured field
        // whose compressor declined is handed back whole rather than clipped,
        // because a mid-record cut would corrupt it.
        let mut without = opts();
        without.enable_fold_fallback = false;
        assert!(
            crush(&input, &without).is_none(),
            "with the fold fallback off nothing shrinks at all, which is the \
             payload the sweep reported as `0.00% removed, untouched`"
        );
    }

    /// A compact JSON array the crusher cannot improve on: four records, under
    /// the array limit, every string under the field limit — but the whole well
    /// over it, so it reaches the crusher as a field that wants clipping.
    fn incompressible_payload() -> String {
        let note = |n: &str| {
            format!(
                "the {n} record, whose value is deliberately long enough to matter but still comfortably under the per-field clip threshold"
            )
        };
        serde_json::to_string(&serde_json::json!([
            {"id": "alpha", "note": note("first")},
            {"id": "beta", "note": note("second")},
            {"id": "gamma", "note": note("third")},
            {"id": "delta", "note": note("fourth")}
        ]))
        .unwrap()
    }

    #[test]
    fn a_structured_field_whose_compressor_declines_is_kept_whole_not_truncated() {
        let payload = incompressible_payload();
        assert!(chars_len(&payload) > opts().max_field_chars);
        let input = serde_json::to_string(&serde_json::json!({
            "payload": payload,
            "note": "z".repeat(900),
        }))
        .unwrap();
        let out = crush(&input, &opts()).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            parsed["payload"].as_str().unwrap(),
            payload,
            "truncating valid JSON mid-record is worse than leaving it long"
        );
        assert!(
            parsed["note"].as_str().unwrap().contains("chars"),
            "prose still clips"
        );
    }

    #[test]
    fn a_rejected_nested_attempt_leaves_no_orphan_handles() {
        let input = serde_json::to_string(&serde_json::json!({
            "payload": incompressible_payload(),
            "note": "z".repeat(900),
        }))
        .unwrap();
        let options = opts();
        let mut refs = options.collector();
        let out = crush_json(&input, &options, &ctx(), &mut refs).unwrap();
        assert!(!refs.is_empty(), "the clipped prose does retain its tail");
        for (id, _) in refs.pairs() {
            assert!(
                out.contains(&id.to_string()),
                "handle {id} is in the store with nothing pointing at it"
            );
        }
    }

    #[test]
    fn nested_routing_can_be_switched_off_entirely() {
        let mut options = opts();
        options.enable_nested_routing = false;
        let payload = incompressible_payload();
        let input = serde_json::to_string(&serde_json::json!({"payload": payload})).unwrap();
        let out = crush(&input, &options).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert!(
            parsed["payload"].as_str().unwrap().contains("chars"),
            "with the kill switch on, the pre-feature answer is a clip"
        );
    }

    // --- lossless ----------------------------------------------------------

    fn lossless() -> EngineOptions {
        EngineOptions {
            mode: Mode::Lossless,
            ..EngineOptions::default()
        }
    }

    /// Reverse every transform the lossless path can emit. Reversibility is the
    /// whole claim of the mode, so the tests check it by actually reversing.
    fn restore(value: &Value) -> Value {
        match value {
            Value::Array(items) => Value::Array(items.iter().map(restore).collect()),
            Value::Object(map) => {
                if map.len() == 1
                    && let Some(Value::Object(inner)) = map.get(LITERAL_KEY)
                {
                    // escaped input: its shape is data, not an envelope
                    let mut out = Map::new();
                    for (key, item) in inner {
                        out.insert(key.clone(), restore(item));
                    }
                    return Value::Object(out);
                }
                let cols: Option<Vec<String>> = map.get(COLS_KEY).and_then(|c| {
                    c.as_array().map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                });
                let rows = map.get(ROWS_KEY).and_then(Value::as_array);
                match (cols, rows, map.len()) {
                    (Some(cols), Some(rows), 2) => {
                        Value::Array(rows.iter().map(|r| record(r, &cols)).collect())
                    }
                    (Some(cols), Some(rows), 3) => {
                        let keys = map[KEYS_KEY].as_array().unwrap();
                        let mut out = Map::new();
                        for (key, row) in keys.iter().zip(rows) {
                            out.insert(key.as_str().unwrap().to_string(), record(row, &cols));
                        }
                        Value::Object(out)
                    }
                    _ => {
                        let mut out = Map::new();
                        for (key, item) in map {
                            out.insert(key.clone(), restore(item));
                        }
                        Value::Object(out)
                    }
                }
            }
            other => other.clone(),
        }
    }

    fn record(row: &Value, cols: &[String]) -> Value {
        let mut out = Map::new();
        for (col, cell) in cols.iter().zip(row.as_array().unwrap()) {
            out.insert(col.clone(), restore(cell));
        }
        Value::Object(out)
    }

    /// Compared canonically: a factored record's key order is declared by
    /// `__cols__`, which is sorted, so records restore with sorted keys. No
    /// value, no key and no record is lost, which is what lossless promises —
    /// the ordering a caller can actually observe, of the *collection*, is
    /// asserted separately by the map test through `__keys__`.
    fn assert_reversible(input: &str, out: &str) {
        let before: Value = serde_json::from_str(input).unwrap();
        let after: Value = serde_json::from_str(out).unwrap();
        assert_eq!(
            canonical(&restore(&after)),
            canonical(&before),
            "lossless output did not restore to its input"
        );
    }

    #[test]
    fn lossless_factors_an_array_of_records_and_restores_it_exactly() {
        let input = body(rows(40));
        let out = crush(&input, &lossless()).expect("repeated key names are removable");
        assert!(out.contains("\"__cols__\""));
        assert!(!out.contains("__keys__"), "an array has no keys to carry");
        assert_reversible(&input, &out);
    }

    #[test]
    fn lossless_factors_a_map_of_records_and_restores_its_key_order() {
        let mut map = Map::new();
        for i in 0..12 {
            map.insert(
                format!("resource-{i}"),
                serde_json::json!({"owner": "platform", "region": "eu-west-1", "replicas": i}),
            );
        }
        let input = serde_json::to_string(&Value::Object(map)).unwrap();
        let out = crush(&input, &lossless()).expect("a map of records factors");
        assert!(
            out.contains("\"__keys__\""),
            "key order must be carried: {out}"
        );
        assert_reversible(&input, &out);
        let restored = restore(&serde_json::from_str::<Value>(&out).unwrap());
        let order: Vec<&str> = restored
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            order[0], "resource-0",
            "the map's own order restores exactly"
        );
        assert_eq!(order[11], "resource-11");
    }

    #[test]
    fn lossless_declines_a_map_too_small_for_the_envelope_to_pay_for_itself() {
        let input = r#"{"a":{"x":1},"b":{"x":2}}"#;
        assert_eq!(
            crush(input, &lossless()),
            None,
            "two one-field records cost more in envelope than they save"
        );
    }

    #[test]
    fn lossless_never_clips_and_never_samples() {
        let mut items = rows(120);
        // same key set, so the collection still factors: one record carries a
        // field far past the clip threshold and an array far past the sampling
        // threshold, and neither may lose a byte
        items.push(serde_json::json!({
            "path": "q".repeat(4000), "status": 200, "ms": 10
        }));
        let input = body(items);
        let out = crush(&input, &lossless()).unwrap();
        assert!(!out.contains("omitted"), "nothing may be dropped");
        assert!(!out.contains(" chars"), "nothing may be clipped");
        assert!(!out.contains('…'));
        assert!(out.contains(&"q".repeat(4000)), "the long field is intact");
    }

    #[test]
    fn an_input_shaped_like_an_envelope_is_escaped_so_the_round_trip_still_holds() {
        let input = r#"{"__cols__":["a","b"],"__rows__":[[1,2]]}"#;
        let out = crush_json(input, &lossless(), &ctx(), &mut RefCollector::disabled());
        // the escape is longer than the input, so the never-larger guard wins
        let escaped = match out {
            Some(text) => text,
            None => serde_json::to_string(&crush_lossless(
                serde_json::from_str::<Value>(input).unwrap(),
            ))
            .unwrap(),
        };
        assert!(escaped.starts_with("{\"__literal__\":"), "got {escaped}");
        assert_reversible(input, &escaped);
    }

    #[test]
    fn escaping_an_escape_terminates_after_one_wrap() {
        // `__literal__` is itself a reserved shape, so the wrap the escape just
        // built would qualify for an escape of its own. It does not get one: the
        // wrap is emitted rather than re-examined, so this terminates instead of
        // regressing.
        let input = r#"{"__literal__":{"a":1}}"#;
        let once = crush_lossless(serde_json::from_str::<Value>(input).unwrap());
        let text = serde_json::to_string(&once).unwrap();
        assert_eq!(text, r#"{"__literal__":{"__literal__":{"a":1}}}"#);
        assert_eq!(
            text.matches(LITERAL_KEY).count(),
            2,
            "exactly one wrap added"
        );
        assert_reversible(input, &text);
    }

    #[test]
    fn an_object_holding_an_envelope_key_is_never_factored() {
        let input = r#"{"__keys__":{"a":1,"b":2},"other":{"a":3,"b":4}}"#;
        assert_eq!(
            crush(input, &lossless()),
            None,
            "factoring here would emit an envelope indistinguishable from data"
        );
    }

    // --- parsing and invariants -------------------------------------------

    #[test]
    fn ndjson_is_read_as_one_array() {
        let lines: Vec<String> = (0..200)
            .map(|i| format!("{{\"path\":\"/api/item/{i}\",\"status\":200,\"ms\":10}}"))
            .collect();
        let input = lines.join("\n");
        let out = crush(&input, &opts()).expect("200 records compress however they arrive");
        assert!(out.starts_with('['), "the lines become one array: {out}");
        assert_eq!(marker_count(&out), 1);
    }

    #[test]
    fn a_single_json_line_is_not_ndjson() {
        // it would already have parsed as a document; one item means the
        // document attempt failed for another reason
        assert_eq!(parse("{\"a\":1}\ntrailing garbage"), None);
    }

    #[test]
    fn malformed_json_is_left_alone() {
        assert_eq!(crush("{not json at all", &opts()), None);
        assert_eq!(crush("", &opts()), None);
        assert_eq!(
            crush(&"prose that is merely long. ".repeat(80), &opts()),
            None
        );
    }

    #[test]
    fn numeric_literals_keep_their_value_and_precision() {
        let input = format!(
            "{{\"scale\":1e5,\"ratio\":0.1000000000000000005,\"price\":1.50,\"big\":12345678901234567890123,\"note\":\"{}\"}}",
            "z".repeat(900)
        );
        let out = crush(&input, &opts()).unwrap();
        // a normalizing round trip would hand back 100000.0, 0.1 and 1.5 here
        assert!(
            out.contains("\"scale\":1e+5"),
            "magnitude must survive: {out}"
        );
        assert!(
            out.contains("0.1000000000000000005"),
            "precision must survive: {out}"
        );
        assert!(
            out.contains("\"price\":1.50"),
            "trailing zeros must survive: {out}"
        );
        assert!(
            out.contains("12345678901234567890123"),
            "an i64 overflow must survive: {out}"
        );
    }

    #[test]
    fn the_output_is_never_longer_than_the_input() {
        let cases = vec![
            body(rows(200)),
            body(rows(3)),
            r#"{"a":{"x":1},"b":{"x":2}}"#.to_string(),
            r#"{"__cols__":["a"],"__rows__":[[1]]}"#.to_string(),
            r#"[]"#.to_string(),
            r#"{}"#.to_string(),
            r#"null"#.to_string(),
            format!("\"{}\"", "z".repeat(900)),
        ];
        for mode in [Mode::Aggressive, Mode::Recoverable, Mode::Lossless] {
            let options = EngineOptions {
                mode,
                ..EngineOptions::default()
            };
            for case in &cases {
                if let Some(out) = crush(case, &options) {
                    assert!(
                        chars_len(&out) < chars_len(case),
                        "{mode} grew a payload: {case} -> {out}"
                    );
                }
            }
        }
    }

    #[test]
    fn sampling_is_deterministic_across_runs() {
        let input = body(rows(200));
        let first = crush(&input, &opts()).unwrap();
        for _ in 0..3 {
            assert_eq!(crush(&input, &opts()).unwrap(), first);
        }
    }

    #[test]
    fn the_head_and_tail_of_a_sampled_array_are_both_present() {
        let out = crush(&body(rows(200)), &opts()).unwrap();
        assert!(
            out.contains("/api/item/0"),
            "the first record anchors the head"
        );
        assert!(
            out.contains("/api/item/199"),
            "the last record anchors the tail"
        );
    }

    #[test]
    fn pythons_half_to_even_rounding_is_preserved() {
        // 15 * 0.30 lands exactly on 4.5, where away-from-zero would give 5
        assert_eq!(round_half_even(4.5), 4);
        assert_eq!(round_half_even(5.5), 6);
        assert_eq!(round_half_even(0.3), 0);
        assert_eq!(round_half_even(-1.0), 0);
    }
}

#[cfg(test)]
mod lossy_factoring_tests {
    use super::*;
    use crate::types::Mode;

    fn lossy() -> EngineOptions {
        EngineOptions {
            mode: Mode::Aggressive,
            min_compress_chars: 1,
            ..Default::default()
        }
    }

    fn run(input: &str, opts: &EngineOptions) -> Option<String> {
        let mut refs = opts.collector();
        crush_json(input, opts, &Context::for_role("tool"), &mut refs)
    }

    /// The map-of-records shape of an API index or a config dump: the reason
    /// this exists at all, since sampling cannot touch it and it repeats every
    /// record's key names once per entry.
    fn map_of_records(count: usize) -> String {
        let entries: Vec<String> = (0..count)
            .map(|i| {
                format!(
                    r#""user_{i}":{{"name":"n{i}","age":{},"city":"lisbon"}}"#,
                    20 + i % 40
                )
            })
            .collect();
        format!("{{{}}}", entries.join(","))
    }

    #[test]
    fn a_map_of_records_now_compresses_in_a_lossy_mode() {
        let input = map_of_records(40);
        let without = EngineOptions {
            enable_lossy_key_factoring: false,
            ..lossy()
        };
        assert!(
            run(&input, &without).is_none(),
            "this is the gap: sampling has nothing to take from a map"
        );
        let out = run(&input, &lossy()).expect("factoring claims it");
        assert!(out.contains("\"__cols__\""));
        assert!(out.chars().count() < input.chars().count() * 7 / 10);
    }

    #[test]
    fn factoring_a_lossy_payload_loses_nothing() {
        let input = map_of_records(12);
        let out = run(&input, &lossy()).expect("factoring claims it");
        let envelope: serde_json::Value = serde_json::from_str(&out).unwrap();
        let keys = envelope["__keys__"].as_array().unwrap();
        let cols: Vec<&str> = envelope["__cols__"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap())
            .collect();
        let rows = envelope["__rows__"].as_array().unwrap();

        let mut restored = serde_json::Map::new();
        for (key, row) in keys.iter().zip(rows) {
            let mut record = serde_json::Map::new();
            for (col, value) in cols.iter().zip(row.as_array().unwrap()) {
                record.insert((*col).to_string(), value.clone());
            }
            restored.insert(
                key.as_str().unwrap().to_string(),
                serde_json::Value::Object(record),
            );
        }
        let original: serde_json::Value = serde_json::from_str(&input).unwrap();
        assert_eq!(
            serde_json::Value::Object(restored),
            original,
            "the envelope reconstructs the input exactly"
        );
    }

    #[test]
    fn arrays_are_left_to_the_sampler() {
        // A long array is sampled, and a short one is deliberately kept whole;
        // factoring either would rewrite bytes for a decision nobody made.
        let items: Vec<String> = (0..80)
            .map(|i| format!(r#"{{"id":{i},"status":"ok","ms":{}}}"#, i % 7))
            .collect();
        let long = format!("[{}]", items.join(","));
        let out = run(&long, &lossy()).expect("sampling claims it");
        assert!(out.contains("similar items omitted"), "sampling still ran");
        assert!(!out.contains("\"__cols__\""), "the sampler owns arrays");

        let short = format!("[{}]", items[..4].join(","));
        assert!(
            run(&short, &lossy()).is_none(),
            "a short array stays exactly as it arrived"
        );
    }

    #[test]
    fn a_record_map_nested_inside_an_array_still_factors() {
        let input = format!(r#"[{{"index":{}}}]"#, map_of_records(20));
        let out = run(&input, &lossy()).expect("the nested map factors");
        assert!(out.contains("\"__cols__\""));
        assert!(out.starts_with('['), "the array itself is untouched");
    }

    #[test]
    fn an_envelope_shaped_input_stays_unambiguous_in_a_lossy_mode() {
        // Left bare, this object would read as an envelope and a consumer
        // reversing it would get different data back.
        let input = format!(
            r#"{{"payload":{{"__cols__":["a","b"],"__rows__":[[1,2]]}},"index":{}}}"#,
            map_of_records(10)
        );
        let out = run(&input, &lossy()).expect("the index factors");
        assert!(
            out.contains("__literal__"),
            "the ambiguous shape is escaped"
        );
    }

    #[test]
    fn the_flag_off_leaves_map_shaped_json_uncompressed() {
        let input = map_of_records(40);
        let off = EngineOptions {
            enable_lossy_key_factoring: false,
            ..lossy()
        };
        assert!(run(&input, &off).is_none());
    }
}
