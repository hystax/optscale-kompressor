//! `proxy.state_path`: what the proxy remembers about each conversation,
//! carried across a restart (DESIGN §12.10).
//!
//! The guarantee is stated on the wire: after a restart that saved and loaded
//! the state, the next turn of an open conversation goes upstream as the
//! bytes a process that never stopped would have sent. Each of those tests
//! runs a third arm, restarted without the file, and requires it to send
//! something else — otherwise the scenario exercised nothing a restart loses
//! and the equality would prove nothing.

mod support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::{Harness, log_payload};
use wiremock::matchers::any;
use wiremock::{Mock, ResponseTemplate};

use kompressor_core::config::{ColdRecompact, Config, FrozenPrefix, ToolsOrder};
use kompressor_proxy::persist::{Loaded, VERSION};
use kompressor_proxy::prefixcache::{
    REMEMBER, RETRIEVED_CONVERSATIONS, RETRIEVED_PER_CONVERSATION,
};

const MESSAGES: &str = "/v1/messages";
/// Placed in every message of the conversation; must never reach the file.
const MARKER: &str = "zebra-quartz-7f3a9c-never-on-disk";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arm {
    /// One process throughout.
    NeverRestarted,
    /// Restarted before the turn under test, with the state saved and loaded.
    Restored,
    /// Restarted the same way with no state file: what a restart cost before.
    Forgotten,
}

fn answered(read: u32, written: u32) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "msg_1", "type": "message", "role": "assistant",
        "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn",
        "usage": {"input_tokens": 12, "output_tokens": 2,
                  "cache_read_input_tokens": read,
                  "cache_creation_input_tokens": written}
    }))
}

fn state_file(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("proxy-state.json")
}

fn with_state(path: &Path, also: impl FnOnce(&mut Config)) -> impl FnOnce(&mut Config) {
    let path = path.display().to_string();
    move |config| {
        config.proxy.state_path = path;
        also(config);
    }
}

/// Two tools, with schema bookkeeping `tools_compact` takes out, in the order
/// given.
fn tools(reversed: bool) -> Value {
    let read = json!({
        "name": "Read", "description": "Read a file.",
        "input_schema": {"$comment": "generated", "type": "object",
                         "properties": {"path": {"title": "Path", "type": "string"}}}
    });
    let bash = json!({
        "name": "Bash", "description": "Run a command.",
        "input_schema": {"$comment": "generated", "type": "object",
                         "properties": {"command": {"title": "Command", "type": "string"}}}
    });
    if reversed {
        json!([bash, read])
    } else {
        json!([read, bash])
    }
}

/// Turn `turn` (1-based) of a conversation that asks, reads a log worth
/// compressing on turn two, and then talks about it, with the cache
/// breakpoint on the final message from turn two on, where Claude Code puts
/// it. Turn four sends its tools in another order.
fn turn(turn: usize) -> Value {
    let mut messages =
        vec![json!({"role": "user", "content": format!("Look at the log. {MARKER}")})];
    if turn >= 2 {
        messages.push(json!({"role": "assistant", "content": [
            {"type": "text", "text": format!("Reading it. {MARKER}")},
            {"type": "tool_use", "id": "t0", "name": "Read", "input": {"path": "/var/log/app.log"}}
        ]}));
        messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t0",
             "content": format!("{MARKER}\n{}", log_payload(400))}
        ]}));
    }
    for step in 3..=turn {
        messages.push(json!({"role": "assistant", "content": [
            {"type": "text", "text": format!("Step {step}. {MARKER}")}
        ]}));
        messages.push(json!({"role": "user", "content": [
            {"type": "text", "text": format!("Go on with step {step}. {MARKER}")}
        ]}));
    }
    if turn >= 2 {
        let last = messages.len() - 1;
        messages[last]["content"][0]["cache_control"] = json!({"type": "ephemeral"});
    }
    json!({
        "model": "claude-opus-5-5", "max_tokens": 1024,
        "system": format!("You are careful. {MARKER}"),
        "tools": tools(turn == 4),
        "messages": messages
    })
}

/// The beta tokens turn `turn` carries: two at first, one on turn four.
fn betas(turn: usize) -> &'static str {
    if turn == 4 {
        "beta-one-2026-01-01"
    } else {
        "beta-one-2026-01-01,beta-two-2026-01-01"
    }
}

fn everything_on(config: &mut Config) {
    config.proxy.frozen_prefix = FrozenPrefix::FirstSeen;
    config.proxy.tools_order = ToolsOrder::Pinned;
    config.proxy.sticky_beta = true;
    config.proxy.tools_compact = true;
}

/// What turn four went upstream as: its body's bytes and its beta header.
async fn fourth_turn(arm: Arm) -> (Vec<u8>, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = state_file(&dir);
    let configure = |config: &mut Config| {
        everything_on(config);
        if arm == Arm::Restored {
            config.proxy.state_path = path.display().to_string();
        }
    };
    let mut harness = Harness::with(configure).await;
    Mock::given(any())
        .respond_with(answered(5_000, 100))
        .mount(&harness.server)
        .await;
    for n in 1..=3 {
        harness
            .post_with(MESSAGES, &turn(n), &[("anthropic-beta", betas(n))])
            .await;
    }
    if arm != Arm::NeverRestarted {
        harness = harness.restart();
        assert_eq!(
            harness.state.loaded(),
            &if arm == Arm::Restored {
                Loaded::Restored { conversations: 1 }
            } else {
                Loaded::Off
            }
        );
    }
    harness
        .post_with(MESSAGES, &turn(4), &[("anthropic-beta", betas(4))])
        .await;
    let requests = harness.server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
    let fourth = &requests[3];
    let beta = fourth
        .headers
        .get("anthropic-beta")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    (fourth.body.clone(), beta)
}

#[tokio::test]
async fn a_restart_with_saved_state_sends_what_a_process_that_never_stopped_sends() {
    let (never, never_beta) = fourth_turn(Arm::NeverRestarted).await;
    let (restored, restored_beta) = fourth_turn(Arm::Restored).await;
    let (forgotten, forgotten_beta) = fourth_turn(Arm::Forgotten).await;

    // What the scenario has to exercise, or the equality below proves
    // nothing: the read compressed under the watermark, the tools put back
    // in their first order and compacted, the dropped beta token put back.
    let sent: Value = serde_json::from_slice(&never).unwrap();
    let read = sent["messages"][2]["content"][0]["content"]
        .as_str()
        .unwrap_or_default();
    assert!(
        read.len() < log_payload(400).len(),
        "the read is compressed"
    );
    assert_eq!(sent["tools"][0]["name"], "Read", "the first order is kept");
    assert!(
        !sent.to_string().contains("$comment"),
        "the tools are compacted"
    );
    assert_eq!(never_beta, betas(1), "the dropped token is put back");

    assert!(
        restored == never,
        "turn four after a restart differs from the uninterrupted run:\n{}\n{}",
        String::from_utf8_lossy(&restored),
        String::from_utf8_lossy(&never)
    );
    assert_eq!(restored_beta, never_beta);

    // The control: without the file the restart costs exactly what it used to.
    assert_ne!(
        forgotten, never,
        "a restart without state changes the bytes"
    );
    let forgotten: Value = serde_json::from_slice(&forgotten).unwrap();
    assert_eq!(
        forgotten["messages"][2]["content"][0]["content"]
            .as_str()
            .map(str::len),
        Some(format!("{MARKER}\n{}", log_payload(400)).len()),
        "frozen to the marker, the read goes out whole"
    );
    assert_eq!(
        forgotten["tools"][0]["name"], "Bash",
        "and nothing is pinned"
    );
    assert_ne!(forgotten_beta, never_beta);
}

/// The conversation `tests/recompact.rs` drives: a read of a file, an edit of
/// it, and `steps` further exchanges, marked on the final message.
fn edited(steps: usize) -> Value {
    let file: String = (0..300)
        .map(|line| format!("    let value_{line} = compute(input_{line}, {line});\n"))
        .collect();
    let mut messages = vec![
        json!({"role": "user", "content": "Fix the bug in src/a.rs."}),
        json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "t0", "name": "Read", "input": {"file_path": "src/a.rs"}}
        ]}),
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t0", "content": file}
        ]}),
        json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "t1", "name": "Edit",
             "input": {"file_path": "src/a.rs", "old_string": "a", "new_string": "b"}}
        ]}),
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "The file was updated."}
        ]}),
    ];
    for step in 0..steps {
        messages.push(json!({"role": "assistant", "content": [
            {"type": "text", "text": format!("Step {step} is done.")}
        ]}));
        messages.push(json!({"role": "user", "content": [
            {"type": "text", "text": format!("Go on with step {}.", step + 1)}
        ]}));
    }
    let last = messages.len() - 1;
    messages[last]["content"][0]["cache_control"] = json!({"type": "ephemeral"});
    json!({"model": "claude-opus-5-5", "max_tokens": 1024, "messages": messages})
}

/// The two turns after a cold turn rewrote the conversation, as sent.
async fn after_the_cold_turn(arm: Arm) -> Vec<Vec<u8>> {
    let dir = tempfile::tempdir().unwrap();
    let path = state_file(&dir);
    let configure = |config: &mut Config| {
        config.proxy.frozen_prefix = FrozenPrefix::FirstSeen;
        config.proxy.cold_recompact = ColdRecompact::Observed;
        if arm == Arm::Restored {
            config.proxy.state_path = path.display().to_string();
        }
    };
    let mut harness = Harness::with(configure).await;
    // Written, read back, then a turn that reads nothing and writes nothing,
    // then the cold turn's own write, then warm.
    for (read, written) in [(0, 30_000), (30_000, 200), (0, 0), (0, 31_000)] {
        Mock::given(any())
            .respond_with(answered(read, written))
            .up_to_n_times(1)
            .mount(&harness.server)
            .await;
    }
    Mock::given(any())
        .respond_with(answered(31_000, 200))
        .mount(&harness.server)
        .await;
    for steps in 0..4 {
        harness.post(MESSAGES, &edited(steps)).await;
    }
    if arm != Arm::NeverRestarted {
        harness = harness.restart();
    }
    for steps in 4..6 {
        harness.post(MESSAGES, &edited(steps)).await;
    }
    let requests = harness.server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 6);
    let cold: Value = serde_json::from_slice(&requests[3].body).unwrap();
    assert!(
        cold["messages"][2]["content"][0]["content"]
            .as_str()
            .unwrap_or_default()
            .starts_with("[Read content stale"),
        "the cold turn replaced the read, or nothing below is tested"
    );
    requests[4..]
        .iter()
        .map(|request| request.body.clone())
        .collect()
}

#[tokio::test]
async fn a_recompacted_conversation_keeps_its_ledger_and_floor_across_a_restart() {
    let never = after_the_cold_turn(Arm::NeverRestarted).await;
    let restored = after_the_cold_turn(Arm::Restored).await;
    let forgotten = after_the_cold_turn(Arm::Forgotten).await;
    assert!(restored == never, "the ledger replays across the restart");
    assert_ne!(forgotten, never, "without it the replaced read comes back");
}

/// A proxy that has served three turns of the conversation and saved them.
async fn saved(path: &Path) -> Harness {
    let harness = Harness::with(with_state(path, everything_on)).await;
    Mock::given(any())
        .respond_with(answered(5_000, 100))
        .mount(&harness.server)
        .await;
    for n in 1..=3 {
        harness
            .post_with(MESSAGES, &turn(n), &[("anthropic-beta", betas(n))])
            .await;
    }
    harness.state.save_state().unwrap();
    harness
}

/// Nothing remembered, and a request still goes through.
async fn starts_empty(harness: &Harness) {
    assert_eq!(harness.state.prefixes().tracked(), 0);
    assert_eq!(harness.state.recompact().tracked(), 0);
    assert_eq!(harness.state.stabilizer().tracked(), 0);
    assert_eq!(harness.state.tool_policies().tracked(), 0);
    let reply = harness.post(MESSAGES, &turn(4)).await;
    assert_eq!(reply.status, 200);
}

#[tokio::test]
async fn a_missing_file_starts_empty() {
    let dir = tempfile::tempdir().unwrap();
    let harness = Harness::with(with_state(&state_file(&dir), |_| {})).await;
    Mock::given(any())
        .respond_with(answered(0, 0))
        .mount(&harness.server)
        .await;
    assert_eq!(harness.state.loaded(), &Loaded::Missing);
    starts_empty(&harness).await;
}

#[tokio::test]
async fn a_file_that_cannot_be_used_is_ignored_and_startup_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let path = state_file(&dir);
    let good = {
        saved(&path).await;
        std::fs::read(&path).unwrap()
    };
    let mut parsed: Value = serde_json::from_slice(&good).unwrap();
    let mut with_version = |version: u32| {
        parsed["version"] = json!(version);
        serde_json::to_vec(&parsed).unwrap()
    };
    let older = with_version(VERSION - 1);
    let newer = with_version(VERSION + 1);
    let mut other_format: Value = serde_json::from_slice(&good).unwrap();
    other_format["format"] = json!("something-else");
    let mut wrong_shape: Value = serde_json::from_slice(&good).unwrap();
    wrong_shape["tracker"]["conversations"] = json!("not a list");

    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("truncated", good[..good.len() / 2].to_vec()),
        ("empty", Vec::new()),
        ("not json", b"\x00\xffgarbage{{{".to_vec()),
        ("older version", older),
        ("newer version", newer),
        ("another format", serde_json::to_vec(&other_format).unwrap()),
        ("damaged body", serde_json::to_vec(&wrong_shape).unwrap()),
    ];
    for (case, bytes) in cases {
        std::fs::write(&path, &bytes).unwrap();
        let harness = Harness::with(with_state(&path, everything_on)).await;
        Mock::given(any())
            .respond_with(answered(0, 0))
            .mount(&harness.server)
            .await;
        assert!(
            matches!(harness.state.loaded(), Loaded::Rejected { .. }),
            "{case}: {:?}",
            harness.state.loaded()
        );
        starts_empty(&harness).await;
    }
}

#[tokio::test]
async fn with_loading_off_the_file_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let path = state_file(&dir);
    let harness = saved(&path).await;
    assert!(harness.state.prefixes().tracked() > 0);
    let harness = harness.restart_with(|config| config.proxy.state_load = false);
    assert_eq!(harness.state.loaded(), &Loaded::Disabled);
    starts_empty(&harness).await;
    // And loading back on reads what the last process left.
    let harness = harness.restart_with(|config| config.proxy.state_load = true);
    assert!(matches!(harness.state.loaded(), Loaded::Restored { .. }));
}

#[tokio::test]
async fn the_file_holds_no_conversation_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = state_file(&dir);
    let harness = saved(&path).await;
    // The marker went upstream, so it was there to be written.
    assert!(
        harness
            .upstream_bodies()
            .await
            .iter()
            .any(|body| body.contains(MARKER))
    );
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(
        written.contains("\"conversations\":[{"),
        "the conversation was saved: {written}"
    );
    assert!(
        !written.contains(MARKER),
        "message text reached the state file"
    );
    assert!(
        !written.contains("worker heartbeat"),
        "tool output reached it"
    );
    assert!(
        !written.contains("You are careful"),
        "the system prompt reached it"
    );
}

#[tokio::test]
async fn the_file_is_bounded_by_the_tables_caps() {
    let dir = tempfile::tempdir().unwrap();
    let path = state_file(&dir);
    let harness = Harness::with(with_state(&path, |_| {})).await;
    let tracker = harness.state.prefixes();
    let over = REMEMBER + 500;
    for n in 0..over as u64 {
        let id = tracker.identify(&[n, n ^ 0xabcd]).unwrap();
        tracker.floor(id, 1);
        tracker.record(id, Some(10), Some(100), 0);
    }
    let handle =
        |n: usize| kompressor_engine::refs::RefId::parse(&format!("k_{n:0>12}")).expect("a handle");
    for conversation in 0..(RETRIEVED_CONVERSATIONS + 100) as u64 {
        let ids: Vec<_> = (0..RETRIEVED_PER_CONVERSATION + 10).map(handle).collect();
        tracker.record_retrieved(conversation, "request-a", &ids);
    }
    harness.state.save_state().unwrap();

    let written: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let count = |pointer: &str| {
        written
            .pointer(pointer)
            .and_then(Value::as_array)
            .map_or(0, Vec::len)
    };
    assert_eq!(count("/tracker/conversations"), REMEMBER);
    assert!(count("/tracker/openings") <= REMEMBER);
    assert!(count("/tracker/histories") <= REMEMBER);
    assert_eq!(count("/tracker/retrieved"), RETRIEVED_CONVERSATIONS);
    for held in written["tracker"]["retrieved"].as_array().unwrap() {
        assert!(held["handles"].as_array().unwrap().len() <= RETRIEVED_PER_CONVERSATION);
    }

    // A file holding more than the caps — from a build with larger ones, or
    // edited by hand — is cut to them on load, newest first.
    let mut padded = written.clone();
    let conversations = padded["tracker"]["conversations"].as_array_mut().unwrap();
    let template = conversations[0].clone();
    for n in 0..1_000u64 {
        let mut extra = template.clone();
        extra["conversation"] = json!(u64::MAX - n);
        extra["age_ms"] = json!(0);
        conversations.push(extra);
    }
    drop(harness);
    std::fs::write(&path, serde_json::to_vec(&padded).unwrap()).unwrap();
    let harness = Harness::with(with_state(&path, |_| {})).await;
    assert_eq!(
        harness.state.loaded(),
        &Loaded::Restored {
            conversations: REMEMBER
        }
    );
    assert_eq!(harness.state.prefixes().tracked(), REMEMBER);
    assert!(
        harness.state.prefixes().held_floor(u64::MAX).is_some(),
        "the newest are the ones kept"
    );
}
