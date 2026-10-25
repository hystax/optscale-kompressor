//! The engine's behaviour, locked to recorded vectors.
//!
//! Compression is a pile of interacting thresholds, and the failure mode that
//! matters is not a crash — it is a change in *which payloads compress and to
//! what*, arriving as a side effect of tuning something else. Unit tests do not
//! catch that; a recorded snapshot does.
//!
//! `fixtures/golden.json.gz` holds the compressed bytes, the reported type and
//! the handles minted for two corpora — 233 payloads of real agent tool output,
//! and 34 hand-written micro payloads that reach detector branches the real
//! corpus does not hold — in all three modes. See `tools/export_golden.py` for
//! how to regenerate them.
//!
//! Vectors are a snapshot, not a specification: several engine behaviours have
//! changed deliberately since they were captured, and each of those lives behind
//! a flag. The comparison therefore runs with those flags set to the state the
//! vectors were recorded in, and the changed behaviours get their own tests
//! elsewhere. Anything else that differs is a regression.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;

use kompressor_engine::detect::{detect_content, detect_result};
use kompressor_engine::pipeline::compress_content;
use kompressor_engine::types::{Context, Mode};
use kompressor_engine::{EngineOptions, Kind};

#[derive(serde::Deserialize)]
struct Golden {
    records: Vec<Record>,
}

#[derive(serde::Deserialize)]
struct Record {
    content_id: String,
    role: String,
    content: String,
    #[serde(default)]
    preceding_user_text: String,
    #[serde(default)]
    source_tool: String,
    #[serde(default)]
    detected: Detected,
    modes: BTreeMap<String, ModeResult>,
}

#[derive(serde::Deserialize, Default)]
struct Detected {
    content_type: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    flavor: Option<String>,
    #[serde(default)]
    uniform_stream: bool,
}

#[derive(serde::Deserialize)]
struct ModeResult {
    output: String,
    kind: Option<String>,
    #[serde(default)]
    refs: Vec<String>,
}

fn load() -> Vec<Record> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/golden.json.gz")
        .canonicalize()
        .expect("golden vectors are committed next to the corpus");
    let file = std::fs::File::open(path).expect("golden vectors readable");
    let mut text = String::new();
    flate2::read::GzDecoder::new(file)
        .read_to_string(&mut text)
        .expect("golden vectors are gzipped json");
    serde_json::from_str::<Golden>(&text)
        .expect("golden vectors parse")
        .records
}

/// Why plain text moved when the text fold stopped reaching across a payload.
const TEXT_FOLDS_IN_PLACE: &str = "Plain text folded by a value-normalised \
     template wherever it recurred, so the first occurrence absorbed every later \
     one and the lines between closed up. Most of what that removed was source \
     the detector did not name (R, Python, Redcode, Makefiles) and tables whose \
     rows are the data. It folds only runs of consecutive lines now, four or \
     more unless identical, and keeps the rest where it stood. On an 89-task \
     run the tasks that saw the wide fold took 17.5% more agent steps than the \
     uncompressed run.";

/// Why disassembly moved when the detector learned to recognise it.
const DISASSEMBLY_SHIPS_WHOLE: &str = "Disassembly. The vectors were recorded \
     by a detector that read `objdump` rows as a listing, which samples away the \
     middle of a function, or as text, which folds similar instructions into \
     one. It is source with no grammar now and ships whole: on a measured run an \
     agent paged a folded disassembly by hand, 40 steps against 12.";

/// Payloads where the engine deliberately and verifiably differs from the
/// vectors, and why.
///
/// What is asserted is "zero *unexplained* differences", not "identical": a
/// snapshot captures mistakes as faithfully as it captures intent, so
/// reproducing all of it is the wrong goal. Each entry here was confirmed by
/// experiment rather than inferred, and anything not listed is a regression.
///
/// Keyed by mode as well as payload. A payload that changed in one mode is
/// still fully checked in the other two, which matters when the change is a
/// mode's own policy rather than a compressor's output: exempting all three
/// would hide a real regression behind a deliberate one.
const KNOWN_DELTAS: &[(&str, &str, &str)] = &[
    ("160dd5259e439aff", "*", TEXT_FOLDS_IN_PLACE),
    ("266c00367241a1ae", "*", TEXT_FOLDS_IN_PLACE),
    ("3a36adcff8882d24", "*", TEXT_FOLDS_IN_PLACE),
    ("6a99b022637d93d7", "*", TEXT_FOLDS_IN_PLACE),
    ("721a060c70b6abf8", "*", TEXT_FOLDS_IN_PLACE),
    ("83ed7d4590189696", "*", TEXT_FOLDS_IN_PLACE),
    ("996f12cf7bfa28d5", "*", TEXT_FOLDS_IN_PLACE),
    ("ab854401ccaf555c", "*", TEXT_FOLDS_IN_PLACE),
    ("af315d3ade85dc6f", "*", TEXT_FOLDS_IN_PLACE),
    ("bceda9507f8558ba", "*", TEXT_FOLDS_IN_PLACE),
    ("d1dc8cfbf82ebdc5", "*", TEXT_FOLDS_IN_PLACE),
    ("e08cec1e48ccc8d9", "*", TEXT_FOLDS_IN_PLACE),
    ("e253da890aaa86c4", "*", TEXT_FOLDS_IN_PLACE),
    ("edd95442fd9778ef", "*", TEXT_FOLDS_IN_PLACE),
    ("f77897c833d80e11", "*", TEXT_FOLDS_IN_PLACE),
    ("dae24a04ef823b86", "*", DISASSEMBLY_SHIPS_WHOLE),
    (
        "97cf0b663b3a8011",
        "*",
        "`ls && cat algo.py`: three file names and then the source. The vectors \
         were recorded by an engine whose grammar failed on the first file name, \
         so the code compressor declined and the payload shipped whole at 1,249 \
         characters. This one sets the lead-in aside verbatim — frame and \
         listing byte for byte — and compresses the source behind it as the \
         Python it is, eliding `map`'s 34-line body. Every mode moved because \
         the recorded options run code elision in all three. On the Claude \
         Code replay corpus the same shape was the largest single loss.",
    ),
    (
        "77084080070a4e1d",
        "*",
        "An apt transcript whose frame carries `Warning: truncated output` and \
         `Total output lines:` *under* its `Output:` line. The vectors were \
         recorded by a splitter that stopped at the terminator, so those two \
         lines stayed in the body — counted among the log lines, and the \
         `Warning:` counted as the payload's one WARN, which it is not: it is \
         the harness saying it cut the result, not the build reporting \
         anything. This engine holds them in the frame, so the body is 299 \
         lines with 0 WARN and the output is 81 characters longer. Bytes \
         lost, correctness gained — and on the payloads the shape actually \
         matters for it is decisive: a 40 KB codex-truncated Python result \
         goes from 0.00% to 86.93%, because those lines in front of the body \
         are a parse error to every grammar.",
    ),
    (
        "9f879fbc95d8fb35",
        "*",
        "The vectors were recorded by a log fold that appends its kept tail a \
         second time when nothing folded but diagnostic echo lines were removed. \
         Its output is therefore longer than its input, and the caller refuses a \
         shrink that was real. Correcting that one expression reproduces the \
         bytes this engine now emits, exactly.",
    ),
    (
        "da7ab918b93c11e4",
        "*",
        "Python 2 source. The vectors were recorded by a strict parser that \
         rejects a `print` statement, so salvage kept only the 65 lines before \
         the first one; tree-sitter-python parses the same file with no error \
         node at all (confirmed directly), so this engine sees the whole class \
         and elides a larger body. More is compressed, and every safety \
         invariant still holds.",
    ),
    (
        "01e36758efab39f4",
        "aggressive",
        "Recorded when a fold in a mode that retains nothing emitted a bare \
         `[×N]`. That count says the kept line occurred N times; these \
         payloads fold lines that merely share a template, so it named a line \
         occurring once — on one 4,000-line payload, `[×3980]` with 3,979 \
         distinct lines destroyed. `aggressive` now says what left instead of \
         counting it, so the marker is longer and the output is a little \
         bigger. Only `aggressive` moved; the other two modes still match.",
    ),
    (
        "21edea15ac8ed849",
        "aggressive",
        "Recorded when a fold in a mode that retains nothing emitted a bare \
         `[×N]`. That count says the kept line occurred N times; these \
         payloads fold lines that merely share a template, so it named a line \
         occurring once — on one 4,000-line payload, `[×3980]` with 3,979 \
         distinct lines destroyed. `aggressive` now says what left instead of \
         counting it, so the marker is longer and the output is a little \
         bigger. Only `aggressive` moved; the other two modes still match.",
    ),
    (
        "504e494696a824c6",
        "aggressive",
        "Recorded when a fold in a mode that retains nothing emitted a bare \
         `[×N]`. That count says the kept line occurred N times; these \
         payloads fold lines that merely share a template, so it named a line \
         occurring once — on one 4,000-line payload, `[×3980]` with 3,979 \
         distinct lines destroyed. `aggressive` now says what left instead of \
         counting it, so the marker is longer and the output is a little \
         bigger. Only `aggressive` moved; the other two modes still match.",
    ),
    (
        "535c3206b1378f43",
        "aggressive",
        "Recorded when a fold in a mode that retains nothing emitted a bare \
         `[×N]`. That count says the kept line occurred N times; these \
         payloads fold lines that merely share a template, so it named a line \
         occurring once — on one 4,000-line payload, `[×3980]` with 3,979 \
         distinct lines destroyed. `aggressive` now says what left instead of \
         counting it, so the marker is longer and the output is a little \
         bigger. Only `aggressive` moved; the other two modes still match.",
    ),
    (
        "54094212038bb110",
        "aggressive",
        "Recorded when a fold in a mode that retains nothing emitted a bare \
         `[×N]`. That count says the kept line occurred N times; these \
         payloads fold lines that merely share a template, so it named a line \
         occurring once — on one 4,000-line payload, `[×3980]` with 3,979 \
         distinct lines destroyed. `aggressive` now says what left instead of \
         counting it, so the marker is longer and the output is a little \
         bigger. Only `aggressive` moved; the other two modes still match.",
    ),
    (
        "57f10573bbed5f6e",
        "aggressive",
        "Recorded when a fold in a mode that retains nothing emitted a bare \
         `[×N]`. That count says the kept line occurred N times; these \
         payloads fold lines that merely share a template, so it named a line \
         occurring once — on one 4,000-line payload, `[×3980]` with 3,979 \
         distinct lines destroyed. `aggressive` now says what left instead of \
         counting it, so the marker is longer and the output is a little \
         bigger. Only `aggressive` moved; the other two modes still match.",
    ),
    (
        "62c661dd0756f478",
        "aggressive",
        "Recorded when a fold in a mode that retains nothing emitted a bare \
         `[×N]`. That count says the kept line occurred N times; these \
         payloads fold lines that merely share a template, so it named a line \
         occurring once — on one 4,000-line payload, `[×3980]` with 3,979 \
         distinct lines destroyed. `aggressive` now says what left instead of \
         counting it, so the marker is longer and the output is a little \
         bigger. Only `aggressive` moved; the other two modes still match.",
    ),
    (
        "71127fe5a37fbc8e",
        "aggressive",
        "Recorded when a fold in a mode that retains nothing emitted a bare \
         `[×N]`. That count says the kept line occurred N times; these \
         payloads fold lines that merely share a template, so it named a line \
         occurring once — on one 4,000-line payload, `[×3980]` with 3,979 \
         distinct lines destroyed. `aggressive` now says what left instead of \
         counting it, so the marker is longer and the output is a little \
         bigger. Only `aggressive` moved; the other two modes still match.",
    ),
    (
        "732484d4a4e077af",
        "aggressive",
        "Recorded when a fold in a mode that retains nothing emitted a bare \
         `[×N]`. That count says the kept line occurred N times; these \
         payloads fold lines that merely share a template, so it named a line \
         occurring once — on one 4,000-line payload, `[×3980]` with 3,979 \
         distinct lines destroyed. `aggressive` now says what left instead of \
         counting it, so the marker is longer and the output is a little \
         bigger. Only `aggressive` moved; the other two modes still match.",
    ),
    (
        "9011b5a39be4d174",
        "aggressive",
        "Recorded when a fold in a mode that retains nothing emitted a bare \
         `[×N]`. That count says the kept line occurred N times; these \
         payloads fold lines that merely share a template, so it named a line \
         occurring once — on one 4,000-line payload, `[×3980]` with 3,979 \
         distinct lines destroyed. `aggressive` now says what left instead of \
         counting it, so the marker is longer and the output is a little \
         bigger. Only `aggressive` moved; the other two modes still match.",
    ),
    (
        "9a56f10ed9c2d8a3",
        "aggressive",
        "Recorded when a fold in a mode that retains nothing emitted a bare \
         `[×N]`. That count says the kept line occurred N times; these \
         payloads fold lines that merely share a template, so it named a line \
         occurring once — on one 4,000-line payload, `[×3980]` with 3,979 \
         distinct lines destroyed. `aggressive` now says what left instead of \
         counting it, so the marker is longer and the output is a little \
         bigger. Only `aggressive` moved; the other two modes still match.",
    ),
    (
        "build_diagnostics_gcc",
        "aggressive",
        "Recorded when a fold in a mode that retains nothing emitted a bare \
         `[×N]`. That count says the kept line occurred N times; these \
         payloads fold lines that merely share a template, so it named a line \
         occurring once — on one 4,000-line payload, `[×3980]` with 3,979 \
         distinct lines destroyed. `aggressive` now says what left instead of \
         counting it, so the marker is longer and the output is a little \
         bigger. Only `aggressive` moved; the other two modes still match.",
    ),
    (
        "e9a8fd488484d4cf",
        "aggressive",
        "Recorded when a fold in a mode that retains nothing emitted a bare \
         `[×N]`. That count says the kept line occurred N times; these \
         payloads fold lines that merely share a template, so it named a line \
         occurring once — on one 4,000-line payload, `[×3980]` with 3,979 \
         distinct lines destroyed. `aggressive` now says what left instead of \
         counting it, so the marker is longer and the output is a little \
         bigger. Only `aggressive` moved; the other two modes still match.",
    ),
    (
        "f9d2233679f32583",
        "aggressive",
        "Recorded when a fold in a mode that retains nothing emitted a bare \
         `[×N]`. That count says the kept line occurred N times; these \
         payloads fold lines that merely share a template, so it named a line \
         occurring once — on one 4,000-line payload, `[×3980]` with 3,979 \
         distinct lines destroyed. `aggressive` now says what left instead of \
         counting it, so the marker is longer and the output is a little \
         bigger. Only `aggressive` moved; the other two modes still match.",
    ),
    (
        "harness_preamble_log",
        "aggressive",
        "Recorded when a fold in a mode that retains nothing emitted a bare \
         `[×N]`. That count says the kept line occurred N times; these \
         payloads fold lines that merely share a template, so it named a line \
         occurring once — on one 4,000-line payload, `[×3980]` with 3,979 \
         distinct lines destroyed. `aggressive` now says what left instead of \
         counting it, so the marker is longer and the output is a little \
         bigger. Only `aggressive` moved; the other two modes still match.",
    ),
    (
        "json_nested_long_string",
        "aggressive",
        "Recorded when a fold in a mode that retains nothing emitted a bare \
         `[×N]`. That count says the kept line occurred N times; these \
         payloads fold lines that merely share a template, so it named a line \
         occurring once — on one 4,000-line payload, `[×3980]` with 3,979 \
         distinct lines destroyed. `aggressive` now says what left instead of \
         counting it, so the marker is longer and the output is a little \
         bigger. Only `aggressive` moved; the other two modes still match.",
    ),
    (
        "log_apt_untimestamped",
        "aggressive",
        "Recorded when a fold in a mode that retains nothing emitted a bare \
         `[×N]`. That count says the kept line occurred N times; these \
         payloads fold lines that merely share a template, so it named a line \
         occurring once — on one 4,000-line payload, `[×3980]` with 3,979 \
         distinct lines destroyed. `aggressive` now says what left instead of \
         counting it, so the marker is longer and the output is a little \
         bigger. Only `aggressive` moved; the other two modes still match.",
    ),
    (
        "log_timestamped",
        "aggressive",
        "Recorded when a fold in a mode that retains nothing emitted a bare \
         `[×N]`. That count says the kept line occurred N times; these \
         payloads fold lines that merely share a template, so it named a line \
         occurring once — on one 4,000-line payload, `[×3980]` with 3,979 \
         distinct lines destroyed. `aggressive` now says what left instead of \
         counting it, so the marker is longer and the output is a little \
         bigger. Only `aggressive` moved; the other two modes still match.",
    ),
    (
        "mixed_log_and_prose",
        "aggressive",
        "Recorded when a fold in a mode that retains nothing emitted a bare \
         `[×N]`. That count says the kept line occurred N times; these \
         payloads fold lines that merely share a template, so it named a line \
         occurring once — on one 4,000-line payload, `[×3980]` with 3,979 \
         distinct lines destroyed. `aggressive` now says what left instead of \
         counting it, so the marker is longer and the output is a little \
         bigger. Only `aggressive` moved; the other two modes still match.",
    ),
];

/// Payloads this detector classifies differently from the vectors, and why.
///
/// Separate from [`KNOWN_DELTAS`] because the two tests ask different
/// questions: that one is about the bytes a payload compresses to, this one is
/// about the name the detector gives it, and a change to the name that leaves
/// the bytes alone has to be explainable in exactly one of them.
const KNOWN_DETECTION_DELTAS: &[(&str, &str)] = &[
    ("17b15ad93c8d6a74", DISASSEMBLY_SHIPS_WHOLE),
    ("21e8d47881da76c9", DISASSEMBLY_SHIPS_WHOLE),
    ("48bedede9df6b003", DISASSEMBLY_SHIPS_WHOLE),
    ("dae24a04ef823b86", DISASSEMBLY_SHIPS_WHOLE),
    ("dd9ba3749d237e37", DISASSEMBLY_SHIPS_WHOLE),
    (
        "d0175da218a5e184",
        "A JSON document behind a harness frame. The vectors were recorded by a \
     detector that ran the JSON test on the framed bytes, where it gives up on \
     the first byte — a payload that opens `Chunk ID:` does not open `{`. So a \
     valid document was classified `text`. It is `json_array` now, which is \
     what it is: strip the five frame lines and it parses. The compressed \
     bytes are unchanged, here and across the corpus, because the mixed \
     segmenter was already finding the JSON span inside it — this is the \
     detector telling the truth rather than a compressor doing more.",
    ),
];

fn known_detection_delta(content_id: &str) -> Option<&'static str> {
    KNOWN_DETECTION_DELTAS
        .iter()
        .find(|(id, _)| *id == content_id)
        .map(|(_, reason)| *reason)
}

fn known_delta(content_id: &str, mode: &str) -> Option<&'static str> {
    KNOWN_DELTAS
        .iter()
        .find(|(id, m, _)| *id == content_id && (*m == "*" || *m == mode))
        .map(|(_, _, reason)| *reason)
}

/// The engine configuration the vectors were recorded under.
///
/// Every flag named here is one whose default has since moved. Spelling them out
/// is the point: it keeps the list of deliberate behaviour changes visible in
/// the one place that would otherwise silently absorb them.
fn recorded_options(mode: Mode) -> EngineOptions {
    EngineOptions {
        mode,
        // factoring ran on the lossless path only
        enable_lossy_key_factoring: false,
        // lossless mode was allowed to elide search, diff, table and code
        strict_lossless: false,
        // a claimed-then-declined payload was returned whole
        enable_fold_fallback: false,
        // off by default anyway; named so the harness states what it assumes
        enable_preamble_aware_detection: false,
        // a fold over variants was announced as a plain repeat
        fold_marks_variants: false,
        // the line-number column was classified along with the content
        enable_line_gutter: false,
        // there was no last-resort clip for over-long lines
        max_line_chars: 0,
        // a line-numbered payload was classified by its numbers, so `grep -n`
        // over source read as YAML config. Ten of these vectors reclassify with
        // it on and not one of them compresses differently — the corpus figure
        // is identical to four decimals — but the Harbor payloads that motivated
        // it go from 0% to 53.7%.
        enable_gutter_aware_detection: false,
        // build output folded by value only, so a line differing from its
        // neighbours in a package name was a line of its own. Twenty-one of
        // these vectors compress 36% further with it on, and none compress
        // less; the vectors record the old reading, and the corpus bench
        // measures the new one.
        enable_build_templates: false,
        // a fold over byte-identical lines stored the repeats behind a handle,
        // although `[×N]` already reconstructs them. Measured on a Harbor run,
        // that was 241 of 275 stored fragments and 88% of the markers the model
        // saw, none ever fetched. The vectors record the old reading.
        fold_handles_identical_repeats: true,
        // HTML had no compressor: `strategy_for` had no arm for it and every
        // HTML payload was returned whole. Five of these vectors compress with
        // the route on — corpus coverage 63.15% -> 64.31% — and one live codex
        // page went from 0.00% to 94.46%. The vectors record the old reading.
        enable_html_compressor: false,
        // a plain-text fold matched lines exactly and nothing else, so
        // generated text — a markdown guide, a declined INI — folded barely or
        // not at all. Corpus coverage 64.31% -> 68.72% with it on.
        enable_normalised_text_fold: false,
        // the pipe-table rule had a count and no ratio, so three rows anywhere
        // claimed the payload: nine of these vectors are prose with a small
        // table in them, at 6% to 23% of their lines. Corpus coverage
        // 68.78% -> 69.95% with the ratio on.
        enable_tabular_ratio: false,
        // a file whose bodies all sat under `code_max_body_lines` returned
        // whole; two of these vectors are that shape and now compress.
        enable_code_budget: false,
        ..Default::default()
    }
}

/// Undo the one cosmetic difference from the vectors: a Python body elision
/// leaves `...` where the generator left `pass`.
///
/// Both parse, and the reason for the change is what they mean to a reader —
/// `pass` is a body that deliberately does nothing, so the stub reads as
/// finished code, while `...` is what a `.pyi` stub puts where a body is not
/// shown. It is not worth a configuration flag, and it is worth the harness
/// staying strict about everything else, so it is reconciled here instead.
///
/// The rewrite is deliberately narrow: only a line that is nothing but `...`,
/// and only directly under a body-elision marker. Anything it fails to match
/// stays different and is reported, so this can make the comparison fail where
/// it would have passed but never the other way round.
fn as_recorded(output: &str) -> String {
    let lines: Vec<&str> = output.split('\n').collect();
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let under_a_marker = i
                .checked_sub(1)
                .and_then(|p| lines.get(p))
                .is_some_and(|p| p.contains("body lines omitted from"));
            if under_a_marker && line.trim() == "..." {
                line.replace("...", "pass")
            } else {
                (*line).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn mode_of(name: &str) -> Mode {
    match name {
        "aggressive" => Mode::Aggressive,
        "lossless" => Mode::Lossless,
        "recoverable" => Mode::Recoverable,
        other => panic!("unknown mode {other}"),
    }
}

/// Every payload in the vectors is classified the way the vectors record.
///
/// Detection is checked separately from compression because it is upstream of
/// everything: a routing difference explains every output difference that
/// follows from it, and reading one failure is easier than reading fifty.
#[test]
fn detection_matches_the_recorded_vectors() {
    let records = load();
    let mut mismatches = Vec::new();
    for record in &records {
        let opts = recorded_options(Mode::Recoverable);
        let detection = detect_result(&record.content, &record.role, &opts);
        let got = detection.content_type.as_str();
        // below the size gate the vectors record plain text with a skip flag;
        // `detect_result` does the same, so the comparison is like for like
        if got != record.detected.content_type {
            if known_detection_delta(&record.content_id).is_none() {
                mismatches.push(format!(
                    "{}: recorded {} got {got}",
                    record.content_id, record.detected.content_type
                ));
            }
            continue;
        }
        let language = detection.meta.language.map(|l| l.as_str().to_string());
        if language != record.detected.language {
            mismatches.push(format!(
                "{}: language recorded {:?} got {language:?}",
                record.content_id, record.detected.language
            ));
        }
        let flavor = detection.meta.flavor.map(|f| f.as_str().to_string());
        if flavor != record.detected.flavor {
            mismatches.push(format!(
                "{}: flavor recorded {:?} got {flavor:?}",
                record.content_id, record.detected.flavor
            ));
        }
        if detection.meta.uniform_stream != record.detected.uniform_stream {
            mismatches.push(format!("{}: uniform_stream differs", record.content_id));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} of {} payloads classified differently:\n{}",
        mismatches.len(),
        records.len(),
        mismatches.join("\n")
    );
}

/// The compressed bytes, the reported kind, and the handles minted.
///
/// Handles are content-addressed, so equal handles prove the retained fragments
/// are equal too, without the vectors having to carry them.
/// Skipped without the grammars, and that is not a gap being papered over: the
/// vectors were recorded with every grammar available, so a build whose code
/// compressor declines everything would be compared against a snapshot that did
/// not. The invariants below still run in both builds.
#[cfg_attr(
    not(feature = "code-aware"),
    ignore = "golden vectors were recorded with grammars available"
)]
#[test]
fn compression_matches_the_recorded_vectors() {
    let records = load();
    let mut mismatches: Vec<(String, String, String)> = Vec::new();
    let mut compared = 0usize;

    for record in &records {
        for (mode_name, expected) in &record.modes {
            let mode = mode_of(mode_name);
            let opts = recorded_options(mode);
            let mut refs = opts.collector();
            let ctx = Context {
                role: &record.role,
                preceding_user_text: &record.preceding_user_text,
                source_tool: (!record.source_tool.is_empty())
                    .then_some(record.source_tool.as_str()),
                ..Default::default()
            };
            let mut applied: Vec<Kind> = Vec::new();
            let (output, kind) = compress_content(
                &record.content,
                &record.role,
                &opts,
                None,
                &mut refs,
                &ctx,
                &mut applied,
            );
            compared += 1;

            let kind_name = kind.map(|k| k.as_str().to_string());
            if kind_name != expected.kind {
                mismatches.push((
                    record.content_id.clone(),
                    mode_name.to_string(),
                    format!(
                        "{} [{mode_name}]: kind recorded {:?} got {kind_name:?}",
                        record.content_id, expected.kind
                    ),
                ));
            }
            if as_recorded(&output) != expected.output {
                mismatches.push((
                    record.content_id.clone(),
                    mode_name.to_string(),
                    format!(
                        "{} [{mode_name}]: output differs (recorded {} chars, got {})\n  \
                         recorded tail: {:?}\n  ours tail:      {:?}",
                        record.content_id,
                        expected.output.chars().count(),
                        output.chars().count(),
                        tail(&expected.output),
                        tail(&output),
                    ),
                ));
                continue;
            }
            let got_refs: Vec<String> = refs.pairs().iter().map(|(id, _)| id.to_string()).collect();
            if got_refs != expected.refs {
                mismatches.push((
                    record.content_id.clone(),
                    mode_name.to_string(),
                    format!(
                        "{} [{mode_name}]: handles recorded {:?} got {got_refs:?}",
                        record.content_id, expected.refs
                    ),
                ));
            }
        }
    }

    let (explained, unexplained): (Vec<_>, Vec<_>) = mismatches
        .iter()
        .partition(|m| known_delta(&m.0, &m.1).is_some());

    assert!(
        unexplained.is_empty(),
        "{} of {compared} comparisons differ with no recorded reason:\n{}",
        unexplained.len(),
        unexplained
            .iter()
            .take(25)
            .map(|m| m.2.clone())
            .collect::<Vec<_>>()
            .join("\n")
    );

    // A recorded delta that stops happening is also news: it means the reason is
    // stale and the entry should go, rather than silently protecting a future
    // regression on the same payload.
    for (id, mode, _) in KNOWN_DELTAS {
        assert!(
            explained
                .iter()
                .any(|m| m.0 == *id && (*mode == "*" || m.1 == *mode)),
            "{id} [{mode}] is recorded as a known difference but now matches the \
             vectors; remove the entry"
        );
    }
}

fn tail(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let start = chars.len().saturating_sub(180);
    chars[start..].iter().collect()
}

/// The invariants that make a compression ratio meaningful at all.
///
/// Checked over every corpus payload in every mode, and not tradeable against
/// ratio: a run that improves the ratio while breaking one of these is a
/// regression, not an improvement.
#[test]
fn no_safety_invariant_is_violated() {
    let records = load();
    let mut violations = Vec::new();

    for record in &records {
        for mode in [Mode::Aggressive, Mode::Lossless, Mode::Recoverable] {
            let opts = EngineOptions {
                mode,
                ..Default::default()
            };
            let mut refs = opts.collector();
            let ctx = Context::for_role(&record.role);
            let mut applied = Vec::new();
            let (output, kind) = compress_content(
                &record.content,
                &record.role,
                &opts,
                None,
                &mut refs,
                &ctx,
                &mut applied,
            );
            let id = format!("{} [{mode}]", record.content_id);

            if output.chars().count() > record.content.chars().count() {
                violations.push(format!("{id}: longer than input"));
            }
            if kind.is_none() && output.as_ref() != record.content {
                violations.push(format!("{id}: mutated while declining"));
            }
            for (handle, fragment) in refs.pairs() {
                if fragment.is_empty() {
                    violations.push(format!("{id}: empty retained fragment"));
                }
                // a handle stored but never referenced is a write nothing can
                // ever read back
                if !output.contains(&format!("ref:{handle}")) {
                    violations.push(format!("{id}: orphaned handle {handle}"));
                }
            }
            if kind.is_some()
                && !refs.is_empty()
                && !kompressor_engine::markers::has_marker(&output)
            {
                violations.push(format!("{id}: retained fragments with no marker"));
            }

            // Byte instability would burst the provider prompt cache on every
            // turn, which costs more than any ratio win it could buy.
            let mut again_refs = opts.collector();
            let mut again_applied = Vec::new();
            let (again, again_kind) = compress_content(
                &record.content,
                &record.role,
                &opts,
                None,
                &mut again_refs,
                &ctx,
                &mut again_applied,
            );
            if again != output || again_kind != kind {
                violations.push(format!("{id}: not deterministic"));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "{} invariant violations:\n{}",
        violations.len(),
        violations
            .iter()
            .take(25)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// A payload's compression must not depend on the question that prompted it.
///
/// Scoring a payload against the surrounding conversation looks like an
/// improvement and is a cache-buster: an agent re-sends its history every turn
/// under a different latest question, so query-dependent bytes change a prefix
/// the provider is caching. The trade was measured and lost, so the one
/// query-aware ranking the engine had was removed; this keeps it removed.
#[test]
fn no_payload_is_query_dependent() {
    const QUERIES: [&str; 3] = [
        "find the failing test and fix it",
        "summarise the build errors in this log",
        "which files reference the parser module?",
    ];
    let records = load();
    let opts = EngineOptions::default();
    let mut dependent = Vec::new();

    for record in &records {
        let mut baseline: Option<String> = None;
        for query in QUERIES {
            let mut refs = opts.collector();
            let ctx = Context {
                role: &record.role,
                preceding_user_text: query,
                ..Default::default()
            };
            let mut applied = Vec::new();
            let (output, _) = compress_content(
                &record.content,
                &record.role,
                &opts,
                None,
                &mut refs,
                &ctx,
                &mut applied,
            );
            match &baseline {
                None => baseline = Some(output.into_owned()),
                Some(first) if *first != output.as_ref() => {
                    dependent.push(record.content_id.clone());
                    break;
                }
                _ => {}
            }
        }
    }

    assert!(
        dependent.is_empty(),
        "{} payloads changed with the question: {:?}",
        dependent.len(),
        &dependent[..dependent.len().min(10)]
    );
}

/// Detection is also checked directly, so a size-gate change cannot silently
/// mask a routing regression.
#[test]
fn detection_is_stable_under_repetition() {
    let records = load();
    for record in records.iter().take(60) {
        let first = detect_content(&record.content).content_type;
        for _ in 0..3 {
            assert_eq!(
                detect_content(&record.content).content_type,
                first,
                "{} classified inconsistently",
                record.content_id
            );
        }
    }
}
