//! The Headroom borrows (docs/HEADROOM-COMPARISON.md, section 3.7): runtime-frame collapse,
//! test totals kept, and the typed-header CSV form of a JSON array — held to the
//! engine's invariants in every mode, and the CSV form measured against key
//! factoring on the JSON the corpora hold.

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;

use kompressor_engine::EngineOptions;
use kompressor_engine::crusher::crush_json;
use kompressor_engine::detect::detect_content;
use kompressor_engine::markers::has_marker;
use kompressor_engine::pipeline::compress_content;
use kompressor_engine::types::{ContentType, Context, ForcedType, Mode};

const MODES: [Mode; 3] = [Mode::Aggressive, Mode::Lossless, Mode::Recoverable];

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

/// Every payload of the committed corpus.
fn corpus() -> Vec<(String, String)> {
    let file = std::fs::File::open(fixture("corpus.jsonl.gz")).expect("corpus committed");
    BufReader::new(flate2::read::GzDecoder::new(file))
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(&line.unwrap()).unwrap())
        .filter_map(|row| {
            Some((
                row.get("content_id")?.as_str()?.to_string(),
                row.get("content")?.as_str()?.to_string(),
            ))
        })
        .collect()
}

/// Every payload of the golden vectors, the hand-written micro payloads among
/// them: the only JSON arrays either corpus holds.
fn golden() -> Vec<(String, String)> {
    let file = std::fs::File::open(fixture("golden.json.gz")).expect("vectors committed");
    let mut text = String::new();
    flate2::read::GzDecoder::new(file)
        .read_to_string(&mut text)
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    value["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["content_id"].as_str().unwrap().to_string(),
                r["content"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn python_trace() -> String {
    let mut lines = vec![
        "Running the import job".to_string(),
        "Traceback (most recent call last):".to_string(),
        "  File \"/app/job.py\", line 88, in <module>".to_string(),
        "    main()".to_string(),
    ];
    for n in 0..14 {
        lines.push(format!(
            "  File \"/app/.venv/lib/python3.12/site-packages/click/core.py\", line {}, in invoke",
            1000 + n
        ));
        lines.push("    return ctx.invoke(self.callback, **ctx.params)".to_string());
        lines.push("           ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^".to_string());
    }
    lines.push("  File \"/app/importer.py\", line 12, in load".to_string());
    lines.push("    rows = parse(path)".to_string());
    lines.push(
        "  File \"/usr/lib/python3.12/json/decoder.py\", line 355, in raw_decode".to_string(),
    );
    lines
        .push("    raise JSONDecodeError(\"Expecting value\", s, err.value) from None".to_string());
    lines.push(
        "json.decoder.JSONDecodeError: Expecting value: line 1 column 1 (char 0)".to_string(),
    );
    lines.join("\n")
}

/// A test run whose warnings outnumber the scorer's budget. Each is distinct,
/// so each scores as a first warning, and ties go to the earliest line — the
/// totals line, scored as a summary, comes after all of them, and the assertion
/// carries no error word at all.
fn pytest_run() -> String {
    let mut lines = vec!["collected 240 items".to_string()];
    for n in 0..120 {
        lines.push(format!(
            "WARNING tests/test_m{n}.py:{n}: helper_{n}() is deprecated"
        ));
    }
    lines.push("E       assert 77 == 78".into());
    for n in 0..40 {
        lines.push(format!("DEBUG worker {n} flushed queue depth={n}"));
    }
    lines.push("================== 239 passed, 120 warnings in 12.04s ==================".into());
    for n in 0..30 {
        lines.push(format!("teardown: released fixture {n}"));
    }
    lines.join("\n")
}

fn json_records() -> String {
    let rows: Vec<serde_json::Value> = (0..9)
        .map(|i| serde_json::json!({"id": i, "path": format!("/api/v1/items/{i}"), "ok": true, "ms": 10 + i}))
        .collect();
    serde_json::to_string_pretty(&rows).unwrap()
}

fn compress(content: &str, opts: &EngineOptions) -> (String, bool, Vec<(String, String)>) {
    let mut refs = opts.collector();
    let mut applied = Vec::new();
    let ctx = Context::for_role("tool");
    let (out, kind) = compress_content(content, "tool", opts, None, &mut refs, &ctx, &mut applied);
    let pairs = refs
        .pairs()
        .iter()
        .map(|(id, fragment)| (id.to_string(), fragment.clone()))
        .collect();
    (out.into_owned(), kind.is_some(), pairs)
}

/// Never larger, deterministic, every handle named and every name held, and
/// lossless keeping every frame — over the golden payloads (233 from the
/// corpus, and every hand-written one) and the shapes the new rules target,
/// with the CSV form switched on. The whole corpus, under the defaults, is
/// `bench replay --check-determinism`'s.
#[test]
fn the_invariants_hold_with_every_borrow_on_in_every_mode() {
    let mut payloads = golden();
    payloads.push(("python_trace".into(), python_trace()));
    payloads.push(("pytest_run".into(), pytest_run()));
    payloads.push(("json_records".into(), json_records()));
    let mut violations = Vec::new();
    for (id, content) in &payloads {
        for mode in MODES {
            let opts = EngineOptions {
                mode,
                enable_json_csv: true,
                ..EngineOptions::default()
            };
            let (out, compressed, pairs) = compress(content, &opts);
            let id = format!("{id} [{mode}]");
            if out.chars().count() > content.chars().count() {
                violations.push(format!("{id}: longer than input"));
            }
            if !compressed && out != *content {
                violations.push(format!("{id}: mutated while declining"));
            }
            for (handle, fragment) in &pairs {
                if !out.contains(&format!("ref:{handle}")) {
                    violations.push(format!("{id}: orphaned handle {handle}"));
                }
                if kompressor_engine::markers::is_bare_marker(fragment) {
                    violations.push(format!("{id}: a bare marker stored as {handle}"));
                }
            }
            if mode != Mode::Recoverable && !pairs.is_empty() {
                violations.push(format!("{id}: {mode} retained a fragment"));
            }
            if mode == Mode::Lossless && out.contains("library frames omitted") {
                violations.push(format!("{id}: lossless collapsed frames"));
            }
            if compress(content, &opts).0 != out {
                violations.push(format!("{id}: not deterministic"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "{} violations:\n{}",
        violations.len(),
        violations.join("\n")
    );
}

#[test]
fn a_traceback_keeps_its_own_frames_and_collapses_the_library() {
    let content = python_trace();
    for mode in [Mode::Recoverable, Mode::Aggressive] {
        let opts = EngineOptions {
            mode,
            ..EngineOptions::default()
        };
        let (out, compressed, pairs) = compress(&content, &opts);
        assert!(compressed, "{mode}");
        assert!(
            out.contains("[... 14 library frames omitted"),
            "{mode}: {out}"
        );
        for kept in [
            "File \"/app/job.py\", line 88",
            "File \"/app/importer.py\", line 12",
            "json/decoder.py\", line 355",
            "JSONDecodeError: Expecting value",
        ] {
            assert!(out.contains(kept), "{mode}: {kept} in {out}");
        }
        assert_eq!(pairs.is_empty(), mode == Mode::Aggressive);
    }
    let lossless = EngineOptions {
        mode: Mode::Lossless,
        ..EngineOptions::default()
    };
    assert!(!compress(&content, &lossless).0.contains("library frames"));
    let off = EngineOptions {
        enable_runtime_frame_collapse: false,
        ..EngineOptions::default()
    };
    assert!(!compress(&content, &off).0.contains("library frames"));
}

/// The log scorer keeps a bounded budget and the tail; the totals line sits
/// thirty lines from the end, behind more equally scored warnings than the
/// budget holds, so without the rule the scorer drops it and the assertion.
#[test]
fn a_test_run_keeps_its_totals_and_first_assertion() {
    let content = pytest_run();
    let totals = "239 passed, 120 warnings in 12.04s";
    let assertion = "E       assert 77 == 78";
    let ctx = Context::for_role("tool");
    let run = |opts: &EngineOptions| {
        let mut refs = opts.collector();
        let (out, kind) = compress_content(
            &content,
            "tool",
            opts,
            Some(ForcedType::Log),
            &mut refs,
            &ctx,
            &mut Vec::new(),
        );
        assert!(kind.is_some());
        out.into_owned()
    };
    for mode in [Mode::Recoverable, Mode::Aggressive] {
        let opts = EngineOptions {
            mode,
            keep_tail_lines: 5,
            ..EngineOptions::default()
        };
        let out = run(&opts);
        assert!(has_marker(&out));
        assert!(out.contains(totals), "{mode}: {out}");
        assert!(out.contains(assertion), "{mode}: {out}");

        let off = run(&EngineOptions {
            keep_test_totals: false,
            ..opts
        });
        assert!(
            !off.contains(totals) && !off.contains(assertion),
            "{mode}: without the rule the scorer drops both, which is what \
             this test needs to show: {off}"
        );
    }
}

/// HEADROOM-PLAN Tier 7a: the CSV form is adopted only if it beats key
/// factoring on the JSON part of the corpus. Every payload either corpus holds
/// that detects as JSON is run both ways, in `lossless` (where key factoring is
/// the whole of the JSON path) and in `recoverable` (the deployed mode, where
/// sampling competes), and the totals are printed for the record.
///
/// Measured 2026-11-25 over both corpora: 8 payloads detect as JSON, 16,041
/// characters. `lossless`: 10,164 with key factoring, 9,465 with the CSV form
/// (taken on 3, all hand-written golden micro payloads: 816 -> 583, 1,000 ->
/// 767, 961 -> 728). `recoverable`: 6,469 either way — sampling is smaller
/// than any lossless form, so the CSV form is never taken. The committed
/// corpus of real traffic holds no JSON array at all, so the win exists only
/// in `lossless` and only on synthetic payloads; `enable_json_csv` ships off.
#[test]
fn csv_against_key_factoring_on_the_corpus() {
    let mut payloads = corpus();
    payloads.extend(golden());
    let ctx = Context::for_role("tool");
    for mode in [Mode::Lossless, Mode::Recoverable] {
        let factoring = EngineOptions {
            mode,
            ..EngineOptions::default()
        };
        let csv = EngineOptions {
            enable_json_csv: true,
            ..factoring.clone()
        };
        let (mut json_payloads, mut chars_in, mut by_factoring, mut by_csv, mut csv_taken) =
            (0usize, 0usize, 0usize, 0usize, 0usize);
        for (id, content) in &payloads {
            if detect_content(content).content_type != ContentType::JsonArray {
                continue;
            }
            let factored = crush_json(content, &factoring, &ctx, &mut factoring.collector());
            let tabled = crush_json(content, &csv, &ctx, &mut csv.collector());
            json_payloads += 1;
            let len = |out: &Option<String>| out.as_deref().unwrap_or(content).chars().count();
            chars_in += content.chars().count();
            by_factoring += len(&factored);
            by_csv += len(&tabled);
            if tabled
                .as_deref()
                .is_some_and(|out| out.contains("[json array as csv; "))
            {
                csv_taken += 1;
                eprintln!(
                    "{mode}: csv taken: {id} {} -> {}",
                    len(&factored),
                    len(&tabled)
                );
            }
            assert!(
                len(&tabled) <= len(&factored),
                "{mode} {id}: the CSV form is only ever taken when it is smaller"
            );
        }
        eprintln!(
            "{mode}: JSON payloads {json_payloads}: in {chars_in}, without CSV {by_factoring}, \
             with CSV {by_csv} ({csv_taken} taken)"
        );
        assert!(json_payloads > 0, "the golden micro payloads hold JSON");
    }
}
