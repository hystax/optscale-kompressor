//! Hook mode for Cursor, driven as Cursor drives it: one event on stdin, one
//! reply on stdout, exit status 0 whatever happens.
//!
//! The acceptance criteria of docs/design/CURSOR.md §13 that need no live
//! Cursor session are here, named by number. The events are the recorded
//! shapes in `fixtures/cursor-hooks/`. Nothing here touches the real
//! `~/.cursor`: every test points `HOME` and `KOMPRESSOR_HOME` at a scratch
//! directory.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::{Value, json};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/cursor-hooks/2026.09.26-dd393fe")
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(fixtures().join(format!("{name}.json"))).expect("fixture")
}

/// A scratch home, store and configuration.
struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Home {
        let home = Home {
            dir: tempfile::tempdir().unwrap(),
        };
        home.config("");
        home
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn config_path(&self) -> PathBuf {
        self.path().join("kompressor.toml")
    }

    /// Write the configuration: the store and statistics in this home, plus
    /// `extra`.
    fn config(&self, extra: &str) {
        let text = format!(
            "[refs]\npath = {:?}\n[stats]\npath = {:?}\n{extra}",
            self.path().join("refs.db").display().to_string(),
            self.path().join("stats.db").display().to_string(),
        );
        std::fs::write(self.config_path(), text).unwrap();
    }

    fn cmd(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kompressor"));
        command
            .env_remove("KOMPRESSOR_CONFIG")
            .env("HOME", self.path())
            .env("USERPROFILE", self.path())
            .env("KOMPRESSOR_HOME", self.path().join("k"))
            .env("XDG_CONFIG_HOME", self.path().join("xdg"))
            .env("TMPDIR", self.path())
            .current_dir(self.path());
        command
    }

    /// Run `hook cursor` on `input`; the reply and the exit status.
    fn hook(&self, input: &[u8]) -> (Value, i32, String) {
        let out = self
            .cmd()
            .args(["hook", "cursor", "--config"])
            .arg(self.config_path())
            .write_stdin(input.to_vec())
            .output()
            .unwrap();
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert_eq!(stdout.lines().count(), 1, "exactly one line: {stdout:?}");
        let value: Value = serde_json::from_str(&stdout).expect("valid JSON");
        assert!(value.is_object(), "one JSON object: {stdout}");
        (
            value,
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn rows(&self, filter: &str) -> i64 {
        let path = self.path().join("stats.db");
        if !path.exists() {
            return 0;
        }
        rusqlite::Connection::open(path)
            .unwrap()
            .query_row(
                &format!("SELECT COUNT(*) FROM optimization_events WHERE {filter}"),
                [],
                |row| row.get(0),
            )
            .unwrap()
    }
}

fn all_fixtures() -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<_> = std::fs::read_dir(fixtures())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .map(|p| {
            (
                p.file_stem().unwrap().to_string_lossy().into_owned(),
                std::fs::read(&p).unwrap(),
            )
        })
        .collect();
    out.sort();
    assert!(out.len() >= 9, "AC-0.4: the fixture set is complete");
    out
}

/// AC-1.1: every recorded event gets one valid JSON object and exit 0.
#[test]
fn every_fixture_is_answered_with_one_object_and_exit_zero() {
    let home = Home::new();
    for (name, input) in all_fixtures() {
        let (_, code, _) = home.hook(&input);
        assert_eq!(code, 0, "{name}");
    }
}

/// AC-1.2: nothing Cursor could send makes it fail or say anything but `{}`.
#[test]
fn garbage_is_answered_with_nothing() {
    let home = Home::new();
    let garbage: Vec<u8> = (0..10 * 1024 * 1024u32)
        .map(|i| (i * 7 % 251) as u8)
        .collect();
    for input in [
        Vec::new(),
        br#"{"hook_event_name":"postToolUse","tool_name":"MCP:x","tool_output":"#.to_vec(),
        b"[1,2,3]".to_vec(),
        br#"{"hook_event_name":"somethingNew"}"#.to_vec(),
        garbage,
    ] {
        let (reply, code, _) = home.hook(&input);
        assert_eq!(reply, json!({}));
        assert_eq!(code, 0);
    }
}

/// AC-1.3: a deadline that cannot be met answers `{}` and still exits 0.
#[test]
fn a_missed_deadline_answers_nothing() {
    let home = Home::new();
    home.config("[hooks.cursor]\ndeadline = \"1ms\"\n");
    let started = std::time::Instant::now();
    let (reply, code, stderr) = home.hook(&fixture("postToolUse-MCP-text"));
    assert_eq!(code, 0);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    if reply != json!({}) {
        // A fast machine can finish inside a millisecond; then the reply is
        // the real one, never a partial one.
        assert!(reply.get("updated_mcp_tool_output").is_some());
    } else {
        assert!(stderr.contains("gave up"), "{stderr}");
    }
}

/// AC-1.4: switched off, nothing is answered and nothing recorded.
#[test]
fn switched_off_it_answers_nothing_and_records_nothing() {
    let home = Home::new();
    home.config("[hooks.cursor]\nenabled = false\n");
    for (name, input) in all_fixtures() {
        let (reply, _, _) = home.hook(&input);
        assert_eq!(reply, json!({}), "{name}");
    }
    assert!(!home.path().join("stats.db").exists());
}

/// AC-1.5 and AC-1.6: shadow mode changes nothing and records one row per
/// tool result, with nothing retained.
#[test]
fn shadow_mode_measures_every_tool_and_changes_nothing() {
    let home = Home::new();
    home.config("[hooks.cursor]\nshadow = true\n[hooks.cursor.shell]\nenabled = true\nonly_when_approved = false\n");
    let mut post = 0;
    for (name, input) in all_fixtures() {
        let (reply, _, _) = home.hook(&input);
        assert!(reply.get("updated_input").is_none(), "{name}");
        assert!(reply.get("updated_mcp_tool_output").is_none(), "{name}");
        assert!(reply.get("additional_context").is_none(), "{name}");
        if name.starts_with("postToolUse") {
            post += 1;
        }
    }
    assert_eq!(
        home.rows("surface = 'hook' AND mode = 'off' AND provider = 'cursor'"),
        post
    );
    assert_eq!(home.rows("tool = 'Read'"), 1);
    assert_eq!(
        home.rows("json_extract(skipped_by_reason, '$.spilled') = 1"),
        1,
        "spilled MCP results are counted per tool"
    );
    assert!(!home.path().join("refs.db").exists(), "nothing retained");
}

/// AC-1.7 and AC-4.7: an unreachable store answers `{}` with one line on
/// standard error.
#[test]
fn an_unreachable_store_answers_nothing() {
    let home = Home::new();
    std::fs::write(
        home.config_path(),
        "[refs]\npath = \"/proc/kompressor-nope/refs.db\"\n[stats]\npath = \"/proc/kompressor-nope/stats.db\"\n",
    )
    .unwrap();
    let (reply, code, stderr) = home.hook(&fixture("postToolUse-MCP-text"));
    assert_eq!(reply, json!({}));
    assert_eq!(code, 0);
    assert!(stderr.contains("kompressor hook:"), "{stderr}");
}

/// AC-4.1 and AC-4.2: an MCP result is compressed, and the handle in it
/// resolves from another directory with the same configuration.
#[test]
fn an_mcp_result_is_compressed_and_its_handle_retrievable() {
    let home = Home::new();
    let input = fixture("postToolUse-MCP-text");
    let event: Value = serde_json::from_slice(&input).unwrap();
    let original: Value = serde_json::from_str(event["tool_output"].as_str().unwrap()).unwrap();
    let original = original["content"][0]["text"].as_str().unwrap().to_string();

    let (reply, code, _) = home.hook(&input);
    assert_eq!(code, 0);
    let text = reply["updated_mcp_tool_output"]["content"][0]["text"]
        .as_str()
        .expect("a compressed result")
        .to_string();
    assert!(text.len() < original.len());
    let handle = text
        .split("ref:")
        .nth(1)
        .and_then(|rest| rest.get(..14))
        .expect("a handle")
        .to_string();
    assert!(handle.starts_with("k_"), "{handle}");
    assert_eq!(
        home.rows("surface = 'hook' AND mode = 'recoverable' AND tool = 'MCP:big_text'"),
        1
    );

    let elsewhere = tempfile::tempdir().unwrap();
    let out = home
        .cmd()
        .current_dir(elsewhere.path())
        .args(["retrieve", &handle, "--config"])
        .arg(home.config_path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let fragment = String::from_utf8(out.stdout).unwrap();
    assert!(!fragment.trim().is_empty());
    for line in fragment.lines().filter(|l| !l.trim().is_empty()).take(3) {
        assert!(
            original.contains(line.trim()),
            "{line:?} is not from the original"
        );
    }
}

/// AC-4.4 and AC-4.5: failed, spilled and our own results are never replaced.
#[test]
fn results_that_are_not_ours_to_touch_are_left_alone() {
    let home = Home::new();
    for name in [
        "postToolUse-MCP-failed",
        "postToolUse-MCP-spilled",
        "postToolUse-Read",
        "postToolUse-Shell",
    ] {
        let (reply, _, _) = home.hook(&fixture(name));
        assert_eq!(reply, json!({}), "{name}");
    }
    let mut own: Value = serde_json::from_slice(&fixture("postToolUse-MCP-text")).unwrap();
    own["tool_name"] = json!("MCP:optscale_kompressor_retrieve");
    let (reply, _, _) = home.hook(&serde_json::to_vec(&own).unwrap());
    assert_eq!(reply, json!({}));
    let mut error: Value = serde_json::from_slice(&fixture("postToolUse-MCP-text")).unwrap();
    let mut body: Value = serde_json::from_str(error["tool_output"].as_str().unwrap()).unwrap();
    body["isError"] = json!(true);
    error["tool_output"] = json!(body.to_string());
    let (reply, _, _) = home.hook(&serde_json::to_vec(&error).unwrap());
    assert_eq!(reply, json!({}));
}

/// AC-4.6: a result the engine cannot shrink is never answered.
#[test]
fn an_incompressible_result_is_left_alone() {
    let home = Home::new();
    let mut event: Value = serde_json::from_slice(&fixture("postToolUse-MCP-text")).unwrap();
    let noise: String = (0..3000u32)
        .map(|i| char::from(b'a' + (i.wrapping_mul(2_654_435_761) >> 27) as u8 % 26))
        .collect();
    event["tool_output"] =
        json!(json!({"content": [{"type": "text", "text": noise}], "isError": false}).to_string());
    let (reply, _, _) = home.hook(&serde_json::to_vec(&event).unwrap());
    if let Some(text) = reply["updated_mcp_tool_output"]["content"][0]["text"].as_str() {
        assert!(text.len() < 3000);
    } else {
        assert_eq!(reply, json!({}));
    }
}

#[test]
fn session_start_offers_the_config_and_the_retrieval_fallback() {
    let home = Home::new();
    let (reply, _, _) = home.hook(&fixture("sessionStart"));
    assert_eq!(
        reply["env"]["KOMPRESSOR_CONFIG"],
        home.config_path().display().to_string()
    );
    let context = reply["additional_context"].as_str().unwrap();
    assert!(context.contains("hook") || context.contains("retrieve"));
    assert!(context.contains(" retrieve --config "));
}

#[test]
fn the_desktop_stop_event_records_the_turns_usage() {
    let home = Home::new();
    let (reply, _, _) = home.hook(&fixture("stop-desktop"));
    assert_eq!(reply, json!({}));
    let conn = rusqlite::Connection::open(home.path().join("stats.db")).unwrap();
    let (prompt, read): (i64, i64) = conn
        .query_row(
            "SELECT prompt_tokens, cache_read_tokens FROM proxy_requests WHERE api_family = 'cursor'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((prompt, read), (103_689, 91_776));
}

// ------------------------------------------------------------------ shell ---

/// The rewrite for `command`, from the hook itself.
#[cfg(unix)]
fn rewritten(home: &Home, command: &str, id: &str) -> Option<String> {
    let mut event: Value = serde_json::from_slice(&fixture("preToolUse-Shell")).unwrap();
    event["tool_input"]["command"] = json!(command);
    event["tool_use_id"] = json!(id);
    let (reply, _, _) = home.hook(&serde_json::to_vec(&event).unwrap());
    reply["updated_input"]["command"]
        .as_str()
        .map(str::to_string)
}

#[cfg(unix)]
fn shells() -> Vec<&'static str> {
    ["sh", "bash", "dash", "zsh"]
        .into_iter()
        .filter(|s| {
            std::process::Command::new(s)
                .args(["-c", "true"])
                .status()
                .is_ok_and(|st| st.success())
        })
        .collect()
}

/// Shell rewriting is off by default, and refuses outside unrestricted
/// approval unless told otherwise.
#[cfg(unix)]
#[test]
fn the_shell_rewrite_is_off_by_default() {
    let home = Home::new();
    assert_eq!(rewritten(&home, "seq 1 30", "t0"), None);
    home.config("[hooks.cursor.shell]\nenabled = true\n");
    assert_eq!(
        rewritten(&home, "seq 1 30", "t1"),
        None,
        "approvalMode is not unrestricted"
    );
    std::fs::create_dir_all(home.path().join(".cursor")).unwrap();
    std::fs::write(
        home.path().join(".cursor/cli-config.json"),
        r#"{"approvalMode":"unrestricted"}"#,
    )
    .unwrap();
    assert!(rewritten(&home, "seq 1 30", "t2").is_some());
}

/// Q1, measured on CLI 2026.11.22: the allow-list judges the rewritten
/// command's words literally, so the rewrite is offered under an allow-list
/// only when that list approves this binary as the rewrite names it.
#[cfg(unix)]
#[test]
fn under_an_allow_list_the_rewrite_waits_for_this_binary_to_be_approved() {
    let home = Home::new();
    home.config("[hooks.cursor.shell]\nenabled = true\n");
    let exe = std::fs::canonicalize(env!("CARGO_BIN_EXE_kompressor")).unwrap();
    let entry = format!("Shell({})", exe.display());
    let cli = home.path().join(".cursor/cli-config.json");
    std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
    let global = |allow: &[&str], deny: &[&str]| {
        std::fs::write(
            &cli,
            json!({"approvalMode": "allowlist", "permissions": {"allow": allow, "deny": deny}})
                .to_string(),
        )
        .unwrap();
    };
    global(&["Shell(seq)"], &[]);
    assert_eq!(rewritten(&home, "seq 1 30", "a1"), None, "would be refused");

    global(&["Shell(seq)", &entry], &[]);
    let command = rewritten(&home, "seq 1 30", "a2").expect("approved, so rewritten");
    assert!(
        command.contains(&format!("; {} hook emit", exe.display())),
        "named unquoted, as the entry matches it: {command}"
    );

    global(&["Shell(seq)", &entry], &[&entry]);
    assert_eq!(rewritten(&home, "seq 1 30", "a3"), None, "deny wins");

    // A workspace's .cursor/cli.json counts as well — and replaces the
    // global allow-list rather than adding to it, as the CLI's merge does.
    global(&["Shell(seq)", &entry], &[]);
    let replaced = home.path().join("replaced");
    std::fs::create_dir_all(replaced.join(".cursor")).unwrap();
    std::fs::write(
        replaced.join(".cursor/cli.json"),
        json!({"permissions": {"allow": ["Shell(seq)"]}}).to_string(),
    )
    .unwrap();
    let mut event: Value = serde_json::from_slice(&fixture("preToolUse-Shell")).unwrap();
    event["tool_use_id"] = json!("a5");
    event["workspace_roots"] = json!([replaced.display().to_string()]);
    let (reply, _, _) = home.hook(&serde_json::to_vec(&event).unwrap());
    assert_eq!(reply, json!({}), "the project list replaced the global one");

    global(&["Shell(seq)"], &[]);
    let workspace = home.path().join("proj");
    std::fs::create_dir_all(workspace.join(".cursor")).unwrap();
    std::fs::write(
        workspace.join(".cursor/cli.json"),
        json!({"permissions": {"allow": [&entry]}}).to_string(),
    )
    .unwrap();
    let mut event: Value = serde_json::from_slice(&fixture("preToolUse-Shell")).unwrap();
    event["tool_use_id"] = json!("a4");
    event["workspace_roots"] = json!([workspace.display().to_string()]);
    let (reply, _, _) = home.hook(&serde_json::to_vec(&event).unwrap());
    assert!(reply["updated_input"]["command"].is_string(), "{reply}");
}

/// AC-5.3: with compression disabled, a rewritten command prints exactly what
/// the bare one prints and exits with its status, in every shell present.
#[cfg(unix)]
#[test]
fn a_rewritten_command_is_equivalent_to_the_bare_one() {
    let home = Home::new();
    home.config("[hooks.cursor.shell]\nenabled = true\nonly_when_approved = false\nskip = []\n");
    let work = home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
    let commands = [
        "seq 1 50",
        "ls",
        "ls /nonexistent-kompressor",
        "cat a.txt",
        "cat a.txt | sort -r",
        "grep beta a.txt",
        "grep nothing a.txt",
        "echo out; echo err >&2",
        "printf 'a\\nb\\n' | wc -l",
        "false",
        "true",
        "seq 1 3 && false",
        "false; echo after",
        "test -f a.txt && echo yes",
        "echo \"quoted $((1+2))\"",
        "echo 'single $HOME'",
        "for i in 1 2 3; do echo $i; done",
        "if true; then echo t; else echo f; fi",
        "x=5; echo $x",
        "f() { echo in-f; }; f",
        "cat <<EOF\nheredoc line\nEOF",
        "cat <<'EOF'\n$not expanded\nEOF",
        "echo a # comment",
        "ls \\\n  a.txt",
        "seq 1 3 | while read n; do echo n$n; done",
        "{ echo grouped; }",
        "(echo sub; exit 3)",
        "echo $(echo nested)",
        "printf '%s\\n' one two",
        "sh -c 'exit 7'",
        "wc -c a.txt",
        "head -n 1 a.txt",
        "tail -n 1 a.txt",
        "sort a.txt | uniq",
        "seq 1 1000 | tail -n 2",
        "echo -n no-newline",
        "printf 'tab\\there\\n'",
        "seq 5 -1 1",
        "date -u +%Y >/dev/stderr",
        "awk 'BEGIN { print 42 }'",
    ];
    assert!(commands.len() >= 40);
    // Compression off: `emit` prints the capture unchanged.
    home.config(
        "[hooks.cursor]\nenabled = true\n[hooks.cursor.shell]\nenabled = true\nonly_when_approved = false\nskip = []\n",
    );
    let off = home.path().join("off.toml");
    std::fs::write(&off, "[hooks.cursor]\nenabled = false\n").unwrap();
    let mut exercised = 0;
    for (n, command) in commands.iter().enumerate() {
        let Some(rewrite) = rewritten(&home, command, &format!("eq{n}")) else {
            continue;
        };
        exercised += 1;
        // Point emit at the switched-off configuration.
        let rewrite = rewrite.replace(
            &format!("--config '{}'", home.config_path().display()),
            &format!("--config '{}'", off.display()),
        );
        for shell in shells() {
            let run = |script: &str| {
                std::process::Command::new(shell)
                    .args(["-c", &format!("{script}\n")])
                    .current_dir(&work)
                    .env("HOME", home.path())
                    .output()
                    .unwrap()
            };
            let bare = run(&format!("{{\n{command}\n}} 2>&1"));
            // A fresh capture file per shell: emit deletes it.
            let capture = rewrite.rsplit('\'').nth(1).unwrap().to_string();
            std::fs::write(&capture, "").unwrap();
            let wrapped = run(&rewrite);
            assert_eq!(
                String::from_utf8_lossy(&wrapped.stdout),
                String::from_utf8_lossy(&bare.stdout),
                "{shell}: output of {command:?}"
            );
            assert_eq!(
                wrapped.status.code(),
                bare.status.code(),
                "{shell}: status of {command:?}"
            );
            assert!(
                !Path::new(&capture).exists(),
                "AC-5.7: emit removes the capture"
            );
        }
    }
    assert!(exercised >= 38, "only {exercised} commands were rewritten");
}

/// AC-5.4: `cd` and `export` inside the group are seen by the next command.
#[cfg(unix)]
#[test]
fn shell_state_survives_the_brace_group() {
    let home = Home::new();
    home.config("[hooks.cursor.shell]\nenabled = true\nonly_when_approved = false\nskip = []\n");
    let target = home.path().join("elsewhere");
    std::fs::create_dir_all(&target).unwrap();
    for (n, shell) in shells()
        .into_iter()
        .filter(|s| *s == "bash" || *s == "zsh")
        .enumerate()
    {
        let cd = rewritten(
            &home,
            &format!("cd '{}'", target.display()),
            &format!("cd{n}"),
        )
        .unwrap();
        let export = rewritten(&home, "export KOMPRESSOR_X=1", &format!("ex{n}")).unwrap();
        let out = std::process::Command::new(shell)
            .args([
                "-c",
                &format!("{cd}\n{export}\npwd; echo \"x=$KOMPRESSOR_X\""),
            ])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains(&target.display().to_string()),
            "{shell}: {text}"
        );
        assert!(text.contains("x=1"), "{shell}: {text}");
    }
}

/// AC-5.6: whatever goes wrong, `emit` exits with the status it was given and
/// never 0 for a failure.
#[cfg(unix)]
#[test]
fn emit_always_exits_with_the_commands_status() {
    let home = Home::new();
    let missing = home
        .cmd()
        .args(["hook", "emit", "--status", "3"])
        .arg(home.path().join("nope"))
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(3));

    let big = home.path().join("big.out");
    std::fs::write(&big, "x".repeat(2048)).unwrap();
    home.config("[hooks.cursor.shell]\nmax_capture_bytes = 1024\n");
    let out = home
        .cmd()
        .args(["hook", "emit", "--status", "2", "--config"])
        .arg(home.config_path())
        .arg(&big)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(out.stdout.len(), 2048, "printed whole, unchanged");
    assert!(!big.exists());

    // An unreachable store: lossless, still the status.
    let log = home.path().join("log.out");
    let text: String = (0..2000)
        .map(|i| format!("INFO step {} done\n", i % 3))
        .collect();
    std::fs::write(&log, &text).unwrap();
    std::fs::write(
        home.config_path(),
        "[refs]\npath = \"/proc/kompressor-nope/refs.db\"\n[stats]\npath = \"/proc/kompressor-nope/stats.db\"\n",
    )
    .unwrap();
    let out = home
        .cmd()
        .args(["hook", "emit", "--status", "1", "--config"])
        .arg(home.config_path())
        .arg(&log)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let printed = String::from_utf8_lossy(&out.stdout);
    assert!(
        !printed.contains("ref:k_"),
        "no handle that could not be kept"
    );
    assert!(!printed.is_empty());
}

#[cfg(unix)]
#[test]
fn emit_compresses_a_large_capture_with_a_working_handle() {
    let home = Home::new();
    let file = home.path().join("seq.out");
    let text: String = (1..=20000).map(|i| format!("{i}\n")).collect();
    std::fs::write(&file, &text).unwrap();
    let out = home
        .cmd()
        .args([
            "hook",
            "emit",
            "--status",
            "0",
            "--request",
            "gen-1",
            "--config",
        ])
        .arg(home.config_path())
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let printed = String::from_utf8(out.stdout).unwrap();
    assert!(printed.len() < text.len() / 10, "{} bytes", printed.len());
    assert!(printed.contains("ref:k_"));
    assert_eq!(
        home.rows("surface = 'hook' AND tool = 'Shell' AND request_id = 'gen-1'"),
        1
    );
}

/// Over Cursor's 40 KB spill threshold the model would have received a file
/// reference, not the output: no tokens are claimed for it, and it is counted
/// as kept inline instead. Under the threshold the removal is claimed as
/// usual. Measured on the real CLI: `seq 1 20000` was being reported as
/// ~78k tokens removed that the model would never have read.
#[cfg(unix)]
#[test]
fn removal_is_counted_only_against_what_would_have_been_inline() {
    let home = Home::new();
    let emit = |name: &str, text: &str, request: &str| {
        let file = home.path().join(name);
        std::fs::write(&file, text).unwrap();
        let out = home
            .cmd()
            .args([
                "hook",
                "emit",
                "--status",
                "0",
                "--request",
                request,
                "--config",
            ])
            .arg(home.config_path())
            .arg(&file)
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    };
    let large: String = (1..=20000).map(|i| format!("{i}\n")).collect();
    assert!(large.len() > 40_000);
    let small: String = (1..=5000).map(|i| format!("{i}\n")).collect();
    assert!(small.len() < 40_000);
    let printed = emit("large.out", &large, "big");
    assert!(printed.len() < 1000, "still compressed for the model");
    emit("small.out", &small, "small");

    let conn = rusqlite::Connection::open(home.path().join("stats.db")).unwrap();
    let row = |id: &str| -> (i64, i64, i64) {
        conn.query_row(
            "SELECT tokens_in, tokens_out, chars_in FROM optimization_events WHERE request_id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    };
    let (tin, tout, chars) = row("big");
    assert_eq!(tin, tout, "nothing claimed over the threshold");
    assert_eq!(chars, large.chars().count() as i64, "the real size is kept");
    let (tin, tout, _) = row("small");
    assert!(tin > tout, "under the threshold the removal is claimed");

    let reader = kompressor_stats::StatsReader::open(home.path().join("stats.db")).unwrap();
    let tools = reader.by_tool(0, i64::MAX).unwrap();
    let shell = tools.iter().find(|t| t.tool == "Shell").unwrap();
    assert_eq!(shell.kept_inline, 1);
    assert_eq!(shell.results, 2);
}

// ---------------------------------------------------------------- install ---

fn cursor_files(home: &Home) -> (Option<String>, Option<String>) {
    let read = |name: &str| std::fs::read_to_string(home.path().join(".cursor").join(name)).ok();
    (read("hooks.json"), read("mcp.json"))
}

fn install(home: &Home, extra: &[&str]) -> std::process::Output {
    home.cmd()
        .args(["hook", "install", "cursor", "--config"])
        .arg(home.config_path())
        .args(extra)
        .output()
        .unwrap()
}

fn uninstall(home: &Home) -> std::process::Output {
    home.cmd()
        .args(["hook", "uninstall", "cursor"])
        .output()
        .unwrap()
}

/// AC-2.1, AC-4.8: install then uninstall leaves both files as they were,
/// from every starting state.
#[test]
fn install_and_uninstall_round_trip_byte_for_byte() {
    let states: Vec<(Option<&str>, Option<&str>, &[&str])> = vec![
        (None, None, &[]),
        (Some("{}"), Some("{}"), &[]),
        (
            Some(
                "{\n  \"version\": 1,\n  \"hooks\": {\"afterFileEdit\": [{\"command\": \"fmt\"}]}\n}\n",
            ),
            Some("{\"mcpServers\": {\"github\": {\"command\": \"gh-mcp\"}}}"),
            &[],
        ),
        (
            Some(
                "{\"version\":1,\"hooks\":{\"postToolUse\":[{\"command\":\"audit\"}],\"sessionStart\":[{\"command\":\"hello\"}]}}",
            ),
            None,
            &[],
        ),
        (
            Some(
                "{\"version\":1,\"hooks\":{\"preToolUse\":[{\"command\":\"rtk hook\",\"matcher\":\"Shell\"}]}}",
            ),
            None,
            &["--force"],
        ),
    ];
    for (hooks, mcp, flags) in states {
        for shadow in [false, true] {
            let home = Home::new();
            let dir = home.path().join(".cursor");
            if hooks.is_some() || mcp.is_some() {
                std::fs::create_dir_all(&dir).unwrap();
            }
            if let Some(text) = hooks {
                std::fs::write(dir.join("hooks.json"), text).unwrap();
            }
            if let Some(text) = mcp {
                std::fs::write(dir.join("mcp.json"), text).unwrap();
            }
            let before = cursor_files(&home);
            let mut args = flags.to_vec();
            if shadow {
                args.push("--shadow");
            }
            let out = install(&home, &args);
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let (installed, servers) = cursor_files(&home);
            let installed: Value = serde_json::from_str(&installed.unwrap()).unwrap();
            for event in ["sessionStart", "preToolUse", "postToolUse", "stop"] {
                assert!(
                    installed["hooks"][event]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|e| e["command"].as_str().unwrap().contains(" hook cursor")),
                    "{event}"
                );
            }
            if !shadow {
                let servers: Value = serde_json::from_str(&servers.unwrap()).unwrap();
                assert!(
                    servers["mcpServers"]["optscale_kompressor"]["args"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("serve"))
                );
            }
            // AC-2.2: twice is the same as once.
            let first = cursor_files(&home);
            assert!(install(&home, &args).status.success());
            assert_eq!(cursor_files(&home), first);

            assert!(uninstall(&home).status.success());
            assert_eq!(
                cursor_files(&home),
                before,
                "{hooks:?} / {mcp:?} shadow={shadow}"
            );
            assert!(uninstall(&home).status.success(), "twice exits 0");
        }
    }
}

/// AC-2.3: an entry the user edited is reported and left.
#[test]
fn an_edited_entry_survives_uninstall_and_is_named() {
    let home = Home::new();
    assert!(install(&home, &[]).status.success());
    let path = home.path().join(".cursor/hooks.json");
    let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    doc["hooks"]["stop"][0]["timeout"] = json!(9);
    std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    let out = uninstall(&home);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("hooks.stop"), "{stderr}");
    let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(after["hooks"]["stop"][0]["timeout"], 9);
    assert!(
        after["hooks"].get("sessionStart").is_none(),
        "the untouched ones are gone"
    );
}

/// AC-2.4: `--print` writes nothing and prints JSON.
#[test]
fn print_writes_nothing() {
    let home = Home::new();
    let out = install(&home, &["--print"]);
    assert!(out.status.success());
    let printed: Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert!(printed["hooks.json"]["hooks"]["preToolUse"].is_array());
    assert_eq!(cursor_files(&home), (None, None));
    assert!(!home.path().join("k").exists());
}

/// AC-2.5: beside another Shell hook, install refuses, writes nothing and
/// names it.
#[test]
fn install_refuses_to_stack_on_another_shell_rewriter() {
    let home = Home::new();
    let dir = home.path().join(".cursor");
    std::fs::create_dir_all(&dir).unwrap();
    let text = "{\"version\":1,\"hooks\":{\"preToolUse\":[{\"command\":\"rtk hook cursor-shell\",\"matcher\":\"^Shell$\"}]}}";
    std::fs::write(dir.join("hooks.json"), text).unwrap();
    let out = install(&home, &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("rtk hook cursor-shell"));
    assert_eq!(
        std::fs::read_to_string(dir.join("hooks.json")).unwrap(),
        text
    );
    assert!(!dir.join("mcp.json").exists());
}

#[test]
fn status_reports_the_install_and_the_last_call() {
    let home = Home::new();
    assert!(install(&home, &[]).status.success());
    let (_, _, _) = home.hook(&fixture("sessionStart"));
    let out = home
        .cmd()
        .args(["hook", "status", "cursor"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("installed     yes"), "{text}");
    assert!(text.contains("(reachable)"), "{text}");
    assert!(text.contains("sessionStart"), "{text}");
    assert!(!text.contains("never"), "{text}");
}

// ------------------------------------------------------------------- wrap ---

/// A stub `cursor-agent` that records its arguments and what the plugin
/// directory held while it ran.
#[cfg(unix)]
fn stub(home: &Home, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let path = bin.join("cursor-agent");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// AC-2.6: the stub gets `--plugin-dir <dir>` first, the directory holds the
/// plugin with absolute paths, no proxy is started, and the directory is gone
/// afterwards.
#[cfg(unix)]
#[test]
fn wrap_hands_one_session_a_plugin_and_starts_no_proxy() {
    let home = Home::new();
    let log = home.path().join("stub.log");
    let bin = stub(
        &home,
        &format!(
            "echo \"$@\" > '{l}'; find \"$2\" -type f | sort >> '{l}'; cat \"$2/hooks/hooks.json\" >> '{l}'; exit 4",
            l = log.display()
        ),
    );
    let out = home
        .cmd()
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .args(["wrap", "cursor-agent", "--config"])
        .arg(home.config_path())
        .args(["--", "-p", "hello"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4), "the agent's status is ours");
    let recorded = std::fs::read_to_string(&log).unwrap();
    let first = recorded.lines().next().unwrap();
    assert!(first.starts_with("--plugin-dir /"), "{first}");
    assert!(first.ends_with(" -p hello"), "{first}");
    let dir = first.split_whitespace().nth(1).unwrap();
    for file in [".cursor-plugin/plugin.json", "hooks/hooks.json", "mcp.json"] {
        assert!(
            recorded.contains(&format!("{dir}/{file}")),
            "{file}: {recorded}"
        );
    }
    assert!(recorded.contains(" hook cursor --config '/"), "{recorded}");
    assert!(!Path::new(dir).exists(), "removed after the agent exits");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("proxy"), "no proxy: {stderr}");
    assert!(
        !home.path().join("k/logs").exists(),
        "no proxy log was opened"
    );
}

/// AC-2.8: with an install present, no second set of hooks is passed.
#[cfg(unix)]
#[test]
fn wrap_adds_no_plugin_beside_an_install() {
    let home = Home::new();
    assert!(install(&home, &[]).status.success());
    let log = home.path().join("stub.log");
    let bin = stub(&home, &format!("echo \"$@\" > '{}'", log.display()));
    let out = home
        .cmd()
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .args(["wrap", "cursor-agent", "--", "-p", "x"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(std::fs::read_to_string(&log).unwrap().trim(), "-p x");
    assert!(String::from_utf8_lossy(&out.stderr).contains("already installed"));
}

/// AC-2.9: the catalogue no longer says Cursor cannot be compressed, and
/// `wrap cursor` prints no loopback URL.
#[test]
fn the_catalogue_tells_the_truth_about_cursor() {
    let home = Home::new();
    let list = home.cmd().args(["wrap", "--list"]).output().unwrap();
    let text = String::from_utf8_lossy(&list.stdout);
    assert!(text.contains("cursor-agent"));
    assert!(!text.contains("cannot be compressed"), "{text}");
    let editor = home.cmd().args(["wrap", "cursor"]).output().unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&editor.stdout),
        String::from_utf8_lossy(&editor.stderr)
    );
    assert!(!all.contains("127.0.0.1"), "{all}");
    assert!(all.contains("hook install cursor"), "{all}");
}

/// AC-7.3, the data half: the Cursor report reads real rows, and reads
/// nothing as an empty report rather than an error.
#[test]
fn the_cursor_report_reads_hook_rows_and_desktop_usage() {
    let home = Home::new();
    home.hook(&fixture("sessionStart"));
    let empty = kompressor_stats::StatsReader::open(home.path().join("stats.db"));
    if let Ok(reader) = empty {
        let report = reader.cursor("", 0, i64::MAX).unwrap();
        assert!(report.tools.is_empty() && report.conversations.is_empty());
    }
    home.hook(&fixture("postToolUse-MCP-text"));
    home.hook(&fixture("stop-desktop"));
    let reader = kompressor_stats::StatsReader::open(home.path().join("stats.db")).unwrap();
    let report = reader.cursor("", 0, i64::MAX).unwrap();
    assert_eq!(report.tools.len(), 1);
    assert_eq!(report.tools[0].tool, "MCP:big_text");
    assert_eq!(report.tools[0].compressed_results, 1);
    assert!(report.tools[0].tokens_removed > 0);
    assert_eq!(report.conversations.len(), 1);
    assert_eq!(report.conversations[0].input_tokens, 103_689);

    let out = home
        .cmd()
        .args(["stats", "by-tool", "--format", "json", "--stats"])
        .arg(home.path().join("stats.db"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(rows[0]["tool"], "MCP:big_text");
}

/// The shadow path counts the same way: a shell result over the spill
/// threshold says what the model would have read inline, which is nothing.
#[test]
fn shadow_rows_claim_nothing_over_the_spill_threshold() {
    let home = Home::new();
    home.config("[hooks.cursor]\nshadow = true\n");
    let mut event: Value = serde_json::from_slice(&fixture("postToolUse-Shell")).unwrap();
    let output: String = (1..=20000).map(|i| format!("{i}\n")).collect();
    event["tool_output"] = json!(json!({"output": output, "exitCode": 0}).to_string());
    let (reply, _, _) = home.hook(&serde_json::to_vec(&event).unwrap());
    assert_eq!(reply, json!({}));
    assert_eq!(
        home.rows(
            "tool = 'Shell' AND mode = 'off' AND tokens_in = tokens_out AND chars_in > 40000"
        ),
        1
    );
    let reader = kompressor_stats::StatsReader::open(home.path().join("stats.db")).unwrap();
    let shell = reader
        .by_tool(0, i64::MAX)
        .unwrap()
        .into_iter()
        .find(|t| t.tool == "Shell")
        .unwrap();
    assert_eq!(shell.shadow_would_remove, 0);
    assert_eq!(shell.kept_inline, 1);
}

/// A result Cursor spilled to a file is counted per tool in compressing mode
/// too: that count is the list of servers worth fronting, and it is read in
/// compressing mode, not only in shadow. Found on the VPS: a 45 KB result
/// left no trace.
#[test]
fn a_spilled_result_is_counted_while_compressing() {
    let home = Home::new();
    let (reply, _, _) = home.hook(&fixture("postToolUse-MCP-spilled"));
    assert_eq!(reply, json!({}));
    assert_eq!(
        home.rows("tool = 'MCP:big_text' AND json_extract(skipped_by_reason, '$.spilled') = 1"),
        1
    );
    let reader = kompressor_stats::StatsReader::open(home.path().join("stats.db")).unwrap();
    let row = reader.by_tool(0, i64::MAX).unwrap().remove(0);
    assert_eq!((row.spilled, row.tokens_removed), (1, 0));
}

/// Two installs Cursor would both read — the user's and a workspace's —
/// run every hook twice and count every result twice. Measured on the VPS;
/// install now refuses the second, names the first, and status warns.
#[test]
fn a_second_install_cursor_would_also_read_is_refused() {
    let home = Home::new();
    let workspace = home.path().join("proj");
    std::fs::create_dir_all(&workspace).unwrap();
    assert!(install(&home, &[]).status.success());
    let out = home
        .cmd()
        .current_dir(&workspace)
        .args(["hook", "install", "cursor", "--project", "--config"])
        .arg(home.config_path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("already installed in"), "{stderr}");
    assert!(
        !workspace.join(".cursor/hooks.json").exists(),
        "nothing written"
    );

    // With --force it goes in, and status says what that costs.
    let out = home
        .cmd()
        .current_dir(&workspace)
        .args([
            "hook",
            "install",
            "cursor",
            "--project",
            "--force",
            "--config",
        ])
        .arg(home.config_path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let status = home
        .cmd()
        .current_dir(&workspace)
        .args(["hook", "status", "cursor", "--project"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&status.stdout).contains("ALSO HERE"));
}

/// A rival shell rewriter in the workspace collides with a user install just
/// as one in the user's own file does.
#[test]
fn a_rival_rewriter_in_the_workspace_file_is_caught_too() {
    let home = Home::new();
    let workspace = home.path().join("proj");
    std::fs::create_dir_all(workspace.join(".cursor")).unwrap();
    std::fs::write(
        workspace.join(".cursor/hooks.json"),
        r#"{"version":1,"hooks":{"preToolUse":[{"command":"rtk rewrite","matcher":"^Shell$"}]}}"#,
    )
    .unwrap();
    let out = home
        .cmd()
        .current_dir(&workspace)
        .args(["hook", "install", "cursor", "--config"])
        .arg(home.config_path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("rtk rewrite (in"));
}
