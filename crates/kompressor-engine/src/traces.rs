//! Stack traces and test runs: the two shapes inside a log whose lines are not
//! equally worth keeping, recognised by their own grammar rather than by score.
//!
//! # Runtime frames
//!
//! A traceback is mostly the language and its libraries: an exception raised
//! three frames into a request handler arrives under forty frames of
//! `site-packages/django`, `node:internal/process`, `java.base/...` or
//! `tokio::runtime`. Those frames say *where the runtime was*, and they are
//! the same on every run; the frames in the agent's own code say where the bug
//! is. `collapse_runtime_frames` replaces each run of consecutive library
//! frames with one omission marker, and keeps verbatim the first and last frame
//! of every trace and every frame outside a library location. The idea is
//! Headroom's (`log_compressor.rs`); the rules for what is a library are
//! deliberately narrow, so a frame this cannot classify is kept.
//!
//! The marker goes through [`omission_marker`], so in `recoverable` mode the
//! collapsed frames are one fetch away and in `aggressive` mode the marker says
//! what left. `lossless` never calls this.
//!
//! # Test totals
//!
//! `is_test_totals_line` and `is_assertion_line` name the lines an agent
//! acts on after a test run — `=== 2 failed, 40 passed in 3.1s ===`, `test
//! result: FAILED.`, the first `AssertionError` — so the log compressor can
//! keep them whatever its scorer or its fold would otherwise do.

use std::borrow::Cow;

use once_cell::sync::Lazy;
use regex::Regex;

use crate::markers::omission_marker;
use crate::refs::RefCollector;
use crate::text::{chars_len, join_lines};

/// Fewer consecutive library frames than this are left alone: one frame
/// replaced by a marker saves almost nothing and costs a fetch to read.
const MIN_COLLAPSE: usize = 2;

/// Python: `  File "/usr/lib/python3.12/site-packages/x.py", line 12, in f`.
static PY_FILE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"^(\s*)File "([^"]+)", line \d+"#).unwrap());

/// Node and the JVM: `    at handler (/app/node_modules/x/index.js:1:2)`,
/// `\tat java.base/java.lang.Thread.run(Thread.java:840)`.
static AT_FRAME: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(\s+)at\s+(?:async\s+)?(\S.*)$").unwrap());

/// A Rust backtrace symbol line: `  12: tokio::runtime::task::raw::poll`, with
/// an optional address. The symbol must be a path, so a numbered list in prose
/// is never read as one.
static RUST_SYMBOL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(\s*)\d+:\s+(?:0x[0-9a-fA-F]+ - )?(<?[A-Za-z_][\w<>, &*\[\]]*::\S*.*|__rust\w*|rust_begin_unwind)$").unwrap()
});

/// The location line under a Rust symbol: `             at /rustc/…/src/x.rs:12:5`.
static RUST_AT: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s+at\s+\S+:\d+(?::\d+)?\s*$").unwrap());

/// A Go goroutine frame: a call, then its tab-indented `file.go:line +0x..`.
static GO_CALL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?:created by )?([\w./*()\[\]-]+)\(.*\)(?: in goroutine \d+)?$").unwrap()
});
static GO_AT: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\t(\S+\.go):\d+").unwrap());

/// One frame: `lines[start..end]`, and whether it is runtime or library code.
#[derive(Clone, Copy, Debug)]
struct Frame {
    start: usize,
    end: usize,
    library: bool,
}

fn python_library(path: &str) -> bool {
    path.contains("site-packages")
        || path.contains("dist-packages")
        || path.starts_with("<frozen ")
        || path.contains("/lib/python")
        || path.contains("\\Lib\\")
}

fn at_library(location: &str) -> bool {
    let symbol = location.trim_start_matches(['(', '<']);
    location.contains("node_modules")
        || location.contains("node:internal")
        || location.contains("(internal/")
        || location.starts_with("internal/")
        || ["java.", "javax.", "sun.", "jdk.internal.", "java.base/"]
            .iter()
            .any(|prefix| symbol.starts_with(prefix))
}

fn rust_library(symbol: &str) -> bool {
    let symbol = symbol.trim_start_matches('<');
    ["std::", "core::", "alloc::", "tokio::"]
        .iter()
        .any(|prefix| symbol.starts_with(prefix))
        || symbol.starts_with("__rust")
        || symbol.starts_with("rust_begin_unwind")
}

fn go_library(call: &str, location: &str) -> bool {
    call.starts_with("runtime.") || location.contains("/src/runtime/")
}

/// The frame starting at `lines[at]`, if one does.
fn frame_at(lines: &[&str], at: usize) -> Option<Frame> {
    let line = lines[at];
    if let Some(caps) = PY_FILE.captures(line) {
        let indent = caps[1].len();
        // The source line and caret line CPython prints under a frame belong
        // to it: they are indented further and are not themselves frames.
        let mut end = at + 1;
        while end < lines.len()
            && end < at + 10
            && !PY_FILE.is_match(lines[end])
            && leading_ws(lines[end]) > indent
            && !lines[end].trim().is_empty()
        {
            end += 1;
        }
        return Some(Frame {
            start: at,
            end,
            library: python_library(&caps[2]),
        });
    }
    if let Some(caps) = RUST_SYMBOL.captures(line) {
        let end = if lines.get(at + 1).is_some_and(|next| RUST_AT.is_match(next)) {
            at + 2
        } else {
            at + 1
        };
        return Some(Frame {
            start: at,
            end,
            library: rust_library(&caps[2]),
        });
    }
    if let Some(caps) = GO_CALL.captures(line)
        && let Some(next) = lines.get(at + 1)
        && let Some(location) = GO_AT.captures(next)
    {
        return Some(Frame {
            start: at,
            end: at + 2,
            library: go_library(&caps[1], &location[1]),
        });
    }
    if let Some(caps) = AT_FRAME.captures(line) {
        return Some(Frame {
            start: at,
            end: at + 1,
            library: at_library(&caps[2]),
        });
    }
    None
}

fn leading_ws(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Every trace in `lines`: maximal runs of frames that follow one another with
/// nothing in between.
fn traces(lines: &[&str]) -> Vec<Vec<Frame>> {
    let mut out: Vec<Vec<Frame>> = Vec::new();
    let mut current: Vec<Frame> = Vec::new();
    let mut at = 0;
    while at < lines.len() {
        match frame_at(lines, at) {
            Some(frame) => {
                at = frame.end;
                current.push(frame);
            }
            None => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                at += 1;
            }
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Replace each run of library frames inside a trace with one marker line.
///
/// `None` when nothing was collapsed, so the caller keeps its own lines. A run
/// whose fragment the collector would refuse for size is left whole rather
/// than shipped behind a marker with nothing behind it.
pub(crate) fn collapse_runtime_frames<'a>(
    lines: &[&'a str],
    refs: &mut RefCollector,
) -> Option<Vec<Cow<'a, str>>> {
    // (first line, past last line, frames) of each run to collapse
    let mut runs: Vec<(usize, usize, usize)> = Vec::new();
    for trace in traces(lines) {
        if trace.len() < MIN_COLLAPSE + 2 {
            continue;
        }
        // the first and last frame of the trace are never part of a run
        let inner = &trace[1..trace.len() - 1];
        let mut index = 0;
        while index < inner.len() {
            if !inner[index].library {
                index += 1;
                continue;
            }
            let first = index;
            while index < inner.len() && inner[index].library {
                index += 1;
            }
            let count = index - first;
            if count >= MIN_COLLAPSE {
                runs.push((inner[first].start, inner[index - 1].end, count));
            }
        }
    }
    if runs.is_empty() {
        return None;
    }
    let mut out: Vec<Cow<'a, str>> = Vec::with_capacity(lines.len());
    let mut cursor = 0;
    let mut collapsed = false;
    for (start, stop, count) in runs {
        out.extend(lines[cursor..start].iter().map(|l| Cow::Borrowed(*l)));
        let block = &lines[start..stop];
        let fragment = join_lines(block);
        let unretainable = refs.is_enabled() && fragment.len() > refs.max_ref_bytes();
        let indent = &lines[start][..leading_ws(lines[start])];
        let marker = if unretainable {
            None
        } else {
            let mark = refs.mark();
            let marker = format!(
                "{indent}{}",
                omission_marker("library frames", count, &fragment, refs, "")
            );
            if chars_len(&marker) < chars_len(&fragment) {
                Some(marker)
            } else {
                refs.rollback(mark);
                None
            }
        };
        match marker {
            Some(marker) => {
                collapsed = true;
                out.push(Cow::Owned(marker));
            }
            None => out.extend(block.iter().map(|l| Cow::Borrowed(*l))),
        }
        cursor = stop;
    }
    out.extend(lines[cursor..].iter().map(|l| Cow::Borrowed(*l)));
    collapsed.then_some(out)
}

/// A test runner's totals line.
static TEST_TOTALS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        // pytest: `==== 2 failed, 40 passed, 1 warning in 3.12s ====`
        r"^=+ .*\b\d+ (?:passed|failed|errors?|skipped|xfailed|xpassed|deselected)\b.*=+\s*$",
        // cargo test
        r"|^test result: (?:ok|FAILED)\. \d+ passed",
        // jest / vitest
        r"|^\s*(?:Tests|Test Suites|Test Files):\s+.*\b\d+ (?:passed|failed|total)\b",
        // go test
        r"|^(?:ok|FAIL)\s+\S+\s+(?:[\d.]+s|\(cached\))",
        r"|^(?:PASS|FAIL)\s*$",
        // unittest
        r"|^Ran \d+ tests? in [\d.]+s",
        r"|^(?:OK|FAILED)(?: \(.*\))?\s*$",
        // mocha
        r"|^\s*\d+ (?:passing|failing)\b",
        // rspec
        r"|^\d+ examples?, \d+ failures?",
        // maven surefire / gradle
        r"|^\s*(?:\[\w+\]\s+)?Tests run: \d+, Failures: \d+",
    ))
    .unwrap()
});

/// The line a failed test reports first.
static ASSERTION: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        // pytest's `E   assert 1 == 2` and `E   AssertionError: …`
        r"^E\s{2,}\S",
        r"|\bAssertionError\b",
        r"|\bassertion failed\b",
        r"|\bassertion `.*` failed",
        r"|panicked at\b",
        r"|^\s*--- FAIL:",
        r"|^\s*●\s",
        r"|^\s*Expected:?\s",
    ))
    .unwrap()
});

/// Whether `line` is a test run's totals line.
pub(crate) fn is_test_totals_line(line: &str) -> bool {
    TEST_TOTALS.is_match(line)
}

/// Whether `line` reports a failed assertion.
pub(crate) fn is_assertion_line(line: &str) -> bool {
    ASSERTION.is_match(line)
}

/// Lines of `lines` a test run must keep: every totals line, and the first
/// assertion line.
pub(crate) fn test_lines_to_keep(lines: &[&str]) -> Vec<usize> {
    let mut keep: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| is_test_totals_line(line))
        .map(|(index, _)| index)
        .collect();
    if let Some(first) = lines.iter().position(|line| is_assertion_line(line)) {
        keep.push(first);
    }
    keep.sort_unstable();
    keep.dedup();
    keep
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collapse(text: &str) -> (Option<String>, RefCollector) {
        let lines: Vec<&str> = text.split('\n').collect();
        let mut refs = RefCollector::new(1 << 20);
        let out = collapse_runtime_frames(&lines, &mut refs).map(|out| join_lines(&out));
        (out, refs)
    }

    fn python_trace() -> String {
        let mut lines = vec![
            "Traceback (most recent call last):".to_string(),
            "  File \"/app/main.py\", line 10, in <module>".to_string(),
            "    run()".to_string(),
        ];
        for n in 0..6 {
            lines.push(format!(
                "  File \"/usr/lib/python3.12/site-packages/django/core/handlers/h{n}.py\", line {n}, in inner"
            ));
            lines.push("    response = get_response(request)".to_string());
        }
        lines.push("  File \"/app/views.py\", line 42, in index".to_string());
        lines.push("    return compute(request)".to_string());
        for n in 0..3 {
            lines.push(format!(
                "  File \"/usr/lib/python3.12/site-packages/lib/m{n}.py\", line 7, in call"
            ));
            lines.push("    return f()".to_string());
        }
        lines.push(
            "  File \"/usr/lib/python3.12/site-packages/lib/last.py\", line 9, in boom".to_string(),
        );
        lines.push("    raise KeyError(key)".to_string());
        lines.push("KeyError: 'id'".to_string());
        lines.join("\n")
    }

    #[test]
    fn python_library_frames_collapse_and_user_frames_stay() {
        let (out, refs) = collapse(&python_trace());
        let out = out.expect("collapsed");
        assert!(out.contains("File \"/app/main.py\", line 10"), "{out}");
        assert!(out.contains("File \"/app/views.py\", line 42"), "{out}");
        assert!(
            out.contains("[... 6 library frames omitted [ref:k_"),
            "{out}"
        );
        assert!(
            out.contains("[... 3 library frames omitted [ref:k_"),
            "{out}"
        );
        // the last frame of the trace and the exception stay
        assert!(out.contains("lib/last.py\", line 9, in boom"), "{out}");
        assert!(out.contains("    raise KeyError(key)"), "{out}");
        assert!(out.ends_with("KeyError: 'id'"), "{out}");
        assert!(!out.contains("handlers/h3.py"), "{out}");
        // the collapsed frames are the fragment, source lines included
        let (_, fragment) = &refs.pairs()[0];
        assert!(fragment.contains("handlers/h0.py") && fragment.contains("handlers/h5.py"));
        assert!(fragment.contains("response = get_response(request)"));
    }

    #[test]
    fn node_and_java_frames_collapse() {
        let mut node = vec!["TypeError: x is undefined".to_string()];
        node.push("    at handler (/app/src/server.js:10:5)".to_string());
        for n in 0..4 {
            node.push(format!(
                "    at Layer.handle (/app/node_modules/express/lib/router/layer.js:{n}:5)"
            ));
        }
        node.push(
            "    at process.processTicksAndRejections (node:internal/process/task_queues:95:5)"
                .into(),
        );
        node.push("    at main (/app/src/index.js:3:1)".to_string());
        let (out, _) = collapse(&node.join("\n"));
        let out = out.expect("collapsed");
        assert!(out.contains("[... 5 library frames omitted"), "{out}");
        assert!(out.contains("/app/src/server.js:10:5") && out.contains("/app/src/index.js:3:1"));

        let mut java = vec!["java.lang.IllegalStateException: boom".to_string()];
        java.push("\tat com.acme.Service.run(Service.java:12)".to_string());
        for n in 0..3 {
            java.push(format!(
                "\tat java.util.concurrent.Executor.run{n}(Executor.java:{n})"
            ));
        }
        java.push("\tat jdk.internal.reflect.Method.invoke(Method.java:1)".into());
        java.push("\tat java.base/java.lang.Thread.run(Thread.java:840)".into());
        let (out, _) = collapse(&java.join("\n"));
        let out = out.expect("collapsed");
        assert!(out.contains("\t[... 4 library frames omitted"), "{out}");
        assert!(
            out.contains("Service.java:12") && out.contains("Thread.java:840"),
            "{out}"
        );
    }

    #[test]
    fn rust_and_go_frames_collapse() {
        let mut rust = vec!["thread 'main' panicked at src/main.rs:4:5:".to_string()];
        rust.push("   0: rust_begin_unwind".into());
        rust.push("             at /rustc/abc/library/std/src/panicking.rs:665:5".into());
        rust.push("   1: myapp::handler".into());
        rust.push("             at ./src/main.rs:4:5".into());
        for n in 2..6 {
            rust.push(format!("   {n}: tokio::runtime::task::raw::poll{n}"));
            rust.push(format!(
                "             at /home/u/.cargo/registry/tokio/src/raw.rs:{n}:1"
            ));
        }
        rust.push("   6: std::rt::lang_start".into());
        rust.push("   7: main".into());
        let (out, _) = collapse(&rust.join("\n"));
        // `main` has no path, so the trace ends at `std::rt::lang_start`
        let out = out.expect("collapsed");
        assert!(out.contains("   [... 4 library frames omitted"), "{out}");
        assert!(out.contains("myapp::handler") && out.contains("std::rt::lang_start"));

        let mut go = vec!["goroutine 1 [running]:".to_string()];
        go.push("main.handler(0x1)".into());
        go.push("\t/app/main.go:12 +0x1d".into());
        for n in 0..3 {
            go.push(format!("runtime.gopark{n}(0x0?)"));
            go.push(format!("\t/usr/local/go/src/runtime/proc.go:{n} +0x1"));
        }
        go.push("main.main()".into());
        go.push("\t/app/main.go:30 +0x2".into());
        let (out, _) = collapse(&go.join("\n"));
        let out = out.expect("collapsed");
        assert!(out.contains("[... 3 library frames omitted"), "{out}");
        assert!(
            out.contains("/app/main.go:12") && out.contains("/app/main.go:30"),
            "{out}"
        );
    }

    #[test]
    fn a_trace_of_user_frames_or_a_lone_library_frame_is_untouched() {
        let user = "Traceback (most recent call last):\n  File \"/app/a.py\", line 1, in a\n    b()\n  \
                    File \"/app/b.py\", line 2, in b\n    c()\n  File \"/app/c.py\", line 3, in c\n    \
                    raise E\nE: x";
        assert!(collapse(user).0.is_none());
        let one = "  File \"/app/a.py\", line 1, in a\n  File \"/x/site-packages/y.py\", line 2, in y\n  \
                   File \"/app/c.py\", line 3, in c";
        assert!(collapse(one).0.is_none());
        // a numbered list in prose is not a Rust backtrace
        assert!(
            collapse("1: first item\n2: second item\n3: third\n4: fourth")
                .0
                .is_none()
        );
    }

    #[test]
    fn a_disabled_collector_still_collapses_with_a_plain_marker() {
        let lines: Vec<String> = python_trace().split('\n').map(str::to_string).collect();
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let mut refs = RefCollector::disabled();
        let out = join_lines(&collapse_runtime_frames(&lines, &mut refs).unwrap());
        assert!(out.contains("[... 6 library frames omitted]"), "{out}");
        assert!(!out.contains("ref:k_"));
    }

    #[test]
    fn a_run_too_large_to_retain_is_left_whole() {
        let lines: Vec<String> = python_trace().split('\n').map(str::to_string).collect();
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let mut refs = RefCollector::new(16);
        assert!(collapse_runtime_frames(&lines, &mut refs).is_none());
        assert_eq!(
            refs.oversized_attempts(),
            0,
            "nothing was asked of the store"
        );
    }

    #[test]
    fn totals_lines_are_recognised_for_each_runner() {
        for line in [
            "==================== 2 failed, 40 passed, 1 warning in 3.12s ====================",
            "=========== 5 passed in 0.10s ===========",
            "test result: FAILED. 3 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out",
            "test result: ok. 12 passed; 0 failed; 0 ignored",
            "Tests:       1 failed, 9 passed, 10 total",
            "Test Suites: 1 failed, 3 passed, 4 total",
            "ok  \tgithub.com/acme/pkg\t0.012s",
            "FAIL\tgithub.com/acme/pkg\t0.020s",
            "FAIL",
            "Ran 14 tests in 0.031s",
            "FAILED (failures=2)",
            "  3 passing (12ms)",
            "12 examples, 1 failure",
            "[INFO] Tests run: 8, Failures: 1, Errors: 0, Skipped: 0",
        ] {
            assert!(is_test_totals_line(line), "{line}");
        }
        for line in [
            "passed the parcel",
            "== heading ==",
            "test result was printed",
        ] {
            assert!(!is_test_totals_line(line), "{line}");
        }
    }

    #[test]
    fn the_first_assertion_and_every_totals_line_are_kept() {
        let lines = [
            "collected 3 items",
            "E       assert 1 == 2",
            "E       AssertionError: second",
            "test result: ok. 1 passed; 0 failed",
            "noise",
            "test result: FAILED. 0 passed; 1 failed",
        ];
        assert_eq!(test_lines_to_keep(&lines), vec![1, 3, 5]);
    }
}
