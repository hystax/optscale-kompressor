//! The command line, driven as a user drives it.
//!
//! These run the real binary rather than calling the command functions, because
//! most of what can break here is the part a unit test does not reach: argument
//! parsing, what lands on stdout against stderr, and the exit code. The exit
//! code in particular is load-bearing — `bench replay --compare` is meant to
//! gate CI on it.

use std::io::Write;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

/// The binary under test, built by cargo for this test target.
///
/// Configuration discovery is pinned at a directory that holds no
/// configuration, because otherwise every one of these runs would read
/// whatever the developer happens to have on the search path: a `[refs] path`
/// in a personal config would send `retrieve` looking somewhere the test never
/// wrote. `$XDG_CONFIG_HOME` ends the search before `$HOME` is consulted.
fn kompressor() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kompressor"));
    command
        .env_remove("KOMPRESSOR_CONFIG")
        .env("XDG_CONFIG_HOME", no_config_dir())
        .current_dir(scratch_dir());
    command
}

/// A working directory belonging to this invocation alone.
///
/// `compress` in recoverable mode retains fragments in the configured refs
/// store, and the default is `./data/refs.db` — *relative to the working
/// directory*. Every test that did not set one ran in the package directory
/// and shared one SQLite file, in parallel. Under contention the retention
/// check fails, the CLI correctly declines to emit a handle it cannot honour
/// and returns the payload uncompressed, and an assertion that compression
/// shrank anything fails.
///
/// It failed twice before the cause was understood, both times passing on
/// every re-run in isolation, which is exactly how a shared-state flake
/// presents. A test that sets its own `current_dir` still overrides this.
fn scratch_dir() -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "cwd-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).expect("a working directory for this invocation");
    path
}

/// A directory that exists and contains no `kompressor/config.toml`.
fn no_config_dir() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("no-config");
    std::fs::create_dir_all(&path).expect("a directory to point config discovery at");
    path
}

/// A payload well over the size gate that the log compressor folds hard.
/// Lines sharing a template but differing in their values, so the fold cannot
/// be reversed from its `[×N]` count and a handle is retained.
fn varying_log() -> String {
    (0..200)
        .map(|i| format!("2026-01-01T00:00:00Z INFO worker heartbeat ok, queue depth {i}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn foldable_log() -> String {
    (0..200)
        .map(|_| "2026-01-01T00:00:00Z INFO worker heartbeat ok, queue depth 0")
        .collect::<Vec<_>>()
        .join("\n")
}

fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let mut file = std::fs::File::create(&path).expect("a temp file");
    file.write_all(body.as_bytes()).expect("written");
    path
}

/// A path as a TOML basic string. Temporary directories are plain enough that
/// only the quoting itself matters, but writing it by hand is how a config
/// fixture ends up unparseable on the one machine with a space in its tmpdir.
fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_string()).to_string()
}

#[test]
fn compressing_from_standard_input_writes_only_the_payload_to_standard_output() {
    let log = foldable_log();
    let assert = kompressor()
        .args(["compress", "--mode", "recoverable"])
        .write_stdin(log.clone())
        .assert()
        .success();
    let output = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert!(output.len() < log.len(), "the payload was not compressed");
    assert!(
        output.contains("[×"),
        "a fold has to announce itself: {output}"
    );
    assert!(
        assert.get_output().stderr.is_empty(),
        "nothing but the payload unless asked"
    );
}

#[test]
fn compressing_a_file_by_name_gives_the_same_answer_as_the_pipe() {
    let dir = tempfile::tempdir().unwrap();
    let log = foldable_log();
    let path = write(dir.path(), "tool-output.log", &log);

    let from_file = kompressor()
        .args(["compress", path.to_str().unwrap()])
        .assert()
        .success();
    let from_stdin = kompressor()
        .arg("compress")
        .write_stdin(log)
        .assert()
        .success();

    assert_eq!(
        from_file.get_output().stdout,
        from_stdin.get_output().stdout,
        "the same bytes must compress the same way whichever way they arrive"
    );
}

#[test]
fn a_protected_role_comes_back_byte_identical() {
    let log = foldable_log();
    let assert = kompressor()
        .args(["compress", "--role", "assistant"])
        .write_stdin(log.clone())
        .assert()
        .success();
    assert_eq!(
        String::from_utf8(assert.get_output().stdout.clone()).unwrap(),
        log,
        "the model's own turns are its working memory"
    );
}

#[test]
fn compress_json_reports_the_type_the_sizes_and_the_handles() {
    let assert = kompressor()
        .args(["compress", "--json"])
        .write_stdin(varying_log())
        .assert()
        .success();
    let value: serde_json::Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("one JSON object");

    assert_eq!(value["kind"], "log");
    assert_eq!(value["mode"], "recoverable");
    assert!(value["chars_out"].as_u64().unwrap() < value["chars_in"].as_u64().unwrap());
    assert!(value["saved_pct"].as_f64().unwrap() > 50.0);
    assert!(
        !value["refs"].as_array().unwrap().is_empty(),
        "recoverable mode retains what it folds"
    );

    // The same fold over identical lines reports no handle at all: `[×200]`
    // reconstructs them, so there is nothing a fetch could add.
    let assert = kompressor()
        .args(["compress", "--json"])
        .write_stdin(foldable_log())
        .assert()
        .success();
    let value: serde_json::Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("one JSON object");
    assert!(value["chars_out"].as_u64().unwrap() < value["chars_in"].as_u64().unwrap());
    assert!(
        value["refs"].as_array().unwrap().is_empty(),
        "an identical repeat needs no handle"
    );
    assert!(
        value["content"].as_str().unwrap().contains("[×180]"),
        "180 folded, 20 kept as the untouched tail"
    );
}

#[test]
fn detect_json_names_the_type_and_the_compressor_it_selects() {
    let assert = kompressor()
        .args(["detect", "--json"])
        .write_stdin(foldable_log())
        .assert()
        .success();
    let value: serde_json::Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();

    assert_eq!(value["content_type"], "build");
    assert_eq!(value["strategy"], "log");
    assert!(value["confidence"].as_f64().unwrap() > 0.0);
}

#[test]
fn detect_says_when_a_payload_was_never_classified_because_it_is_too_small() {
    kompressor()
        .arg("detect")
        .write_stdin("tiny")
        .assert()
        .success()
        .stdout(predicate::str::contains("below min_compress_chars"));
}

#[test]
fn explain_names_the_winning_route_and_every_decision_behind_it() {
    let assert = kompressor()
        .args(["compress", "--explain"])
        .write_stdin(foldable_log())
        .assert()
        .success();
    let explanation = String::from_utf8(assert.get_output().stderr.clone()).unwrap();

    assert!(
        explanation.contains("route        whole_message"),
        "the winning reading has to be named: {explanation}"
    );
    for expected in [
        "size gate    passed",
        "detected     build",
        "strategy     log",
        "redraw",
        "accounted    accepted: the output carries an omission marker",
        "markers",
        "refs",
    ] {
        assert!(
            explanation.contains(expected),
            "{expected} missing from:\n{explanation}"
        );
    }
    assert!(
        !assert.get_output().stdout.is_empty(),
        "the payload still goes to stdout"
    );
}

#[test]
fn explain_says_a_handleless_marker_is_a_size_cap_rather_than_a_mode() {
    // The confusion this command exists to end: in aggressive mode nothing is
    // retained, and every marker is plain. The report must say so rather than
    // leaving an operator to conclude the mode is misconfigured.
    let assert = kompressor()
        .args(["compress", "--explain", "--mode", "aggressive"])
        .write_stdin(foldable_log())
        .assert()
        .success();
    let explanation = String::from_utf8(assert.get_output().stderr.clone()).unwrap();

    assert!(
        explanation.contains("retention is off in aggressive mode"),
        "{explanation}"
    );
    assert!(explanation.contains("refs         none retained"));
}

#[test]
fn explain_in_json_carries_the_same_report_as_one_field() {
    let assert = kompressor()
        .args(["compress", "--json", "--explain"])
        .write_stdin(foldable_log())
        .assert()
        .success();
    let value: serde_json::Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();

    assert_eq!(value["explain"]["route"]["winner"], "whole_message");
    assert_eq!(value["explain"]["strategy"], "log");
    assert!(
        value["explain"]["accounted"]["marker_present"]
            .as_bool()
            .unwrap()
    );
    assert!(!value["explain"]["markers"].as_array().unwrap().is_empty());
}

/// Standard output and standard error of one `compress` run.
fn compress_streams(args: &[&str], stdin: &str) -> (Vec<u8>, String) {
    let assert = kompressor()
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR")
        .env_remove("CLICOLOR_FORCE")
        .arg("compress")
        .args(args)
        .write_stdin(stdin.to_string())
        .assert()
        .success();
    let output = assert.get_output();
    (
        output.stdout.clone(),
        String::from_utf8(output.stderr.clone()).unwrap(),
    )
}

/// `text` without its SGR colour sequences.
fn strip_sgr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            for d in chars.by_ref() {
                if d == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[test]
fn colour_never_touches_standard_output() {
    let payload = varying_log();
    let (plain, _) = compress_streams(&[], &payload);
    for args in [
        &["--explain"][..],
        &["--explain", "--color", "always"],
        &["--show-output", "--color", "always"],
        &["--explain", "--show-output", "--color", "always"],
        &["--explain", "--show-output", "--color", "never"],
    ] {
        let (stdout, _) = compress_streams(args, &payload);
        assert_eq!(stdout, plain, "standard output changed under {args:?}");
    }
}

#[test]
fn a_coloured_report_is_the_plain_report_with_colour_added() {
    let payload = varying_log();
    let (_, plain) = compress_streams(&["--explain", "--color", "never"], &payload);
    let (_, coloured) = compress_streams(&["--explain", "--color", "always"], &payload);
    let (_, auto) = compress_streams(&["--explain"], &payload);
    assert!(
        !plain.contains('\u{1b}'),
        "never means no escapes: {plain:?}"
    );
    assert!(coloured.contains("\u{1b}["), "always means colour");
    assert_eq!(strip_sgr(&coloured), plain);
    assert_eq!(
        auto, plain,
        "auto on a pipe is plain, so existing scripts see what they always saw"
    );
}

#[test]
fn auto_colour_follows_clicolor_force_and_json_is_never_coloured() {
    let payload = varying_log();
    let forced = kompressor()
        .env_remove("NO_COLOR")
        .env("CLICOLOR_FORCE", "1")
        .args(["compress", "--explain"])
        .write_stdin(payload.clone())
        .assert()
        .success();
    let stderr = String::from_utf8(forced.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("\u{1b}["), "auto honours CLICOLOR_FORCE");

    let (stdout, stderr) = compress_streams(
        &["--json", "--explain", "--show-output", "--color", "always"],
        &payload,
    );
    assert!(
        !stdout.contains(&0x1b),
        "JSON on standard output stays plain"
    );
    let value: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(value["explain"]["strategy"], "log");
    assert!(
        strip_sgr(&stderr).starts_with("output       "),
        "with --json the view alone goes to standard error: {stderr}"
    );
}

#[test]
fn show_output_numbers_the_payload_and_picks_out_markers() {
    let payload = varying_log();
    let (stdout, stderr) = compress_streams(&["--show-output", "--color", "always"], &payload);
    let output = String::from_utf8(stdout).unwrap();
    let view = strip_sgr(&stderr);
    let mut lines = view.lines();
    assert!(
        lines.next().unwrap().contains("read as log"),
        "the detector's verdict picks the reading: {view}"
    );
    let shown: Vec<String> = lines
        .map(|l| l.split_once(" │ ").map_or("", |(_, t)| t).to_string())
        .collect();
    let expected: Vec<&str> = output.trim_end_matches('\n').split('\n').collect();
    assert_eq!(shown, expected, "each view line is the payload's line");
    // Bold yellow opens every marker span.
    assert!(
        stderr.contains("\u{1b}[1m\u{1b}[33m["),
        "markers are highlighted: {stderr:?}"
    );

    let json =
        r#"[{"id": 1, "name": "alpha", "ok": true}, {"id": 2, "name": "beta", "ok": false}]"#;
    let (_, stderr) = compress_streams(
        &["--show-output", "--type", "json", "--color", "always"],
        json,
    );
    assert!(strip_sgr(&stderr).contains("read as json"));
}

#[test]
fn a_payload_cannot_send_its_own_escapes_to_the_terminal() {
    // Under the size gate, so it passes through untouched.
    let payload = "title\u{1b}]0;pwned\u{7}\nclear\u{1b}[2J";
    let (stdout, stderr) = compress_streams(&["--show-output", "--color", "never"], payload);
    assert_eq!(
        stdout,
        payload.as_bytes(),
        "standard output is the payload, verbatim"
    );
    assert!(!stderr.contains('\u{1b}'), "{stderr:?}");
    assert!(stderr.contains("title␛]0;pwned␇"));
    assert!(stderr.contains("clear␛[2J"));
}

#[test]
fn check_config_accepts_the_built_in_defaults_and_prints_them_back() {
    kompressor()
        .arg("check-config")
        .assert()
        .success()
        .stdout(predicate::str::contains("[server]"))
        .stdout(predicate::str::contains("min_compress_chars = 512"));
}

#[test]
fn check_config_reads_a_file_and_redacts_its_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
        dir.path(),
        "kompressor.toml",
        "[server]\nlisten = \"127.0.0.1:9123\"\n\n[auth]\nsecret = \"hunter2\"\n",
    );
    kompressor()
        .args(["check-config", "--config", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("127.0.0.1:9123"))
        .stdout(predicate::str::contains("<redacted>"))
        .stdout(predicate::str::contains("hunter2").not());
}

#[test]
fn check_config_rejects_an_invalid_file_with_the_usage_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "broken.toml", "[server\nlisten = ");
    kompressor()
        .args(["check-config", "--config", path.to_str().unwrap()])
        .assert()
        .code(2);
}

#[test]
fn retrieve_returns_a_fragment_and_a_line_window_of_it() {
    use kompressor_engine::refs::ref_for;
    use kompressor_engine::types::Kind;
    use kompressor_store::SqliteRefStore;
    use kompressor_store::types::{PutMeta, RefStore, Tenant};

    let dir = tempfile::tempdir().unwrap();
    let refs_path = dir.path().join("refs.db");
    let fragment = "line one\nline two\nline three\nline four";
    let handle = ref_for(fragment);
    {
        let store = SqliteRefStore::open(&refs_path).unwrap();
        store
            .put_many(
                &Tenant::new("acme"),
                &[(handle, fragment.to_string())],
                &PutMeta::new(Kind::Log),
            )
            .unwrap();
    }

    kompressor()
        .args([
            "retrieve",
            &handle.to_string(),
            "--refs",
            refs_path.to_str().unwrap(),
            "--tenant",
            "acme",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("line one"))
        .stdout(predicate::str::contains("line four"));

    kompressor()
        .args([
            "retrieve",
            &handle.to_string(),
            "--refs",
            refs_path.to_str().unwrap(),
            "--tenant",
            "acme",
            "--lines",
            "2:3",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("line two"))
        .stdout(predicate::str::contains("line three"))
        .stdout(predicate::str::contains("line one").not())
        .stdout(predicate::str::contains("line four").not());
}

#[test]
fn retrieving_a_handle_from_another_tenant_fails_with_a_hint_that_does_not_invite_invention() {
    use kompressor_engine::refs::ref_for;
    use kompressor_engine::types::Kind;
    use kompressor_store::SqliteRefStore;
    use kompressor_store::types::{PutMeta, RefStore, Tenant};

    let dir = tempfile::tempdir().unwrap();
    let refs_path = dir.path().join("refs.db");
    let handle = ref_for("tenant acme's tool output");
    {
        let store = SqliteRefStore::open(&refs_path).unwrap();
        store
            .put_many(
                &Tenant::new("acme"),
                &[(handle, "tenant acme's tool output".to_string())],
                &PutMeta::new(Kind::Log),
            )
            .unwrap();
    }

    kompressor()
        .args([
            "retrieve",
            &handle.to_string(),
            "--refs",
            refs_path.to_str().unwrap(),
            "--tenant",
            "beta",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("rather than reconstructing"));
}

#[test]
fn a_handle_that_is_not_shaped_like_a_handle_is_a_usage_error() {
    kompressor()
        .args(["retrieve", "refs/heads/main"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("is not a handle"));
}

/// A statistics database holding one compression and one retrieval of it.
fn stats_database(dir: &Path) -> PathBuf {
    use kompressor_engine::types::{Kind, Mode};
    use kompressor_stats::{Event, OptimizationEvent, RetrievalEvent, StatsConfig, Surface, spawn};

    let stats_path = dir.join("stats.db");
    let config = StatsConfig::at(&stats_path);
    let (sink, handle) = spawn(&config).unwrap();

    let mut event = OptimizationEvent::new(
        1_700_000_000_000,
        "acme",
        "req-1",
        Surface::Api,
        Mode::Recoverable,
    );
    event.tokens_in = 9_000;
    event.tokens_out = 3_000;
    event.record_type(Kind::Json, 2);
    sink.send(Event::Optimization(event));

    // The debit side: without it every report reads as pure profit.
    sink.send(Event::Retrieval(RetrievalEvent {
        context: None,
        time_ms: 1_700_000_001_000,
        tenant: "acme".to_string(),
        request_id: Some("req-1".to_string()),
        surface: Surface::Mcp,
        reference: "k_AbCdEfGhIjKl".to_string(),
        found: true,
        expired: false,
        ranged: true,
        fragment_bytes: 4_000,
        fragment_chars: 4_000,
        fragment_tokens: 1_000,
        returned_chars: 2_000,
        returned_tokens: 500,
        withheld: false,
        range_status: None,
    }));

    sink.flush().unwrap();
    handle.shutdown().unwrap();
    stats_path
}

#[test]
fn stats_summary_reports_gross_and_net_from_a_real_database() {
    let dir = tempfile::tempdir().unwrap();
    let stats_path = stats_database(dir.path());

    kompressor()
        .args([
            "stats",
            "summary",
            "--stats",
            stats_path.to_str().unwrap(),
            "--tenant",
            "acme",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("saved gross         6000"))
        // gross minus what the retrieval put back: a report that showed only
        // gross would be advertising the system rather than describing it
        .stdout(predicate::str::contains("saved net           5500"));

    let assert = kompressor()
        .args([
            "stats",
            "by-type",
            "--stats",
            stats_path.to_str().unwrap(),
            "--tenant",
            "acme",
            "--format",
            "json",
        ])
        .assert()
        .success();
    let rows: serde_json::Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(rows[0]["content_type"], "json");
}

#[test]
fn every_other_stats_report_answers_from_the_same_database() {
    let dir = tempfile::tempdir().unwrap();
    let stats_path = stats_database(dir.path());
    let path = stats_path.to_str().unwrap().to_string();

    let run = |args: &[&str]| {
        let mut command = kompressor();
        command.arg("stats");
        command.args(args);
        command.args(["--stats", &path, "--tenant", "acme"]);
        command.assert().success()
    };

    run(&["histogram", "--bucket", "day"])
        .stdout(predicate::str::contains("2023-11-14"))
        .stdout(predicate::str::contains("5500"));
    run(&["retrievals"])
        .stdout(predicate::str::contains("hits           1"))
        // a ranged read is the cheap kind, so its share is worth watching
        .stdout(predicate::str::contains("ranged         1"))
        .stdout(predicate::str::contains("k_AbCdEfGhIjKl"));
    run(&["request", "req-1"])
        .stdout(predicate::str::contains("saved gross    6000"))
        .stdout(predicate::str::contains("saved net      5500"));
    // No sweep has sampled the store, and that is not an error.
    run(&["store"]).stdout(predicate::str::contains("no store sample"));

    kompressor()
        .args(["stats", "request", "req-missing", "--stats", &path])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("no events recorded"));
}

#[test]
fn stats_against_a_database_that_does_not_exist_says_so_rather_than_reporting_zeros() {
    kompressor()
        .args(["stats", "summary", "--stats", "/nonexistent/stats.db"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("no statistics database"));
}

#[test]
fn a_window_bound_that_is_not_a_time_is_a_usage_error() {
    kompressor()
        .args(["stats", "summary", "--from", "last tuesday"])
        .assert()
        .code(2);
}

/// A one-payload corpus and a baseline claiming a coverage this run cannot meet.
fn regression_fixture(dir: &Path) -> (PathBuf, PathBuf) {
    let corpus = write(
        dir,
        "corpus.jsonl",
        &format!(
            "{{\"corpus_version\": 1}}\n{}\n",
            serde_json::json!({
                "content_id": "a",
                "content": foldable_log(),
                "role": "tool",
                "source_tool": "exec_command",
                "requests": 4,
            })
        ),
    );
    let baseline = write(
        dir,
        "baseline.json",
        "{\"format\": 4, \"coverage_pct\": 99.0, \"claimed_pct\": 100.0, \"payloads\": 1,
          \"chars_in\": 1000, \"chars_out\": 10}",
    );
    (corpus, baseline)
}

/// The baseline gate has to measure the engine, not the machine it runs on.
///
/// `bench replay` reads `[engine]` so a knob can be A/B'd against the corpus
/// without rebuilding, and with no `--config` it used to fall back to the same
/// discovery every other command uses — `$KOMPRESSOR_CONFIG`, then
/// `./kompressor.toml`, then the XDG path. A developer with
/// `enable_normalised_text_fold = false` in a personal config therefore ran
/// `make bench` against a baseline recorded without it and was told the `text`
/// bucket had regressed from 61.20% to 14.15%, which is a real 47-point
/// difference in a real measurement and tells you nothing about the code.
///
/// A path that does not exist is the sharpest probe available: `discover`
/// returns `$KOMPRESSOR_CONFIG` whether or not the file is there, precisely so
/// a typo is reported rather than skipped, so a replay that still consulted
/// discovery could not possibly succeed here.
#[test]
fn bench_replay_ignores_a_discovered_config() {
    let dir = tempfile::tempdir().unwrap();
    let (corpus, _) = regression_fixture(dir.path());

    kompressor()
        .env("KOMPRESSOR_CONFIG", dir.path().join("nothing-here.toml"))
        .args(["bench", "replay", "--corpus", corpus.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("coverage"));
}

/// The same, for the search path rather than the variable: a config sitting in
/// the working directory is not read either. Unparseable on purpose — a replay
/// that read it would fail, and the point is that it is never opened.
#[test]
fn bench_replay_ignores_a_config_in_the_working_directory() {
    let dir = tempfile::tempdir().unwrap();
    let (corpus, _) = regression_fixture(dir.path());
    write(dir.path(), "kompressor.toml", "this is not toml {{{");

    kompressor()
        .current_dir(dir.path())
        .args(["bench", "replay", "--corpus", corpus.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("coverage"));
}

/// And the converse, because ignoring discovery would be the wrong fix if it
/// also ignored the flag: an explicit `--config` is still read, and a bad one
/// is still an error rather than a silent fall back to the defaults.
#[test]
fn bench_replay_still_reads_an_explicit_config() {
    let dir = tempfile::tempdir().unwrap();
    let (corpus, _) = regression_fixture(dir.path());
    let config = write(dir.path(), "broken.toml", "this is not toml {{{");

    kompressor()
        .args([
            "bench",
            "replay",
            "--corpus",
            corpus.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
        ])
        .assert()
        .failure();
}

/// An explicit `--config` is the whole configuration: the environment cannot
/// move it. Reproduced in the 2026-11-11 review — `min_compress_chars = 512`
/// in the file and an ambient `KOMPRESSOR_MIN_COMPRESS_CHARS=1000000` made a
/// payload that compressed 78.91% report zero eligible payloads, exit 0.
/// Both spellings are checked, because both reached `Config::load`'s overlay.
#[test]
fn bench_replay_with_an_explicit_config_ignores_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let (corpus, _) = regression_fixture(dir.path());
    let config = write(
        dir.path(),
        "replay.toml",
        "[engine]\nmin_compress_chars = 512\n",
    );
    let replay = |env: &[(&str, &str)]| {
        let mut command = kompressor();
        for (key, value) in env {
            command.env(key, value);
        }
        let output = command
            .args([
                "bench",
                "replay",
                "--json",
                "--corpus",
                corpus.to_str().unwrap(),
                "--config",
                config.to_str().unwrap(),
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let mut value =
            serde_json::from_slice::<serde_json::Value>(&output).expect("replay --json is JSON");
        // wall-clock, and the one field two identical replays disagree on
        value["baseline"]
            .as_object_mut()
            .expect("a baseline object")
            .remove("timing");
        value
    };

    let clean = replay(&[]);
    assert_eq!(clean["engine"]["min_compress_chars"], 512, "{clean}");
    assert!(
        clean["baseline"]["chars_out"].as_u64() < clean["baseline"]["chars_in"].as_u64(),
        "the fixture compresses under the file's settings: {clean}"
    );
    for env in [
        [("KOMPRESSOR_MIN_COMPRESS_CHARS", "1000000")],
        [("KOMPRESSOR__ENGINE__MIN_COMPRESS_CHARS", "1000000")],
    ] {
        let ambient = replay(&env);
        assert_eq!(
            ambient["engine"], clean["engine"],
            "{env:?} reached the replay"
        );
        assert_eq!(
            ambient["baseline"], clean["baseline"],
            "{env:?} moved the result"
        );
    }
}

#[test]
fn bench_replay_prints_coverage_claimed_and_billed() {
    let dir = tempfile::tempdir().unwrap();
    let (corpus, _) = regression_fixture(dir.path());

    kompressor()
        .args(["bench", "replay", "--corpus", corpus.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("coverage"))
        .stdout(predicate::str::contains("claimed"))
        .stdout(predicate::str::contains("billed"))
        .stdout(predicate::str::contains("no invariant violated"));
}

#[test]
fn a_bench_comparison_that_regressed_exits_three() {
    let dir = tempfile::tempdir().unwrap();
    let (corpus, baseline) = regression_fixture(dir.path());

    kompressor()
        .args([
            "bench",
            "replay",
            "--corpus",
            corpus.to_str().unwrap(),
            "--compare",
            baseline.to_str().unwrap(),
        ])
        .assert()
        .code(3)
        .stdout(predicate::str::contains("REGRESSED"))
        .stdout(predicate::str::contains("FAIL"));
}

#[test]
fn a_run_compared_against_its_own_recording_passes() {
    let dir = tempfile::tempdir().unwrap();
    let (corpus, _) = regression_fixture(dir.path());
    let recorded = dir.path().join("recorded.json");

    kompressor()
        .args([
            "bench",
            "replay",
            "--corpus",
            corpus.to_str().unwrap(),
            "--save",
            recorded.to_str().unwrap(),
        ])
        .assert()
        .success();
    kompressor()
        .args([
            "bench",
            "replay",
            "--corpus",
            corpus.to_str().unwrap(),
            "--compare",
            recorded.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("PASS"));
}

#[test]
fn a_baseline_from_an_older_format_fails_loudly_instead_of_comparing_against_zeros() {
    let dir = tempfile::tempdir().unwrap();
    let (corpus, _) = regression_fixture(dir.path());
    let stale = write(
        dir.path(),
        "stale.json",
        "{\"format\": 2, \"coverage_pct\": 1}",
    );

    kompressor()
        .args([
            "bench",
            "replay",
            "--corpus",
            corpus.to_str().unwrap(),
            "--compare",
            stale.to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("baseline format 2, expected 4"));
}

#[test]
fn saving_and_comparing_in_one_run_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (corpus, baseline) = regression_fixture(dir.path());

    kompressor()
        .args([
            "bench",
            "replay",
            "--corpus",
            corpus.to_str().unwrap(),
            "--save",
            baseline.to_str().unwrap(),
            "--compare",
            baseline.to_str().unwrap(),
        ])
        .assert()
        .code(2);
}

#[test]
fn version_reports_the_build_and_whether_the_grammars_are_compiled_in() {
    // Against the compiled-in version rather than a literal. Pinning the
    // number made this the one test that fails on a release, which teaches
    // whoever bumps it that a red suite is routine — and it never checked
    // anything the build did not already guarantee.
    kompressor()
        .arg("version")
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "kompressor {}",
            env!("CARGO_PKG_VERSION")
        )))
        .stdout(predicate::str::contains("code-aware"));
}

#[test]
fn recoverable_cli_handles_survive_the_compress_process() {
    let dir = tempfile::tempdir().unwrap();
    let log = varying_log();
    let output = kompressor()
        .current_dir(dir.path())
        .args(["compress", "--json"])
        .write_stdin(log.clone())
        .assert()
        .success();
    let compressed: serde_json::Value =
        serde_json::from_slice(&output.get_output().stdout).unwrap();
    let refs = compressed["refs"].as_array().unwrap();
    assert!(!refs.is_empty());
    for reference in refs {
        let handle = reference["handle"].as_str().unwrap();
        let retrieved = kompressor()
            .current_dir(dir.path())
            .args(["retrieve", handle])
            .assert()
            .success();
        let fragment = std::str::from_utf8(&retrieved.get_output().stdout)
            .unwrap()
            .trim_end();
        assert!(!fragment.is_empty());
        assert!(log.contains(fragment));
        assert_eq!(
            fragment.len(),
            reference["bytes"].as_u64().unwrap() as usize
        );
    }
}

#[test]
fn a_configured_refs_path_round_trips_from_compress_to_retrieve() {
    // The packaged installs write a config naming a refs path outside the
    // working directory. `retrieve` used to ignore the configuration entirely
    // and open the compiled-in `./data/refs.db`, so every handle `compress`
    // minted on those installs failed to resolve.
    let dir = tempfile::tempdir().unwrap();
    let refs_path = dir.path().join("store").join("refs.db");
    let config = write(
        dir.path(),
        "config.toml",
        &format!(
            "[refs]\npath = {}\n",
            toml_string(refs_path.to_str().unwrap())
        ),
    );
    let config = config.to_str().unwrap();
    let log = varying_log();

    let output = kompressor()
        .current_dir(dir.path())
        .args(["compress", "--json", "--config", config])
        .write_stdin(log.clone())
        .assert()
        .success();
    let compressed: serde_json::Value =
        serde_json::from_slice(&output.get_output().stdout).unwrap();
    let refs = compressed["refs"].as_array().unwrap();
    assert!(!refs.is_empty(), "nothing was retained to retrieve");
    assert!(
        refs_path.exists(),
        "the fragments went somewhere other than the configured store"
    );
    assert!(
        !dir.path().join("data").exists(),
        "the compiled-in default store was written despite the configuration"
    );

    for reference in refs {
        let handle = reference["handle"].as_str().unwrap();
        // Named explicitly, the way a user debugging this would.
        let retrieved = kompressor()
            .current_dir(dir.path())
            .args(["retrieve", handle, "--config", config])
            .assert()
            .success();
        let fragment = std::str::from_utf8(&retrieved.get_output().stdout)
            .unwrap()
            .trim_end();
        assert!(log.contains(fragment));

        // And discovered, which is what the packaged installs actually do.
        kompressor()
            .current_dir(dir.path())
            .env("KOMPRESSOR_CONFIG", config)
            .args(["retrieve", handle])
            .assert()
            .success()
            .stdout(predicate::str::contains(fragment.to_string()));

        // `--refs` still wins over the configuration rather than being the
        // only way to reach a store.
        kompressor()
            .current_dir(dir.path())
            .args([
                "retrieve",
                handle,
                "--config",
                config,
                "--refs",
                dir.path().join("elsewhere.db").to_str().unwrap(),
            ])
            .assert()
            .code(1);
    }
}

#[test]
fn cli_falls_back_when_fragments_cannot_survive_exit() {
    for refs_config in [
        "backend = \"memory\"",
        "path = \"blocked/refs.db\"",
        "max_bytes = 1",
    ] {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "blocked", "a file, not a directory");
        let config = write(
            dir.path(),
            "config.toml",
            &format!("[refs]\n{refs_config}\n"),
        );
        let log = varying_log();
        let output = kompressor()
            .current_dir(dir.path())
            .args(["compress", "--json", "--config", config.to_str().unwrap()])
            .write_stdin(log.clone())
            .assert()
            .success();
        let compressed: serde_json::Value =
            serde_json::from_slice(&output.get_output().stdout).unwrap();
        assert_eq!(compressed["content"], log);
        assert!(compressed["refs"].as_array().unwrap().is_empty());
        assert!(compressed["applied"].as_array().unwrap().is_empty());
    }
}

/// A six-thousand-step training loop, optionally interrupted at step 3000.
///
/// The shape matters: every line shares a template with its neighbour and
/// differs only in a varying tail, which is what makes detection read it as a
/// uniform stream and hand it to the head-and-tail sampler.
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

/// `compress --json --explain` over a payload, in a directory of its own so the
/// handles it mints can be fetched back afterwards.
fn compress_explaining(payload: &str, dir: &Path) -> serde_json::Value {
    let assert = Command::new(env!("CARGO_BIN_EXE_kompressor"))
        .env_remove("KOMPRESSOR_CONFIG")
        .env("XDG_CONFIG_HOME", no_config_dir())
        .current_dir(dir)
        .args(["compress", "--json", "--explain"])
        .write_stdin(payload.to_string())
        .assert()
        .success();
    serde_json::from_slice(&assert.get_output().stdout).expect("one JSON object")
}

#[test]
fn a_crash_buried_in_a_training_log_stays_visible() {
    // The reported failure: 353KB of training log went to 11 lines — five
    // steps, a marker, three steps — with `Fatal Python error: Segmentation
    // fault` inside the fragment. It was retrievable, and nothing in the
    // output said there was anything worth retrieving.
    let dir = tempfile::tempdir().unwrap();
    let value = compress_explaining(
        &training_log(&[
            "Fatal Python error: Segmentation fault",
            "Segmentation fault (core dumped)",
            "Restarting worker 3 from checkpoint step-2999",
        ]),
        dir.path(),
    );

    let content = value["content"].as_str().unwrap();
    for line in [
        "Fatal Python error: Segmentation fault",
        "Segmentation fault (core dumped)",
        "Restarting worker 3 from checkpoint step-2999",
    ] {
        assert!(
            content.contains(line),
            "{line:?} must survive in:\n{content}"
        );
    }

    // It is still a uniform stream — the fix routes it, it does not reclassify
    // it — and the route it took is the one that scores errors.
    let detection = &value["explain"]["detection"];
    assert_eq!(detection["uniform_stream"], true);
    assert_eq!(detection["error_signal"], true);
    assert_eq!(value["explain"]["strategy"], "log");
    // and the diversion is visible to analytics, which read `kind`
    assert_eq!(value["kind"], "log");

    // Keeping the crash costs about a kilobyte out of 353.
    assert!(
        value["saved_pct"].as_f64().unwrap() > 99.0,
        "still compresses hard, got {}",
        value["saved_pct"]
    );

    // The routine steps it did elide remain fetchable. `retrieve` truncates a
    // fragment this size by default, so assert on a line near its head rather
    // than one in the middle that the cap cuts off.
    let handle = value["refs"][0]["handle"].as_str().expect("a handle");
    Command::new(env!("CARGO_BIN_EXE_kompressor"))
        .env_remove("KOMPRESSOR_CONFIG")
        .env("XDG_CONFIG_HOME", no_config_dir())
        .current_dir(dir.path())
        .args([
            "retrieve",
            handle,
            "--refs",
            dir.path().join("data/refs.db").to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("step 20/6000"));
}

#[test]
fn a_native_crash_with_no_python_header_stays_visible() {
    // `Fatal Python error:` matches on `fatal` and on `error`, so a
    // CPython-wrapped segfault was always scored. A process that dies under
    // any other runtime says none of those words.
    let dir = tempfile::tempdir().unwrap();
    let value = compress_explaining(
        &training_log(&[
            "Segmentation fault (core dumped)",
            "Received signal 11 SIGSEGV, address not mapped to object",
            "Restarting worker 3 from checkpoint step-2999",
        ]),
        dir.path(),
    );

    let content = value["content"].as_str().unwrap();
    assert!(
        content.contains("Segmentation fault (core dumped)"),
        "the crash must survive without a Python header:\n{content}"
    );
    assert!(content.contains("SIGSEGV"), "and so must the signal line");
    assert_eq!(value["explain"]["detection"]["error_signal"], true);
    assert_eq!(value["kind"], "log");
}

#[test]
fn a_clean_training_log_still_samples_head_and_tail() {
    // The other half of the bargain: diverting every uniform stream to the
    // scored path would cost a clean progress log its ratio for nothing.
    let dir = tempfile::tempdir().unwrap();
    let value = compress_explaining(&training_log(&[]), dir.path());

    let detection = &value["explain"]["detection"];
    assert_eq!(detection["uniform_stream"], true);
    assert_eq!(detection["error_signal"], false, "nothing went wrong in it");
    assert_eq!(value["explain"]["strategy"], "listing");
    assert_eq!(value["kind"], "listing");
    assert!(
        value["content"]
            .as_str()
            .unwrap()
            .contains("entries omitted"),
        "the sampler's marker, not the log path's"
    );
    assert!(
        value["saved_pct"].as_f64().unwrap() > 99.5,
        "head-and-tail sampling stays as aggressive as it was, got {}",
        value["saved_pct"]
    );
}

#[test]
fn the_failures_a_long_run_actually_dies_of_all_stay_visible() {
    // The segfault was one member of a family. Probing the vocabulary against
    // real messages found five of eight common operational failures elided
    // with no hint, because `Permission denied`, `Connection refused`,
    // `Operation timed out` and `No space left on device` carry none of the
    // words the scorer knew. Each of these is a whole run lost.
    for failure in [
        "ls: cannot open directory '/srv/data/shard_0200': Permission denied",
        "ConnectionRefusedError: [Errno 111] Connection refused",
        "curl: (28) Operation timed out after 30001 milliseconds",
        "OSError: [Errno 28] No space left on device",
        "thread 'main' panicked at src/worker.rs:412",
        "Aborted (core dumped)",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let value = compress_explaining(&training_log(&[failure]), dir.path());
        assert!(
            value["content"].as_str().unwrap().contains(failure),
            "{failure:?} must survive, got:\n{}",
            value["content"].as_str().unwrap()
        );
        assert!(
            value["saved_pct"].as_f64().unwrap() > 99.0,
            "and keeping it must stay nearly free, got {}",
            value["saved_pct"]
        );
    }
}

/// The payload from the report, line for line.
///
/// Reproduced faithfully rather than simplified: the loss decays and `tok/s`
/// varies per step, so each line differs from its neighbour in three fields
/// rather than one. That matters — a stream whose lines vary more is a weaker
/// uniform-stream signal, and a test built on a constant `loss=` would be an
/// easier payload than the one that was filed.
///
/// Python's Mersenne Twister is not reproduced, so the bytes are not identical
/// to the reporter's; the shape, the field count and the size are.
fn reported_training_log() -> String {
    let mut seed: u64 = 3;
    let mut next = move || {
        // xorshift64*, so the varying tail is deterministic across runs
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        48_000 + (seed.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) % 4_001
    };
    let mut lines: Vec<String> = vec!["Loading dataset /data/train.jsonl".to_string()];
    let mut loss = 2.5f64;
    for step in 1..=6000 {
        loss *= 0.9999;
        lines.push(format!(
            "epoch 1 step {step}/6000 loss={loss:.4} lr=0.000300 tok/s={}",
            next()
        ));
        if step == 3000 {
            lines.push("Fatal Python error: Segmentation fault".to_string());
            lines.push("Segmentation fault (core dumped)".to_string());
            lines.push("Restarting worker 3 from checkpoint step-2999".to_string());
        }
    }
    lines.push("Training complete".to_string());
    lines.join("\n")
}

#[test]
fn the_reported_segfault_buried_in_a_training_log_is_no_longer_hidden() {
    // Reported: 353,063 chars went to 556 — the first five steps, the last
    // three, and `[... 5995 entries omitted [ref:k_…]]`. The segfault was
    // inside that fragment. It was retrievable, and nothing in the output
    // said there was anything worth retrieving.
    let dir = tempfile::tempdir().unwrap();
    let payload = reported_training_log();
    assert!(
        (340_000..370_000).contains(&payload.len()),
        "the reported payload was 353,063 chars; this one is {}",
        payload.len()
    );
    let value = compress_explaining(&payload, dir.path());
    let content = value["content"].as_str().unwrap();

    // The symptom, named exactly as the report named it.
    assert_eq!(
        value["explain"]["detection"]["uniform_stream"], true,
        "the report's `detection.uniform_stream: true`"
    );
    for line in [
        "Fatal Python error: Segmentation fault",
        "Segmentation fault (core dumped)",
        "Restarting worker 3 from checkpoint step-2999",
    ] {
        assert!(
            content.contains(line),
            "{line:?} was inside the fragment and must now be in the output:\n{content}"
        );
    }
    assert!(
        !content.contains("entries omitted"),
        "`entries omitted` is the sampler's marker — the report's failure mode:\n{content}"
    );

    // …and the compression the report measured is substantially kept: 99.84%
    // became 99.5%, about a kilobyte of the 353.
    assert!(
        value["saved_pct"].as_f64().unwrap() > 99.0,
        "got {}",
        value["saved_pct"]
    );
}

/// `mcp serve` fronting one server, answering `tools/list`.
///
/// The fronted server is this binary too, but reading a configuration that
/// names no servers. Pointing it at its own configuration would test the same
/// guard, and a regression in that guard would then fork without bound rather
/// than fail. That the spawned process is told it is fronted is covered against
/// a fake server in `kompressor-mcp`'s `tests/proxy.rs`.
///
/// Returns every tool name it listed. With `fronted` set, the process is
/// started as though some other kompressor had spawned it.
fn mcp_proxy_tool_names(fronted: bool) -> Vec<String> {
    let dir = scratch_dir();
    let inner = dir.join("inner.toml");
    std::fs::write(&inner, MCP_PLAIN).unwrap();
    let servers = format!(
        "[[mcp.servers]]\nname = \"inner\"\ncommand = {binary:?}\n\
         args = [\"mcp\", \"serve\", \"--config\", {inner:?}]\n",
        binary = env!("CARGO_BIN_EXE_kompressor"),
        inner = inner.to_str().unwrap(),
    );
    mcp_serve_tools_list(&dir, &servers, fronted).0
}

/// A configuration for `mcp serve` that touches no disk.
const MCP_PLAIN: &str = "[refs]\nbackend = \"memory\"\n\n[stats]\nenabled = false\n";

/// Run `mcp serve` with `servers` appended to [`MCP_PLAIN`], ask `tools/list`,
/// and hang up. Returns the tool names and everything written to stderr.
fn mcp_serve_tools_list(dir: &Path, servers: &str, fronted: bool) -> (Vec<String>, String) {
    use std::io::{BufRead, BufReader, Read};
    use std::process::Stdio;

    let outer = dir.join("mcp-proxy.toml");
    std::fs::write(&outer, format!("{MCP_PLAIN}\n{servers}")).unwrap();

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_kompressor"));
    command
        .args(["mcp", "serve", "--config"])
        .arg(&outer)
        .env_remove("KOMPRESSOR_CONFIG")
        .env_remove("KOMPRESSOR_MCP_FRONTED")
        .env("XDG_CONFIG_HOME", no_config_dir())
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if fronted {
        command.env("KOMPRESSOR_MCP_FRONTED", "1");
    }
    let mut child = command.spawn().expect("mcp serve starts");

    let mut stdin = child.stdin.take().unwrap();
    for message in [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    ] {
        writeln!(stdin, "{message}").unwrap();
    }
    // Stdin stays open until the answer is in: closing it ends the session.
    let listed = BufReader::new(child.stdout.take().unwrap())
        .lines()
        .map_while(Result::ok)
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(&line).expect("one JSON message a line")
        })
        .find(|message| message["id"] == 2)
        .expect("an answer to tools/list");
    drop(stdin);
    let _ = child.wait();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();

    let names = listed["result"]["tools"]
        .as_array()
        .expect("a tool list")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect();
    (names, stderr)
}

/// `mcp serve` with `[[mcp.servers]]` is proxy mode: ours unprefixed, the
/// fronted server's under its name.
#[test]
fn mcp_serve_fronts_the_servers_it_names() {
    let names = mcp_proxy_tool_names(false);
    assert!(
        names.iter().any(|n| n == "optscale_kompressor_retrieve"),
        "ours, unprefixed: {names:?}"
    );
    assert!(
        names
            .iter()
            .any(|n| n == "inner__optscale_kompressor_retrieve"),
        "the fronted server's, prefixed: {names:?}"
    );
}

/// A process started as somebody's fronted server ignores `[[mcp.servers]]`
/// and serves only its own tools: the guard that keeps a self-referential
/// configuration from recursing.
#[test]
fn a_fronted_mcp_serve_ignores_its_own_servers() {
    let names = mcp_proxy_tool_names(true);
    assert!(
        names.iter().any(|n| n == "optscale_kompressor_retrieve"),
        "{names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("__")),
        "nothing may be fronted: {names:?}"
    );
}

/// A server that cannot be fronted says why, on stderr, where a runtime keeps
/// a stdio server's log — and stdout stays nothing but the protocol, which the
/// listing parsing every stdout line as JSON-RPC already holds to.
#[test]
fn mcp_serve_logs_a_server_it_could_not_front_to_stderr() {
    let dir = scratch_dir();
    let (names, stderr) =
        mcp_serve_tools_list(&dir, "[[mcp.servers]]\nname = \"nowhere\"\n", false);
    assert!(
        names.iter().any(|n| n == "optscale_kompressor_retrieve"),
        "{names:?}"
    );
    assert!(
        stderr.contains("mcp server not fronted"),
        "nothing said why: {stderr:?}"
    );
    assert!(
        stderr.contains("nowhere") && stderr.contains("names neither command nor url"),
        "the reason names the server: {stderr:?}"
    );
}

/// `serve` as `kompressor wrap` starts it: stdin is a pipe the wrap holds.
fn supervised_serve() -> std::process::Child {
    supervised_serve_with(&[])
}

fn supervised_serve_with(extra: &[&std::ffi::OsStr]) -> std::process::Child {
    std::process::Command::new(env!("CARGO_BIN_EXE_kompressor"))
        .args([
            "serve",
            "--api",
            "--listen",
            "127.0.0.1:0",
            "--stop-on-stdin",
        ])
        .args(extra)
        .env_remove("KOMPRESSOR_CONFIG")
        .env("XDG_CONFIG_HOME", no_config_dir())
        .current_dir(scratch_dir())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("serve starts")
}

/// Poll for exit, since `std` has no wait with a timeout.
fn exited_within(child: &mut std::process::Child, limit: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    while std::time::Instant::now() < deadline {
        if child.try_wait().expect("the child can be polled").is_some() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

/// The stop request a wrap session sends instead of a signal, which is the only
/// way it can stop its proxy on Windows. Before this, every wrap there left the
/// proxy running after the agent exited.
#[test]
fn a_supervised_proxy_stops_on_the_stop_line() {
    let mut child = supervised_serve();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "stop").unwrap();
    let stopped = exited_within(&mut child, std::time::Duration::from_secs(20));
    if !stopped {
        let _ = child.kill();
    }
    assert!(stopped, "serve ignored the stop line");
    assert!(
        child.wait().unwrap().success(),
        "a requested stop is a clean exit"
    );
}

/// A proxy whose owner has gone (exited while others used it, or was killed)
/// keeps serving while a live session holds its marker, and stops by itself,
/// cleanly, once the last one lets go. Before this nothing ever stopped it: no
/// later session owns it, and on Windows it has no window to close.
#[test]
fn an_orphaned_proxy_stops_once_no_session_uses_it() {
    // An assertion failure must not leave a proxy running after the test.
    struct ReapOnDrop(std::process::Child);
    impl Drop for ReapOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let clients = tempfile::tempdir().unwrap();
    let live = clients.path().join(format!("{}.held", std::process::id()));
    let marker = std::fs::File::create(&live).unwrap();
    marker.lock().unwrap();
    // A dead session's marker: nobody holds its lock, so it must not count.
    // Zero cannot be the proxy's own pid, which the sweep skips.
    let dead = clients.path().join("0.held");
    std::fs::File::create(&dead).unwrap();

    let mut child = ReapOnDrop(supervised_serve_with(&[
        "--stop-when-unused".as_ref(),
        clients.path().as_os_str(),
    ]));
    let child = &mut child.0;
    drop(child.stdin.take());

    // Startup and the first sweep can take more than six seconds on a busy
    // runner. Observe a sweep before starting the keep-alive window; merely
    // seeing a process that has not exited says nothing about its readiness.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while dead.exists() {
        let status = child.try_wait().expect("the child can be polled");
        assert!(
            status.is_none(),
            "proxy exited before its first sweep: {status:?}"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "the dead session's marker was not reaped"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let exited_while_used = exited_within(child, std::time::Duration::from_secs(6));
    assert!(!exited_while_used, "stopped while a live session used it");

    marker.unlock().unwrap();
    drop(marker);
    let stopped = exited_within(child, std::time::Duration::from_secs(20));
    assert!(stopped, "an unused orphaned proxy kept running");
    assert!(
        child.wait().unwrap().success(),
        "stopping when unused is a clean exit"
    );
}

/// What a .NET supervisor writes: a UTF-8 byte-order mark, a line that is not
/// UTF-8 at all, then the stop with a CRLF ending. Neither of the first two may
/// end the listener, or the stop that follows is never heard.
#[test]
fn a_supervised_proxy_stops_after_a_bom_and_a_line_that_is_not_utf8() {
    let mut child = supervised_serve();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"\xFF\xFEnoise\r\n").unwrap();
    stdin.write_all(b"\xEF\xBB\xBFstop\r\n").unwrap();
    stdin.flush().unwrap();
    let stopped = exited_within(&mut child, std::time::Duration::from_secs(20));
    if !stopped {
        let _ = child.kill();
    }
    assert!(stopped, "serve ignored a BOM-prefixed stop line");
    assert!(
        child.wait().unwrap().success(),
        "a requested stop is a clean exit"
    );
}

/// The wrap that started a proxy can exit while another session still uses it.
/// Its end of the pipe closes then, and that must not take the proxy down.
#[test]
fn a_supervised_proxy_outlives_its_supervisor() {
    let mut child = supervised_serve();
    drop(child.stdin.take());
    let exited = exited_within(&mut child, std::time::Duration::from_secs(2));
    let _ = child.kill();
    let _ = child.wait();
    assert!(!exited, "end of input stopped the proxy");
}

/// What `serve` logs to stdout from start to a clean stop, with `NO_COLOR`
/// set to `no_color` or absent.
fn serve_stdout(no_color: Option<&str>) -> String {
    use std::io::Read;
    let dir = scratch_dir();
    // The human-readable format: JSON lines carry no colour to begin with.
    let config = dir.join("text-log.toml");
    std::fs::write(&config, format!("{MCP_PLAIN}\n[log]\nformat = \"text\"\n")).unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_kompressor"));
    command
        .args([
            "serve",
            "--api",
            "--listen",
            "127.0.0.1:0",
            "--stop-on-stdin",
            "--config",
        ])
        .arg(&config)
        .env_remove("KOMPRESSOR_CONFIG")
        .env_remove("RUST_LOG")
        .env_remove("NO_COLOR")
        .env("XDG_CONFIG_HOME", no_config_dir())
        .current_dir(&dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if let Some(value) = no_color {
        command.env("NO_COLOR", value);
    }
    let mut child = command.spawn().expect("serve starts");
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "stop").unwrap();
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    assert!(child.wait().unwrap().success());
    stdout
}

/// `serve` logs in colour unless `NO_COLOR` says not to. Routing `mcp serve`'s
/// log to stderr passed an explicit "colour on" for stdout, which overrides
/// the variable: an operator's `NO_COLOR=1` stopped working for every command
/// that logs.
#[test]
fn serve_honours_no_color_in_its_log() {
    let plain = serve_stdout(Some("1"));
    assert!(plain.contains("listening"), "it did log: {plain:?}");
    assert!(
        !plain.contains('\u{1b}'),
        "NO_COLOR is set and the log is coloured: {plain:?}"
    );
    let coloured = serve_stdout(None);
    assert!(
        coloured.contains('\u{1b}'),
        "the default is unchanged: {coloured:?}"
    );
}

/// `kompressor backup` writes one archive of every database the
/// configuration names, into the directory it is pointed at, and refuses to
/// write over one that is there.
#[test]
fn backup_writes_one_archive_of_every_database_and_never_replaces_one() {
    let dir = scratch_dir();
    let config = dir.join("backup.toml");
    std::fs::write(
        &config,
        format!(
            "[refs]\nbackend = \"sqlite\"\npath = {refs:?}\n\n[stats]\nenabled = true\npath = {stats:?}\n",
            refs = dir.join("refs.db").to_str().unwrap(),
            stats = dir.join("stats.db").to_str().unwrap(),
        ),
    )
    .unwrap();
    let kompressor = || {
        let mut command = Command::cargo_bin("kompressor").unwrap();
        command
            .env_remove("KOMPRESSOR_CONFIG")
            .env("XDG_CONFIG_HOME", no_config_dir())
            .current_dir(&dir);
        command
    };
    // Something to back up: a service that ran creates the statistics
    // database, and a compression stores a fragment.
    kompressor()
        .args([
            "serve",
            "--api",
            "--listen",
            "127.0.0.1:0",
            "--stop-on-stdin",
            "--config",
        ])
        .arg(&config)
        .write_stdin("stop\n")
        .assert()
        .success();
    let payload = dir.join("log.txt");
    std::fs::write(&payload, varying_log()).unwrap();
    kompressor()
        .args(["compress", "--config"])
        .arg(&config)
        .arg(&payload)
        .assert()
        .success();

    let out = dir.join("backups");
    std::fs::create_dir(&out).unwrap();
    let assert = kompressor()
        .args(["backup", "--json", "--config"])
        .arg(&config)
        .arg("--out")
        .arg(&out)
        .assert()
        .success();
    let report: serde_json::Value =
        serde_json::from_slice(&assert.get_output().stdout).expect("one JSON object");
    let archive = PathBuf::from(report["archive"].as_str().unwrap());
    assert_eq!(archive.parent(), Some(out.as_path()));
    let name = archive.file_name().unwrap().to_str().unwrap();
    assert!(
        name.starts_with("kompressor-backup-") && name.ends_with(".tar.gz"),
        "{name}"
    );
    assert_eq!(
        report["bytes"].as_u64(),
        Some(std::fs::metadata(&archive).unwrap().len())
    );
    let names: Vec<&str> = report["manifest"]["databases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|database| database["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["stats.db", "refs.db"]);
    // A gzip and its checksum, in the format `sha256sum -c` reads, and
    // nothing else left beside it.
    assert_eq!(std::fs::read(&archive).unwrap()[..2], [0x1f, 0x8b]);
    assert_eq!(std::fs::read_dir(&out).unwrap().count(), 2);
    let sum = kompressor_core::backup::read_checksum(&archive).expect("a checksum beside it");
    assert_eq!(sum, kompressor_core::backup::sha256_file(&archive).unwrap());
    let line = std::fs::read_to_string(kompressor_core::backup::checksum_path(&archive)).unwrap();
    assert_eq!(line, format!("{sum}  {name}\n"));

    // Named outright, the summary is for a person; asked twice, it refuses.
    let named = dir.join("mine.tar.gz");
    let assert = kompressor()
        .args(["backup", "--config"])
        .arg(&config)
        .arg("-o")
        .arg(&named)
        .assert()
        .success();
    let summary = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(summary.contains("mine.tar.gz"), "{summary}");
    assert!(
        summary.contains("stats.db") && summary.contains("schema v"),
        "{summary}"
    );
    let before = std::fs::read(&named).unwrap();
    let refused = kompressor()
        .args(["backup", "--config"])
        .arg(&config)
        .arg("-o")
        .arg(&named)
        .assert()
        .code(2);
    let said = String::from_utf8(refused.get_output().stderr.clone()).unwrap();
    assert!(said.contains("already exists"), "{said}");
    assert_eq!(std::fs::read(&named).unwrap(), before);
}

/// With nothing on disk to copy, `backup` fails and says why rather than
/// writing an archive that holds no database.
#[test]
fn backup_with_nothing_on_disk_fails_and_writes_nothing() {
    let dir = scratch_dir();
    let config = dir.join("memory.toml");
    std::fs::write(&config, MCP_PLAIN).unwrap();
    let assert = Command::cargo_bin("kompressor")
        .unwrap()
        .args(["backup", "--config"])
        .arg(&config)
        .env_remove("KOMPRESSOR_CONFIG")
        .env("XDG_CONFIG_HOME", no_config_dir())
        .current_dir(&dir)
        .assert()
        .code(1);
    let said = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(said.contains("no database to back up"), "{said}");
    let left: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name != "memory.toml")
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

/// A configuration over on-disk databases in `dir`, holding one fragment
/// and a statistics database a service created.
fn on_disk_deployment(dir: &Path) -> PathBuf {
    let config = dir.join("disk.toml");
    std::fs::write(
        &config,
        format!(
            "[refs]\nbackend = \"sqlite\"\npath = {refs:?}\n\n[stats]\nenabled = true\npath = {stats:?}\n",
            refs = dir.join("refs.db").to_str().unwrap(),
            stats = dir.join("stats.db").to_str().unwrap(),
        ),
    )
    .unwrap();
    run_in(dir)
        .args([
            "serve",
            "--api",
            "--listen",
            "127.0.0.1:0",
            "--stop-on-stdin",
            "--config",
        ])
        .arg(&config)
        .write_stdin("stop\n")
        .assert()
        .success();
    let payload = dir.join("log.txt");
    std::fs::write(&payload, varying_log()).unwrap();
    run_in(dir)
        .args(["compress", "--config"])
        .arg(&config)
        .arg(&payload)
        .assert()
        .success();
    config
}

fn run_in(dir: &Path) -> Command {
    let mut command = Command::cargo_bin("kompressor").unwrap();
    command
        .env_remove("KOMPRESSOR_CONFIG")
        .env("XDG_CONFIG_HOME", no_config_dir())
        .current_dir(dir);
    command
}

fn json_of(assert: &assert_cmd::assert::Assert) -> serde_json::Value {
    serde_json::from_slice(&assert.get_output().stdout).expect("JSON on stdout")
}

/// `kompressor db` describes, checks and checkpoints the databases a
/// configuration names, and `check` fails loudly on a damaged one.
#[test]
fn db_describes_checks_and_checkpoints_the_databases() {
    let dir = scratch_dir();
    let config = on_disk_deployment(&dir);
    let db = |args: &[&str]| {
        let mut command = run_in(&dir);
        command.arg("db").args(args).arg("--config").arg(&config);
        command
    };

    let status = json_of(&db(&["status", "--json"]).assert().success());
    assert_eq!(status[0]["name"], "stats.db");
    assert_eq!(
        status[0]["schema_version"],
        status[0]["expected_schema_version"]
    );
    assert_eq!(status[1]["name"], "refs.db");
    assert!(status[1]["bytes"].as_u64().unwrap() > 0);
    assert!(status[1]["reclaimable_bytes"].is_u64());
    db(&["status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("stats.db").and(predicate::str::contains("schema v")));
    let one = json_of(&db(&["status", "refs.db", "--json"]).assert().success());
    assert_eq!(one.as_array().unwrap().len(), 1);
    db(&["status", "config.toml"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("stats.db and refs.db"));

    let checked = json_of(&db(&["check", "--json"]).assert().success());
    assert!(
        checked
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["ok"] == true)
    );
    db(&["check"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ok"));

    let done = json_of(&db(&["checkpoint", "--json"]).assert().success());
    assert!(
        done.as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["complete"] == true)
    );
    assert_eq!(done[1]["wal_bytes_after"], 0);
    assert!(
        !dir.join("refs.db-wal").exists()
            || std::fs::metadata(dir.join("refs.db-wal")).unwrap().len() == 0
    );

    // Damage the store: `check` says which, in SQLite's words, and exits 1.
    let path = dir.join("refs.db");
    let mut bytes = std::fs::read(&path).unwrap();
    for byte in &mut bytes[4_096..8_192] {
        *byte = 0xAB;
    }
    std::fs::write(&path, bytes).unwrap();
    let failed = db(&["check"]).assert().code(1);
    let said = String::from_utf8(failed.get_output().stdout.clone()).unwrap();
    assert!(
        said.contains("refs.db") && said.contains("DAMAGED"),
        "{said}"
    );
    assert!(
        said.contains("stats.db") && said.contains("ok"),
        "the other is still sound: {said}"
    );
    db(&["check", "stats.db"]).assert().success();
}

/// With nothing on disk, `db` says so for each and fails nothing.
#[test]
fn db_with_nothing_on_disk_says_so() {
    let dir = scratch_dir();
    let config = dir.join("memory.toml");
    std::fs::write(&config, MCP_PLAIN).unwrap();
    for action in ["status", "check", "checkpoint"] {
        run_in(&dir)
            .args(["db", action, "--config"])
            .arg(&config)
            .assert()
            .success()
            .stdout(
                predicate::str::contains("statistics are off")
                    .and(predicate::str::contains("the ref store is in memory")),
            );
    }
}

/// `backup --keep N` rotates the directory it writes into, and removes
/// nothing it did not name itself.
#[test]
fn backup_keep_rotates_the_directory_it_writes_into() {
    let dir = scratch_dir();
    let config = on_disk_deployment(&dir);
    let out = dir.join("backups");
    std::fs::create_dir(&out).unwrap();
    for stale in [
        "kompressor-backup-20200101-000000.tar.gz",
        "kompressor-backup-20210101-000000.tar.gz",
    ] {
        std::fs::write(out.join(stale), b"old").unwrap();
    }
    std::fs::write(out.join("notes.txt"), b"mine").unwrap();
    let report = json_of(
        &run_in(&dir)
            .args(["backup", "--json", "--keep", "2", "--config"])
            .arg(&config)
            .arg("--out")
            .arg(&out)
            .assert()
            .success(),
    );
    assert_eq!(
        report["removed"],
        serde_json::json!(["kompressor-backup-20200101-000000.tar.gz"])
    );
    let mut left: Vec<String> = std::fs::read_dir(&out)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left.len(), 4, "the new archive with its checksum: {left:?}");
    assert_eq!(left[0], "kompressor-backup-20210101-000000.tar.gz");
    assert!(left[2].ends_with(".tar.gz.sha256"), "{left:?}");
    assert_eq!(left[3], "notes.txt");
}

/// How many fragments the store at `path` holds.
fn fragments(dir: &Path, config: &Path) -> u64 {
    let status = json_of(
        &run_in(dir)
            .args(["db", "status", "refs.db", "--json", "--config"])
            .arg(config)
            .assert()
            .success(),
    );
    status[0]["bytes"].as_u64().unwrap()
}

/// `kompressor restore` rehearses by default, restores with `--yes`, moves
/// what was there aside, and the restored deployment works.
#[test]
fn restore_rehearses_then_puts_a_backup_back() {
    let dir = scratch_dir();
    let config = on_disk_deployment(&dir);
    let archive = dir.join("before.tar.gz");
    let taken = json_of(
        &run_in(&dir)
            .args(["backup", "--json", "--config"])
            .arg(&config)
            .arg("-o")
            .arg(&archive)
            .assert()
            .success(),
    );
    assert_eq!(taken["recorded"], true, "the backup is on record");
    let handle = {
        let out = run_in(&dir)
            .args(["compress", "--config"])
            .arg(&config)
            .arg(dir.join("log.txt"))
            .assert()
            .success();
        let text = String::from_utf8(out.get_output().stdout.clone()).unwrap();
        text.split("ref:")
            .nth(1)
            .map(|rest| rest[..14].to_string())
            .expect("a handle")
    };
    // Wreck the store, as a bad disk or a bad day would.
    std::fs::write(dir.join("refs.db"), b"not a database any more").unwrap();
    let _ = std::fs::remove_file(dir.join("refs.db-wal"));
    let _ = std::fs::remove_file(dir.join("refs.db-shm"));
    run_in(&dir)
        .args(["retrieve", "--config"])
        .arg(&config)
        .arg(&handle)
        .assert()
        .failure();

    let restore = |extra: &[&str]| {
        let mut command = run_in(&dir);
        command
            .arg("restore")
            .arg(&archive)
            .arg("--config")
            .arg(&config)
            .args(extra);
        command
    };
    // A rehearsal: checked, described, nothing changed.
    restore(&[]).assert().success().stdout(
        predicate::str::contains("would replace")
            .and(predicate::str::contains("nothing was changed"))
            .and(predicate::str::contains("--yes")),
    );
    assert_eq!(
        std::fs::read(dir.join("refs.db")).unwrap(),
        b"not a database any more"
    );

    let done = json_of(&restore(&["--yes", "--json"]).assert().success());
    assert_eq!(done["applied"], true);
    for database in done["databases"].as_array().unwrap() {
        assert_eq!(database["restored"], true, "{database}");
        let aside = database["moved_aside"]
            .as_str()
            .expect("moved aside, not deleted");
        assert!(aside.contains(".before-restore-"), "{aside}");
        assert!(Path::new(aside).is_file());
    }
    // The wreck is kept beside it, and the fragment is back.
    let aside = done["databases"][1]["moved_aside"].as_str().unwrap();
    assert_eq!(std::fs::read(aside).unwrap(), b"not a database any more");
    run_in(&dir)
        .args(["retrieve", "--config"])
        .arg(&config)
        .arg(&handle)
        .assert()
        .success()
        .stdout(predicate::str::contains("worker"));
    run_in(&dir)
        .args(["db", "check", "--config"])
        .arg(&config)
        .assert()
        .success();
    assert!(fragments(&dir, &config) > 0);

    // One database only, and the things that are refused before anything moves.
    restore(&["--only", "stats.db", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "refs.db   left alone: not asked for",
        ));
    restore(&["--only", "config.toml"]).assert().code(1);
    run_in(&dir)
        .args(["restore", "nowhere.tar.gz", "--config"])
        .arg(&config)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("is not a file"));
    std::fs::write(dir.join("junk.tar.gz"), b"junk").unwrap();
    run_in(&dir)
        .args(["restore", "junk.tar.gz", "--yes", "--config"])
        .arg(&config)
        .assert()
        .code(1);
}

/// With the service running, `restore` refuses and says to stop it.
#[test]
fn restore_refuses_while_the_service_is_running() {
    use std::io::{BufRead, BufReader};
    let dir = scratch_dir();
    let config = on_disk_deployment(&dir);
    let archive = dir.join("b.tar.gz");
    run_in(&dir)
        .args(["backup", "--config"])
        .arg(&config)
        .arg("-o")
        .arg(&archive)
        .assert()
        .success();

    let service = std::process::Command::new(env!("CARGO_BIN_EXE_kompressor"))
        .args([
            "serve",
            "--api",
            "--listen",
            "127.0.0.1:0",
            "--stop-on-stdin",
            "--config",
        ])
        .arg(&config)
        .env_remove("KOMPRESSOR_CONFIG")
        // Readiness below is an INFO line. Under an inherited `RUST_LOG=warn`
        // the service comes up and never says so, and this waited for ever.
        .env("RUST_LOG", "info")
        .env("XDG_CONFIG_HOME", no_config_dir())
        .current_dir(&dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("serve starts");
    // An assertion that fails below must not leave a service holding the
    // directory's databases open after the test has gone.
    struct Reaped(std::process::Child);
    impl Drop for Reaped {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut service = Reaped(service);
    // Up, with its databases open, once it says it is listening.
    let mut lines = BufReader::new(service.0.stdout.take().unwrap()).lines();
    assert!(
        lines
            .by_ref()
            .map_while(Result::ok)
            .any(|line| line.contains("listening")),
        "serve never came up"
    );
    let before = std::fs::read(dir.join("refs.db")).unwrap();
    for extra in [&[][..], &["--yes"][..]] {
        run_in(&dir)
            .arg("restore")
            .arg(&archive)
            .arg("--config")
            .arg(&config)
            .args(extra)
            .assert()
            .code(1)
            .stderr(predicate::str::contains("stop the service first"));
    }
    assert_eq!(std::fs::read(dir.join("refs.db")).unwrap(), before);
    assert!(
        !std::fs::read_dir(&dir).unwrap().flatten().any(|entry| entry
            .file_name()
            .to_string_lossy()
            .contains("before-restore")),
        "nothing was moved"
    );

    writeln!(service.0.stdin.take().unwrap(), "stop").unwrap();
    assert!(service.0.wait().unwrap().success());
    // Stopped, it goes through.
    run_in(&dir)
        .arg("restore")
        .arg(&archive)
        .arg("--yes")
        .arg("--config")
        .arg(&config)
        .assert()
        .success()
        .stdout(predicate::str::contains("Restored. Start the service."));
}
