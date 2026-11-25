//! Pathological payloads, and a seeded generator of more of them.
//!
//! The proxy gives the engine one deadline for a whole request
//! (`proxy.compress_timeout`, 2 s by default) and forwards the client's body
//! when it runs out. A payload whose compression is superlinear in its length
//! is therefore not a crash but a silent loss — the request goes out whole —
//! and it hides behind the deadline until the tail gets long enough to show up
//! as `compress` fail-opens. These cases put a bound on each shape that has
//! caused such a blowup somewhere: a multi-megabyte single line, deep JSON
//! nesting, lines that almost repeat, a million tiny lines, a diff with
//! thousands of hunks — and the one this file found, Python followed by text
//! the grammar cannot read (`code::PARSE_BUDGET_FACTOR`).
//!
//! # The bound
//!
//! 1.5 s per payload in a release build, inside the proxy's 2 s deadline with
//! room for the rest of the request. Measured on a 16-core machine at a load
//! average near 20, the slowest shapes at 1 MiB took 0.85 s (a million
//! one-character lines) and 0.97 s (Python then unparseable text); the rest
//! under 0.45 s. The ceiling is generous for the reason `timing.rs` gives:
//! it still catches a superlinear blowup, which on these sizes is tens of
//! seconds, while staying stable on a shared machine. Like
//! `timing.rs`, the numbers only mean something optimized: a debug build runs
//! the engine about fifteen times slower, so there every payload is an eighth
//! of the size and the bound ten seconds — a smoke test that each shape still
//! runs to the end. Run the real check with `--release`.
//!
//! Payloads above `max_compress_bytes` are not compressed at all, so the
//! multi-megabyte cases run with that bound set to 1 MiB and assert that they
//! are skipped, quickly; the shapes themselves are timed at 1 MiB, the most
//! that bound lets through.
//!
//! # The long run
//!
//! `a_long_seeded_fuzz_run_stays_inside_the_deadline` is ignored by default.
//! `KOMPRESSOR_FUZZ_SECONDS=1800 cargo test --release -p kompressor-engine
//! --test pathological -- --ignored` runs it for thirty minutes;
//! `KOMPRESSOR_FUZZ_SEED` replays one seed. Every payload it reports is
//! reproducible from the seed it prints.

use std::time::{Duration, Instant};

use kompressor_engine::EngineOptions;
use kompressor_engine::messages::{Message, RequestOptions, compress_messages};
use kompressor_engine::pipeline::compress_content;
use kompressor_engine::types::Context;

const MB: usize = 1024 * 1024;

/// The `max_compress_bytes` the long cases run under: the value
/// `EngineOptions::max_compress_bytes` documents as keeping one payload inside
/// half the deadline.
const BOUND_BYTES: usize = MB;

/// The proxy's whole compress deadline, `proxy.compress_timeout`'s default:
/// what the long run holds each payload to, as the plan's acceptance
/// criterion states it. In a debug build, the same ten seconds as [`bound`].
fn deadline() -> Duration {
    if cfg!(debug_assertions) {
        Duration::from_secs(10)
    } else {
        Duration::from_secs(2)
    }
}

fn bound() -> Duration {
    if cfg!(debug_assertions) {
        Duration::from_secs(10)
    } else {
        Duration::from_millis(1500)
    }
}

/// `bytes`, or an eighth of it in a debug build. See the module docs.
fn sized(bytes: usize) -> usize {
    if cfg!(debug_assertions) {
        bytes / 8
    } else {
        bytes
    }
}

fn bounded_options() -> EngineOptions {
    EngineOptions {
        max_compress_bytes: BOUND_BYTES,
        ..EngineOptions::default()
    }
}

/// SplitMix64: a few lines, no dependency, and the same stream on every
/// platform, which is all a reproducible fuzz case needs.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }

    fn word(&mut self) -> String {
        const SYLLABLES: &[&str] = &[
            "ka", "lo", "mir", "un", "sta", "pe", "tor", "ex", "ul", "qua", "zi", "ron",
        ];
        (0..1 + self.below(4))
            .map(|_| self.pick(SYLLABLES))
            .collect()
    }
}

/// Each shape the generator can produce, at a size it is asked for.
#[derive(Clone, Copy, Debug)]
enum Shape {
    /// One line, no newline, of words.
    LongLine,
    /// One line of a single repeated character.
    RepeatedChar,
    /// Minified JSON on one line: an array of similar objects.
    MinifiedJson,
    /// Arrays and objects nested deep, then closed.
    DeepJson,
    /// The same unclosed opening repeated: JSON that never parses.
    UnclosedJson,
    /// Log lines from one template, each a little different from the last.
    NearMatches,
    /// Long lines (tens of kilobytes) that are near-copies of each other.
    LongNearMatches,
    /// A very large number of one- or two-character lines.
    TinyLines,
    /// Short lines, every one of them different.
    DistinctLines,
    /// A unified diff across many files and hunks.
    HugeDiff,
    /// grep-style `path:line:text` hits across many files.
    GrepHits,
    /// Python-like source, many small functions.
    Source,
    /// Base64 with no line breaks.
    Base64Blob,
    /// Multibyte text, carriage-return redraws and tabs.
    Unicode,
    /// Slices of the other shapes, concatenated.
    Mixed,
}

const SHAPES: &[Shape] = &[
    Shape::LongLine,
    Shape::RepeatedChar,
    Shape::MinifiedJson,
    Shape::DeepJson,
    Shape::UnclosedJson,
    Shape::NearMatches,
    Shape::LongNearMatches,
    Shape::TinyLines,
    Shape::DistinctLines,
    Shape::HugeDiff,
    Shape::GrepHits,
    Shape::Source,
    Shape::Base64Blob,
    Shape::Unicode,
    Shape::Mixed,
];

/// A payload of about `bytes` bytes in the given shape.
fn generate(shape: Shape, bytes: usize, rng: &mut Rng) -> String {
    let mut out = String::with_capacity(bytes + 256);
    match shape {
        Shape::LongLine => {
            while out.len() < bytes {
                out.push_str(&rng.word());
                out.push(' ');
            }
        }
        Shape::RepeatedChar => {
            let c = rng.pick(&["a", "=", " ", "-", "0"]);
            out.push_str(&c.repeat(bytes));
        }
        Shape::MinifiedJson => {
            out.push('[');
            let mut i = 0;
            while out.len() < bytes {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&format!(
                    r#"{{"id":{i},"name":"{}","status":"{}","score":{},"tags":["{}","{}"]}}"#,
                    rng.word(),
                    rng.pick(&["ok", "failed", "pending"]),
                    rng.below(1000),
                    rng.word(),
                    rng.word()
                ));
                i += 1;
            }
            out.push(']');
        }
        Shape::DeepJson => {
            let depth = (bytes / 12).max(1);
            for i in 0..depth {
                if i % 2 == 0 {
                    out.push_str("{\"k\":");
                } else {
                    out.push('[');
                }
            }
            out.push_str("null");
            for i in (0..depth).rev() {
                out.push(if i % 2 == 0 { '}' } else { ']' });
            }
        }
        Shape::UnclosedJson => {
            while out.len() < bytes {
                out.push_str("{\"a\":[");
            }
        }
        Shape::NearMatches => {
            let template = format!("{} {} request", rng.word(), rng.word());
            let mut line = 0u64;
            while out.len() < bytes {
                let mut text = format!(
                    "2026-11-25T10:{:02}:{:02}.{:03}Z INFO {template} id={} took={}ms\n",
                    (line / 60) % 60,
                    line % 60,
                    rng.below(1000),
                    rng.below(50),
                    rng.below(5000)
                );
                // One character somewhere flipped, so no two lines are the
                // same and every line is almost the one before.
                let at = rng.below(text.len() - 1);
                if text.is_char_boundary(at) && text.is_char_boundary(at + 1) {
                    text.replace_range(at..at + 1, "x");
                }
                out.push_str(&text);
                line += 1;
            }
        }
        Shape::LongNearMatches => {
            let mut base = String::new();
            while base.len() < 20_000 {
                base.push_str(&rng.word());
                base.push(' ');
            }
            while out.len() < bytes {
                let mut line = base.clone();
                for _ in 0..3 {
                    let at = rng.below(line.len() - 1);
                    if line.is_char_boundary(at) && line.is_char_boundary(at + 1) {
                        line.replace_range(at..at + 1, "#");
                    }
                }
                out.push_str(&line);
                out.push('\n');
            }
        }
        Shape::DistinctLines => {
            let mut i = 0u64;
            while out.len() < bytes {
                out.push_str(&format!("{i} {}\n", rng.next()));
                i += 1;
            }
        }
        Shape::TinyLines => {
            while out.len() < bytes {
                out.push_str(rng.pick(&["a", "", "}", "1", "ok", "-", " "]));
                out.push('\n');
            }
        }
        Shape::HugeDiff => {
            let mut file = 0;
            while out.len() < bytes {
                let path = format!("src/{}/{}.rs", rng.word(), rng.word());
                out.push_str(&format!(
                    "diff --git a/{path} b/{path}\nindex 1234567..89abcde 100644\n--- a/{path}\n+++ b/{path}\n"
                ));
                for hunk in 0..(5 + rng.below(40)) {
                    let start = hunk * 40 + 1;
                    out.push_str(&format!("@@ -{start},7 +{start},7 @@ fn f{file}() {{\n"));
                    for _ in 0..3 {
                        out.push_str(&format!("     let {} = {};\n", rng.word(), rng.below(99)));
                    }
                    out.push_str(&format!("-    old_{}();\n", rng.word()));
                    out.push_str(&format!("+    new_{}();\n", rng.word()));
                    for _ in 0..3 {
                        out.push_str(&format!("     let {} = {};\n", rng.word(), rng.below(99)));
                    }
                }
                file += 1;
            }
        }
        Shape::GrepHits => {
            while out.len() < bytes {
                out.push_str(&format!(
                    "src/{}/{}.rs:{}:    let {} = {}({});\n",
                    rng.word(),
                    rng.word(),
                    1 + rng.below(5000),
                    rng.word(),
                    rng.word(),
                    rng.word()
                ));
            }
        }
        Shape::Source => {
            let mut f = 0;
            while out.len() < bytes {
                out.push_str(&format!("def {}_{f}(value):\n", rng.word()));
                for _ in 0..(2 + rng.below(12)) {
                    out.push_str(&format!(
                        "    {} = {}(value, {})\n",
                        rng.word(),
                        rng.word(),
                        rng.below(100)
                    ));
                }
                out.push_str("    return value\n\n");
                f += 1;
            }
        }
        Shape::Base64Blob => {
            const ALPHABET: &[u8] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            while out.len() < bytes {
                out.push(ALPHABET[rng.below(ALPHABET.len())] as char);
            }
        }
        Shape::Unicode => {
            while out.len() < bytes {
                out.push_str(rng.pick(&[
                    "Ünïcödé ",
                    "日本語のテキスト ",
                    "🦀🦀 ",
                    "\tcol\t",
                    "\r[====>    ] 42%",
                    "\r\n",
                    "ζωή ",
                    "\u{200b}",
                    "\n",
                ]));
            }
        }
        Shape::Mixed => {
            while out.len() < bytes {
                let part = SHAPES[rng.below(SHAPES.len() - 1)];
                let size = (bytes / 4).max(64).min(bytes - out.len() + 64);
                out.push_str(&generate(part, size, rng));
                out.push('\n');
            }
        }
    }
    out
}

/// Compress `payload` both ways the engine is entered — as one body, and as a
/// tool message in a conversation, which adds the cross-message fold against
/// what came before it — and say how long the slower took.
fn time_one(payload: &str, opts: &EngineOptions) -> Duration {
    let started = Instant::now();
    let mut refs = opts.collector();
    let mut applied = Vec::new();
    let ctx = Context::for_role("tool");
    let _ = compress_content(payload, "tool", opts, None, &mut refs, &ctx, &mut applied);
    let single = started.elapsed();

    let started = Instant::now();
    let mut refs = opts.collector();
    let mut messages = vec![
        Message::text("user", "look at this"),
        Message::text("tool", payload),
    ];
    let _ = compress_messages(&mut messages, opts, &RequestOptions::default(), &mut refs);
    single.max(started.elapsed())
}

/// [`time_one`], timed a second time when the first is over `limit`, and
/// the faster of the two.
///
/// The machine these run on is shared, and a build starting beside a run
/// once put a payload that takes 0.37 s at 2.33 s. The engine is
/// deterministic, so a payload that is really slow is slow twice; one that
/// was only unlucky is not.
fn time_settled(payload: &str, opts: &EngineOptions, limit: Duration) -> Duration {
    let took = time_one(payload, opts);
    if took < limit {
        return took;
    }
    took.min(time_one(payload, opts))
}

fn assert_inside(name: &str, payload: &str, opts: &EngineOptions) {
    let took = time_settled(payload, opts, bound());
    println!("{name}: {} bytes in {took:?}", payload.len());
    assert!(
        took < bound(),
        "{name} ({} bytes) took {took:?}, over {:?}",
        payload.len(),
        bound()
    );
}

/// A payload over the bound is returned whole, under its skip reason, without
/// being read: these are 3 MiB and take what a copy takes.
#[test]
fn a_multi_megabyte_payload_over_the_bound_is_skipped_quickly() {
    let opts = bounded_options();
    let mut rng = Rng(1);
    for shape in [Shape::LongLine, Shape::TinyLines, Shape::Source] {
        let payload = generate(shape, 3 * MB, &mut rng);
        assert_inside(&format!("{shape:?} over the bound"), &payload, &opts);

        let mut refs = opts.collector();
        let mut messages = vec![Message::text("tool", payload.as_str())];
        let stats = compress_messages(&mut messages, &opts, &RequestOptions::default(), &mut refs);
        assert_eq!(stats.compressed_messages, 0, "{shape:?}");
        assert_eq!(
            stats.skipped_by_reason.get("above_max_bytes"),
            Some(&1),
            "{shape:?}: {:?}",
            stats.skipped_by_reason
        );
    }
}

#[test]
fn a_megabyte_single_line_stays_inside_the_deadline() {
    let opts = bounded_options();
    let mut rng = Rng(1);
    for shape in [
        Shape::LongLine,
        Shape::RepeatedChar,
        Shape::MinifiedJson,
        Shape::Base64Blob,
    ] {
        let payload = generate(shape, sized(BOUND_BYTES - 4096), &mut rng);
        assert_inside(&format!("{shape:?}"), &payload, &opts);
    }
}

#[test]
fn deep_json_nesting_stays_inside_the_deadline() {
    let opts = bounded_options();
    let mut rng = Rng(2);
    for shape in [Shape::DeepJson, Shape::UnclosedJson] {
        let payload = generate(shape, sized(BOUND_BYTES - 4096), &mut rng);
        assert_inside(&format!("{shape:?}"), &payload, &opts);
    }
}

#[test]
fn lines_that_almost_repeat_stay_inside_the_deadline() {
    let opts = bounded_options();
    let mut rng = Rng(3);
    for shape in [Shape::NearMatches, Shape::LongNearMatches] {
        let payload = generate(shape, sized(BOUND_BYTES - 32_768), &mut rng);
        assert_inside(&format!("{shape:?}"), &payload, &opts);
    }
}

#[test]
fn many_tiny_lines_stay_inside_the_deadline() {
    let opts = bounded_options();
    let mut rng = Rng(4);
    for shape in [Shape::TinyLines, Shape::DistinctLines] {
        let payload = generate(shape, sized(BOUND_BYTES - 4096), &mut rng);
        assert_inside(&format!("{shape:?}"), &payload, &opts);
    }
}

#[test]
fn a_huge_diff_stays_inside_the_deadline() {
    let opts = bounded_options();
    let mut rng = Rng(5);
    for shape in [Shape::HugeDiff, Shape::GrepHits] {
        let payload = generate(shape, sized(BOUND_BYTES - 32_768), &mut rng);
        assert_inside(&format!("{shape:?}"), &payload, &opts);
    }
}

#[test]
fn source_unicode_and_mixed_payloads_stay_inside_the_deadline() {
    let opts = bounded_options();
    let mut rng = Rng(6);
    for shape in [Shape::Source, Shape::Unicode, Shape::Mixed] {
        let payload = generate(shape, sized(BOUND_BYTES - 32_768), &mut rng);
        assert_inside(&format!("{shape:?}"), &payload, &opts);
    }
}

/// The blowup this file found. Python, then lines of words the grammar reads
/// as one long run of errors, then short lines holding lone carriage returns:
/// detected as source, and every rung of the Python salvage reached, each
/// re-parsing most of the payload. 17.4 s in a release build before the parse
/// budget; about one second after it.
#[test]
fn python_followed_by_text_the_grammar_cannot_read_stays_inside_the_deadline() {
    let opts = bounded_options();
    let mut rng = Rng(9);
    let mut payload = generate(Shape::Source, sized(40_000), &mut rng);
    payload.push_str(&generate(Shape::LongNearMatches, sized(540_000), &mut rng));
    payload.push_str(&generate(Shape::Unicode, sized(30_000), &mut rng));
    assert_inside("python then garbage", &payload, &opts);
}

/// The cases the long run found, kept. Each is a mixture detected as
/// source, around 110 KB, that the Python grammar spent seconds on:
///
/// - seed 1802471341: Python, multibyte text and log lines, which the grammar
///   reads as more errors than lines; salvage parsed all of it again on every
///   rung. 3.3 s, before salvage learned to decline such text
///   (`code::MIN_LINES_PER_SALVAGE_ERROR`).
/// - seed 1802468599: Python, then a JSON value nested 4,500 deep on one
///   line, with few enough errors to be salvaged. 2.8 s, before payloads
///   nested deeper than any compiler takes stopped being parsed
///   (`code::MAX_NESTING`); 38 ms after.
#[cfg_attr(
    debug_assertions,
    ignore = "a seeded case is full size; timing is only meaningful in a release build"
)]
#[test]
fn the_mixtures_the_long_run_found_stay_inside_the_deadline() {
    for seed in [1_802_471_341, 1_802_468_599] {
        let (shape, payload) = fuzz_case(seed);
        assert!(matches!(shape, Shape::Mixed), "{seed}: {shape:?}");
        assert_inside(&format!("seed {seed}"), &payload, &bounded_options());
    }
}

/// The payload the long run generates for `seed`.
fn fuzz_case(seed: u64) -> (Shape, String) {
    let mut rng = Rng(seed);
    let shape = SHAPES[rng.below(SHAPES.len())];
    // Sizes over four orders of magnitude, weighted to the small end where
    // most real payloads are, with the tail reaching past the bound.
    let bytes = 1usize << (8 + rng.below(14));
    let bytes = bytes + rng.below(bytes);
    let payload = generate(shape, bytes, &mut rng);
    (shape, payload)
}

/// The generator is deterministic: a seed names a payload.
#[test]
fn the_generator_replays_a_seed() {
    for shape in SHAPES {
        let a = generate(*shape, 4096, &mut Rng(42));
        let b = generate(*shape, 4096, &mut Rng(42));
        assert_eq!(a, b, "{shape:?}");
        assert!(
            a.len() >= 4096 || matches!(shape, Shape::DeepJson),
            "{shape:?}"
        );
    }
}

/// Random shapes at random sizes for as long as `KOMPRESSOR_FUZZ_SECONDS`
/// says (60 by default), under the 1 MiB bound, each held to the whole 2 s
/// deadline rather than the fixed cases' 1.5 s: the run is long, the machine
/// it runs on is often busy, and the deadline is the criterion. A 1 MiB
/// payload of one-character lines took 1.57 s at a load average near 20.
/// See the module docs.
#[test]
#[ignore = "long-running; set KOMPRESSOR_FUZZ_SECONDS and run with --release -- --ignored"]
fn a_long_seeded_fuzz_run_stays_inside_the_deadline() {
    let seconds: u64 = std::env::var("KOMPRESSOR_FUZZ_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(60);
    let replay: Option<u64> = std::env::var("KOMPRESSOR_FUZZ_SEED")
        .ok()
        .and_then(|value| value.parse().ok());
    let opts = bounded_options();
    let until = Instant::now() + Duration::from_secs(seconds);
    let mut seed = replay.unwrap_or(0x6b6f_6d70);
    let mut runs = 0u64;
    let mut slowest = (Duration::ZERO, 0u64, 0usize);
    loop {
        let (shape, payload) = fuzz_case(seed);
        let took = time_settled(&payload, &opts, deadline());
        assert!(
            took < deadline(),
            "seed {seed}: {shape:?} of {} bytes took {took:?}, over {:?}",
            payload.len(),
            deadline()
        );
        if took > slowest.0 {
            slowest = (took, seed, payload.len());
        }
        runs += 1;
        if replay.is_some() || Instant::now() >= until {
            break;
        }
        seed = seed.wrapping_add(1);
    }
    println!(
        "{runs} payloads; slowest {:?} (seed {}, {} bytes)",
        slowest.0, slowest.1, slowest.2
    );
}
