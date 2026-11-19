//! Regressions from the November 2026 review of the compressors.
//!
//! Each test states the property the compressor is supposed to keep and the
//! smallest payload found that broke it. They share one shape: something was
//! removed from a payload without being put behind a handle, or something that
//! was kept came back altered — the two things a `recoverable` compression
//! must never do.

use kompressor_engine::code::crush_code;
use kompressor_engine::configs::crush_config;
use kompressor_engine::detect::detect_content;
use kompressor_engine::diff::crush_diff;
use kompressor_engine::gutter::split_line_gutter;
use kompressor_engine::mixed::crush_mixed;
use kompressor_engine::tabular::crush_tabular;
use kompressor_engine::{Context, EngineOptions, Language, RefCollector};

fn collector(opts: &EngineOptions) -> RefCollector {
    opts.collector()
}

/// Everything the engine kept or stored: the output and every fragment.
fn recoverable_text(out: &str, refs: &RefCollector) -> String {
    let mut all = out.to_string();
    for (_, fragment) in refs.pairs() {
        all.push('\n');
        all.push_str(fragment);
    }
    all
}

// ------------------------------------------------------------------- code

/// An arrow function whose body is an *expression* that happens to contain
/// braces (a curried thunk) is not a block. The body node was replaced whole
/// while only the text between its first `{` and last `}` was stored, so
/// `async (dispatch) =>` was deleted and recoverable from nowhere.
#[cfg(feature = "code-aware")]
#[test]
fn an_expression_bodied_arrow_function_loses_nothing() {
    let opts = EngineOptions::default();
    let mut src = String::from("export const fetchUser = (id) => async (dispatch) => {\n");
    for i in 0..60 {
        src.push_str(&format!("  dispatch({{ type: 'step{i}', id }});\n"));
    }
    src.push_str("};\n");
    let mut refs = collector(&opts);
    let out = crush_code(
        &src,
        &opts,
        &Context::default(),
        &mut refs,
        Some(Language::TypeScript),
    );
    let Some(out) = out else {
        return; // declining is always allowed
    };
    let all = recoverable_text(&out, &refs);
    assert!(
        all.contains("async (dispatch) =>"),
        "the inner arrow head is in neither the output nor a fragment:\n{out}"
    );
    for i in [0, 30, 59] {
        assert!(all.contains(&format!("'step{i}'")), "step{i} is gone");
    }
}

/// The same for a call wrapped round an object literal: `foo(a, {…}, c)`.
#[cfg(feature = "code-aware")]
#[test]
fn an_arrow_function_returning_a_call_keeps_the_call() {
    let opts = EngineOptions::default();
    let mut src = String::from("export const build = (a, c) => register(a, {\n");
    for i in 0..60 {
        src.push_str(&format!("  key{i}: compute(a, {i}),\n"));
    }
    src.push_str("}, c);\n");
    let mut refs = collector(&opts);
    let Some(out) = crush_code(
        &src,
        &opts,
        &Context::default(),
        &mut refs,
        Some(Language::TypeScript),
    ) else {
        return;
    };
    let all = recoverable_text(&out, &refs);
    assert!(
        all.contains("register(a, {"),
        "the call head is gone:\n{out}"
    );
    assert!(all.contains("}, c);"), "the call tail is gone:\n{out}");
}

/// A form feed is a line break to `split_lines` and not to tree-sitter, whose
/// rows count `\n` only. Each one before a function shifted the elided span up
/// a line: the `def` went behind the marker and the last body line stayed.
#[cfg(feature = "code-aware")]
#[test]
fn a_form_feed_before_a_python_function_does_not_shift_what_is_elided() {
    let opts = EngineOptions::default();
    let mut src = String::from("def prev():\n    return 1\n\x0c\ndef target():\n");
    for i in 0..40 {
        src.push_str(&format!("    x{i} = compute({i})\n"));
    }
    src.push_str("    return x0\n");
    let mut refs = collector(&opts);
    let Some(out) = crush_code(
        &src,
        &opts,
        &Context::default(),
        &mut refs,
        Some(Language::Python),
    ) else {
        return;
    };
    assert!(
        out.contains("def target():"),
        "the signature was elided:\n{out}"
    );
    assert!(
        !out.contains("\n    return x0"),
        "the last body line was left outside the elision:\n{out}"
    );
    let all = recoverable_text(&out, &refs);
    assert!(all.contains("    return x0"));
    assert!(all.contains("    x0 = compute(0)"));
}

// ----------------------------------------------------------------- detect

/// A payload whose lines open with a long run of `1:1:1:…` strips one gutter
/// level per recursion. Unbounded, that is a frame and a copy of the body per
/// level, and a stack overflow is an abort `catch_unwind` cannot turn into
/// "forward the original".
#[test]
fn nested_line_number_prefixes_do_not_recurse_per_level() {
    let content: String = (1..=6)
        .map(|i| format!("{}x\n", format!("{i}:").repeat(20_000)))
        .collect();
    // On a small stack, so that depth proportional to the line length cannot
    // pass by luck. One lift needs a few kilobytes; twenty thousand would not
    // fit in megabytes.
    let handle = std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(move || {
            let started = std::time::Instant::now();
            let _ = detect_content(&content);
            started.elapsed()
        })
        .unwrap();
    let elapsed = handle
        .join()
        .expect("detection must not overflow its stack");
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "detection took {elapsed:?}"
    );
}

// ----------------------------------------------------------------- gutter

/// A line that survives compression must come back with *its own* number or
/// with none. Matching "the first identical line at or after the cursor" gave
/// a kept `}` the number of an earlier `}` that had been omitted.
#[test]
fn a_kept_line_never_takes_the_number_of_an_omitted_duplicate() {
    let gutter = split_line_gutter("1\talpha\n2\t}\n3\tbeta\n4\t}\n5\tgamma\n").unwrap();
    let out = gutter.reattach("alpha\n[... 2 lines omitted]\n}\ngamma\n");
    assert!(
        !out.contains("2\t}"),
        "line 4 was numbered as line 2, which was omitted:\n{out}"
    );
    assert!(out.contains("1\talpha"));
    assert!(out.contains("5\tgamma"));
    // Its true number, or bare — never a wrong one.
    assert!(out.contains("\n4\t}\n") || out.contains("\n}\n"), "{out}");
}

/// The unambiguous case still numbers every line it kept.
#[test]
fn unambiguous_lines_after_an_omission_keep_their_numbers() {
    let gutter = split_line_gutter("1\talpha\n2\tbeta\n3\tgamma\n4\tdelta\n5\tomega\n").unwrap();
    let out = gutter.reattach("alpha\n[... 2 lines omitted]\ndelta\nomega\n");
    assert_eq!(out, "1\talpha\n[... 2 lines omitted]\n4\tdelta\n5\tomega\n");
}

/// A run of identical lines kept *after* an omission is numbered from its end:
/// the lines before the marker anchor forwards, the lines after it backwards.
#[test]
fn lines_after_the_last_omission_are_numbered_from_the_end() {
    let gutter = split_line_gutter("1\t{\n2\t}\n3\t}\n4\t}\n5\t}\n6\tend\n").unwrap();
    let out = gutter.reattach("{\n[... 2 lines omitted]\n}\n}\nend\n");
    assert_eq!(out, "1\t{\n[... 2 lines omitted]\n4\t}\n5\t}\n6\tend\n");
}

// ------------------------------------------------------------------ mixed

/// Compressing the body of a fenced block must leave the closing fence on a
/// line of its own. The body capture includes the newline before the fence and
/// the compressors return text without one, so the fence was glued to the last
/// line of the body and the block never closed.
#[test]
fn a_compressed_fenced_block_still_closes() {
    let opts = EngineOptions::default();
    let mut body = String::new();
    for i in 0..400 {
        body.push_str(&format!(
            "2026-11-18T10:00:{:02}Z INFO worker processed batch {i} in {}ms\n",
            i % 60,
            10 + i % 7
        ));
    }
    let content = format!("Here is the run:\n\n```\n{body}```\n\nThat is all of it.\n");
    let mut refs = collector(&opts);
    let Some((out, _)) = crush_mixed(&content, &opts, &Context::for_role("tool"), &mut refs) else {
        return;
    };
    assert!(out.len() < content.len());
    assert!(out.starts_with("Here is the run:\n\n```\n"), "{out}");
    assert!(
        out.ends_with("\n```\n\nThat is all of it.\n"),
        "the closing fence is not on its own line:\n…{}",
        &out[out.len().saturating_sub(160)..]
    );
    assert_eq!(out.matches("```").count(), 2);
}

// ---------------------------------------------------------------- configs

/// A block scalar's content is literal text: a `#` line inside it is not a
/// comment. The detector looked for `key: |` and missed the list-item form
/// `- |`, which is how CI files write their scripts.
#[test]
fn a_block_scalar_in_a_list_item_is_not_read_as_comments() {
    let opts = EngineOptions {
        min_compress_chars: 0,
        ..EngineOptions::default()
    };
    let mut content =
        String::from("stages:\n  - build\nbuild:\n  image: alpine\n  script:\n    - |\n");
    for i in 0..12 {
        content.push_str(&format!(
            "      # this hash line is shell text inside the scalar, not yaml {i}\n"
        ));
    }
    content.push_str("      make all\n    - make test\ntest:\n  image: alpine\n");
    for form in ["- |", "- >-", "- |+", "- >2"] {
        let payload = content.replace("- |\n", &format!("{form}\n"));
        let mut refs = collector(&opts);
        let out = crush_config(&payload, &opts, &Context::default(), &mut refs);
        if let Some(out) = out {
            for i in 0..12 {
                assert!(
                    out.contains(&format!("not yaml {i}\n")),
                    "`{form}`: scalar line {i} was elided as a comment:\n{out}"
                );
            }
        }
    }
    // Tags, anchors and a trailing comment on the indicator line as well.
    for head in ["script: !!str |", "script: &run |", "script: | # the build"] {
        let payload = content
            .replace("  script:\n    - |\n", &format!("  {head}\n"))
            .replace("    - make test\n", "");
        let mut refs = collector(&opts);
        if let Some(out) = crush_config(&payload, &opts, &Context::default(), &mut refs) {
            assert!(
                out.contains("not yaml 5\n"),
                "`{head}`: scalar content was elided:\n{out}"
            );
        }
    }
}

// ---------------------------------------------------------------- tabular

/// A quoted cell may contain a newline, so one record can span several lines.
/// Row selection works on lines; on such a table it kept half a record and
/// left a quote open. A table whose records are not its lines is declined.
#[test]
fn a_csv_with_multi_line_cells_is_not_cut_through_a_record() {
    let opts = EngineOptions::default();
    let mut content = String::from("id,note\n");
    for i in 0..60 {
        content.push_str(&format!(
            "{i},\"first half of note {i}\nsecond half of note {i}\"\n"
        ));
    }
    let mut refs = collector(&opts);
    if let Some(out) = crush_tabular(&content, &opts, &Context::default(), &mut refs) {
        assert_eq!(
            out.matches('"').count() % 2,
            0,
            "a record was cut in half, leaving a quote open:\n{out}"
        );
        assert!(
            !out.lines().any(|l| l.starts_with("second half")),
            "the tail of a record was kept without its head:\n{out}"
        );
    }
}

// ------------------------------------------------------------------- diff

/// Inside a hunk every line that starts with `+` or `-` is a change, whatever
/// follows: `+++counter;` adds the C statement `++counter;`. It was taken for
/// neither a change nor a header and trimmed away as "context".
#[test]
fn an_added_line_that_begins_with_plus_plus_is_a_change() {
    let opts = EngineOptions::default();
    let mut diff = String::new();
    diff.push_str("diff --git a/src/count.c b/src/count.c\n--- a/src/count.c\n+++ b/src/count.c\n");
    diff.push_str("@@ -1,40 +1,42 @@\n");
    for i in 0..20 {
        diff.push_str(&format!(" int unchanged_before_{i} = {i};\n"));
    }
    diff.push_str("+++counter;\n");
    diff.push_str("---legacy_counter;\n");
    for i in 0..20 {
        diff.push_str(&format!(" int unchanged_after_{i} = {i};\n"));
    }
    diff.push_str("diff --git a/src/other.c b/src/other.c\n--- a/src/other.c\n+++ b/src/other.c\n");
    diff.push_str("@@ -1,30 +1,31 @@\n");
    for i in 0..15 {
        diff.push_str(&format!(" int other_before_{i} = {i};\n"));
    }
    diff.push_str("+int added = 1;\n");
    for i in 0..15 {
        diff.push_str(&format!(" int other_after_{i} = {i};\n"));
    }
    let mut refs = collector(&opts);
    let Some(out) = crush_diff(&diff, &opts, &Context::default(), &mut refs) else {
        return;
    };
    assert!(out.len() < diff.len());
    assert!(
        out.contains("\n+++counter;\n"),
        "the added line was dropped:\n{out}"
    );
    assert!(
        out.contains("\n---legacy_counter;\n"),
        "the removed line was dropped:\n{out}"
    );
    assert!(out.contains("\n+int added = 1;\n"));
    // The file headers are still headers.
    assert!(out.contains("\n+++ b/src/count.c\n"));
}

// ------------------------------------------------------------- invariants

/// A small deterministic generator: the cases are the same on every run and
/// on every machine, so a failure names a seed that reproduces it.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Pieces chosen to sit on every seam the compressors cut along: each line
/// terminator `split_lines` knows, multi-byte characters, the openers of every
/// content type the detector recognises, and text that looks like a marker.
const PIECES: &[&str] = &[
    "\n",
    "\n",
    "\n",
    "\r\n",
    "\r",
    "\t",
    " ",
    "  ",
    "\x0c",
    "\x0b",
    "\u{2028}",
    "\u{85}",
    "é",
    "日本",
    "🙂",
    "{",
    "}",
    "[",
    "]",
    "\"",
    ":",
    ",",
    "```",
    "```yaml",
    "1:",
    "12\t",
    "+++",
    "---",
    "+++ b/x.rs",
    "--- a/x.rs",
    "@@ -1,3 +1,3 @@",
    "diff --git a/x b/x",
    "| a | b |",
    "|---|---|",
    "key: |",
    "- |",
    "# comment",
    "ERROR failed step ",
    "WARN retry ",
    "def f():",
    "    return 1",
    "fn x() {",
    "=> {",
    "</div>",
    "<div class=\"a\">",
    "a,b,c",
    "\"x\": 1",
    "[... 3 lines omitted]",
    "[×4]",
    "ref:k_AbC123dEf456",
    " omitted",
    "…[+12 chars]",
    "2026-11-18T10:00:00Z INFO worker ",
    "src/main.rs:12:",
    "0123456789",
    "the quick brown fox ",
];

fn payload(rng: &mut Rng) -> String {
    let pieces = 40 + rng.below(1_400);
    // A third of the cases repeat a short run, which is what makes a payload
    // compressible at all; pure noise is returned whole and proves little.
    let repeat = rng.below(3) == 0;
    let run: Vec<&str> = (0..2 + rng.below(6))
        .map(|_| PIECES[rng.below(PIECES.len())])
        .collect();
    let mut out = String::new();
    for i in 0..pieces {
        if repeat && rng.below(4) != 0 {
            out.push_str(run[i % run.len()]);
            if i % run.len() == run.len() - 1 {
                out.push_str(&(i % 97).to_string());
                out.push('\n');
            }
        } else {
            out.push_str(PIECES[rng.below(PIECES.len())]);
        }
    }
    out
}

/// The three invariants the pipeline promises for every payload, plus the one
/// `recoverable` adds, over a few hundred generated payloads and several
/// option sets:
///
/// - it never panics (a panic outside `catch_unwind` takes the request down);
/// - the output is never longer than the input;
/// - the same input gives the same output, byte for byte;
/// - every fragment stored is referenced by the output, and every handle the
///   output gained is one that was stored.
#[test]
fn generated_payloads_never_panic_grow_or_change_between_runs() {
    use kompressor_engine::pipeline::compress_content;
    use kompressor_engine::refs_in;

    let option_sets = [
        EngineOptions::default(),
        EngineOptions {
            min_compress_chars: 0,
            max_ref_bytes: 256,
            max_line_chars: 48,
            ..EngineOptions::default()
        },
        EngineOptions {
            min_compress_chars: 64,
            min_elision_saving_chars: 4_000,
            code_max_body_lines: 3,
            ..EngineOptions::default()
        },
    ];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut compressed = 0usize;
    for case in 0..360 {
        let content = payload(&mut rng);
        let opts = &option_sets[case % option_sets.len()];
        let role = if case % 5 == 0 { "user" } else { "tool" };
        let run = |content: &str| {
            let mut refs = opts.collector();
            let mut applied = Vec::new();
            let ctx = Context::for_role(role);
            let (out, kind) =
                compress_content(content, role, opts, None, &mut refs, &ctx, &mut applied);
            (out.into_owned(), kind, refs)
        };
        let (out, kind, refs) = run(&content);
        let (again, kind_again, _) = run(&content);
        assert_eq!(out, again, "case {case}: output differs between two runs");
        assert_eq!(kind, kind_again, "case {case}");
        assert!(
            out.chars().count() <= content.chars().count(),
            "case {case}: {} -> {} characters",
            content.chars().count(),
            out.chars().count()
        );
        if kind.is_none() {
            assert_eq!(
                out, content,
                "case {case}: declined, but not returned whole"
            );
            assert!(
                refs.is_empty(),
                "case {case}: declined, but left fragments behind"
            );
            continue;
        }
        compressed += 1;
        let before = refs_in(&content);
        let after = refs_in(&out);
        for (id, _) in refs.pairs() {
            assert!(
                after.contains(id),
                "case {case}: {id} was stored and nothing in the output points at it"
            );
        }
        for id in &after {
            assert!(
                before.contains(id) || refs.pairs().iter().any(|(held, _)| held == id),
                "case {case}: the output names {id}, which was never stored"
            );
        }
    }
    assert!(
        compressed >= 30,
        "only {compressed} of 360 payloads compressed: the generator proves too little"
    );
}
