//! The live body capture, driven through the real request path.
//!
//! `clippy::await_holding_lock` is allowed for the file. Every test here holds
//! `ONE_AT_A_TIME` for its whole body, which is the point -- the critical
//! region is the test, not a section of it. `#[tokio::test]` runs on a
//! current-thread runtime, so the guard never crosses a thread and the hazard
//! the lint guards against, a future parked on one worker while another waits
//! on a lock it cannot see, cannot arise. A `tokio::sync::Mutex` would also
//! work and would mean adding the `sync` feature for a test-only lock.
#![allow(clippy::await_holding_lock)]
//!
//! Its own test binary, deliberately. `KOMPRESSOR_BODY_CAPTURE` is read from
//! the process environment, so a test that sets it arms the capture for every
//! other test running beside it -- which is how the interleaving bug in
//! `bodycapture` was found, and which would make this test's own assertions
//! depend on what its neighbours happened to be doing. Cargo runs each test
//! binary on its own, so one test here has the environment to itself.

mod support;

/// These tests set `KOMPRESSOR_BODY_CAPTURE`, which is process-wide, so they
/// cannot run beside each other: one calling `remove_var` while another is
/// mid-request leaves that one capturing nowhere. Two of them coexisted by
/// luck; adding a third broke it immediately, exactly as the comment at the top
/// of this file warned. One binary is not enough isolation -- they need to be
/// serial within it too.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

use support::{Harness, log_payload};
use wiremock::matchers::any;
use wiremock::{Mock, ResponseTemplate};

fn answered() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "id": "resp_1",
        "object": "response",
        "output": [{"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": "ok"}]}],
        "usage": {"input_tokens": 100, "output_tokens": 5}
    }))
}

/// The live capture, exercised through the real request path.
///
/// `bodycapture` is the tool for the question the offline cases here cannot
/// reach: which byte changed on a *live* run, and whether it changed on the way
/// in or on the way out. A diagnostic that silently records nothing is worse
/// than none at all, so this drives a request through the proxy with the
/// capture armed and reads the file back.
#[tokio::test]
async fn the_body_capture_records_both_directions_of_a_real_request() {
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("bodies.jsonl");
    // Set for this process; the proxy reads it per request.
    // Safe here: `set_var` is unsafe in edition 2024 because it races any
    // other thread reading the environment, and this is a single-threaded
    // test that sets the variable before the code under test runs.
    unsafe { std::env::set_var("KOMPRESSOR_BODY_CAPTURE", &file) };

    let harness = Harness::new().await;
    Mock::given(any())
        .respond_with(answered())
        .mount(&harness.server)
        .await;
    harness
        .post("/v1/responses", &support::responses_body(&log_payload(400)))
        .await;
    // Safe here: `set_var` is unsafe in edition 2024 because it races any
    // other thread reading the environment, and this is a single-threaded
    // test that sets the variable before the code under test runs.
    unsafe { std::env::remove_var("KOMPRESSOR_BODY_CAPTURE") };

    let written = std::fs::read_to_string(&file).expect("the capture file must exist");
    let lines: Vec<serde_json::Value> = written
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is JSON"))
        .collect();
    assert_eq!(lines.len(), 2, "one record each way: {written:.200}");

    let incoming = &lines[0];
    let outgoing = &lines[1];
    assert_eq!(incoming["dir"], "client->proxy");
    assert_eq!(outgoing["dir"], "proxy->upstream");
    assert_eq!(
        incoming["request_id"], outgoing["request_id"],
        "both sides carry the id that joins them to proxy_requests"
    );

    // The pair has to be usable for the thing it exists for: the outgoing body
    // is the compressed one, and both are whole rather than truncated.
    let (before, after) = (
        incoming["body"].as_str().unwrap(),
        outgoing["body"].as_str().unwrap(),
    );
    assert!(
        after.len() < before.len(),
        "this fixture compresses, so the captured pair should differ in size: \
         {} -> {}",
        before.len(),
        after.len()
    );
    assert_eq!(before.len() as u64, incoming["bytes"].as_u64().unwrap());
    assert_eq!(after.len() as u64, outgoing["bytes"].as_u64().unwrap());
    assert!(
        before.starts_with('{') && before.ends_with('}'),
        "a truncated body cannot be diffed and is the one thing this must not do"
    );
}

/// A proxy with the relay switched on, in front of `upstream`.
///
/// Inlined rather than shared with `tests/websocket.rs`, which has its own
/// copy: moving that one into `support` would touch a file this change has no
/// other business in.
async fn relaying_proxy(upstream: &str) -> String {
    use kompressor_core::AppCore;
    use kompressor_core::config::{Config, Upstream};
    use kompressor_proxy::ProxyState;
    use kompressor_store::memory::MemoryRefStore;
    use std::sync::Arc;
    use tokio::net::TcpListener;

    let mut config = Config::default();
    config.stats.enabled = false;
    config.proxy.websocket_relay = true;
    config.proxy.upstreams = vec![Upstream {
        name: "provider".to_string(),
        path_prefix: String::new(),
        base_url: format!("http://{upstream}"),
        api_family: "openai".to_string(),
        forward_auth: true,
        ..Default::default()
    }];
    let (sink, _events) = kompressor_stats::writer::capturing_sink(256);
    let core = Arc::new(AppCore::new(config, Arc::new(MemoryRefStore::new()), sink));
    let router = ProxyState::new(core, reqwest::Client::new()).into_router();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    addr
}

/// No relay, no frame capture — the file's existence is the signal.
///
/// The benchmark names `KOMPRESSOR_WS_CAPTURE` on every run now, pointing it
/// into the dump, because a capture kept in `/tmp` is one nobody has when the
/// question comes up. That only works if the file appears when the relay is on
/// and not otherwise: an empty `kompressor-ws-frames.jsonl` in every dump would
/// make a reader wonder what went uncaptured on a run that never relayed
/// anything, which is the opposite of what a capture is for.
#[tokio::test]
async fn a_run_without_the_relay_writes_no_frame_capture() {
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("frames.jsonl");
    // Safe here: `set_var` is unsafe in edition 2024 because it races any
    // other thread reading the environment, and this is a single-threaded
    // test that sets the variable before the code under test runs.
    unsafe { std::env::set_var("KOMPRESSOR_WS_CAPTURE", &file) };

    // The ordinary HTTP path, with the relay left at its shipped default.
    let harness = Harness::new().await;
    Mock::given(any())
        .respond_with(answered())
        .mount(&harness.server)
        .await;
    harness
        .post("/v1/responses", &support::responses_body(&log_payload(400)))
        .await;

    // Safe here: `set_var` is unsafe in edition 2024 because it races any
    // other thread reading the environment, and this is a single-threaded
    // test that sets the variable before the code under test runs.
    unsafe { std::env::remove_var("KOMPRESSOR_WS_CAPTURE") };
    assert!(
        !file.exists(),
        "a run that relayed no websocket must leave no frame capture behind"
    );
}

/// The relay's frame capture, driven through a real websocket.
///
/// It recorded only what the client sent, under a direction label
/// (`client->upstream`) that named the frame's destination rather than its
/// content — so a capture taken to answer "which byte did we change" held no
/// record of what was actually sent. Both sides are recorded now, and this
/// drives a frame through the relay to prove it.
#[tokio::test]
async fn the_relay_records_the_frame_it_sends_as_well_as_the_one_it_got() {
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message as Ws;

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("frames.jsonl");
    // Safe here: `set_var` is unsafe in edition 2024 because it races any
    // other thread reading the environment, and this is a single-threaded
    // test that sets the variable before the code under test runs.
    unsafe { std::env::set_var("KOMPRESSOR_WS_CAPTURE", &file) };

    // An upstream that accepts the upgrade and echoes, so the relay has a
    // socket to talk to.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                while let Some(Ok(message)) = socket.next().await {
                    if socket.send(message).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    let proxy = relaying_proxy(&upstream).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{proxy}/v1/responses"))
        .await
        .expect("the proxy should accept the upgrade");

    // A frame big enough that the engine rewrites it, so the two sides differ.
    let frame = serde_json::json!({
        "type": "response.create",
        "model": "gpt-5-codex",
        "input": [{"type": "function_call_output", "call_id": "call_a1",
                   "output": log_payload(400)}],
        "stream": true,
    })
    .to_string();
    socket.send(Ws::Text(frame.clone().into())).await.unwrap();
    let _echo = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
        .await
        .expect("the relay should answer")
        .expect("the stream should be open")
        .unwrap();
    // Safe here: `set_var` is unsafe in edition 2024 because it races any
    // other thread reading the environment, and this is a single-threaded
    // test that sets the variable before the code under test runs.
    unsafe { std::env::remove_var("KOMPRESSOR_WS_CAPTURE") };

    let written = std::fs::read_to_string(&file).expect("a capture file");
    let records: Vec<serde_json::Value> = written
        .lines()
        .map(|line| serde_json::from_str(line).expect("every line is JSON"))
        .collect();

    let sent = |dir: &str| {
        records
            .iter()
            .find(|r| {
                r["dir"] == dir
                    && r["text"]
                        .as_str()
                        .is_some_and(|t| t.contains("response.create"))
            })
            .map(|r| r["text"].as_str().unwrap().to_string())
    };
    let got = sent("client->proxy").expect("the frame as it arrived");
    let out = sent("proxy->upstream").expect("the frame as it was sent");

    assert_eq!(got, frame, "the arriving side is the client's own bytes");
    assert!(
        out.len() < got.len(),
        "this frame compresses, so the two sides must differ: {} -> {}",
        got.len(),
        out.len()
    );
    assert!(
        out.contains("ref:k_"),
        "and the sent side is the rewritten one, which is the side a prefix \
         diff needs"
    );
}

/// The relay's per-frame decision, recorded alongside the request bodies.
///
/// The question this exists for: a retrieve call that reaches the client did so
/// because the turn had already gone passthrough, and only the running content
/// total against the grace says why. Request bodies cannot show it — the
/// decision happens in the response stream.
#[tokio::test]
async fn the_relay_records_why_it_gave_up_on_a_streamed_turn() {
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    use wiremock::matchers::any;

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("bodies.jsonl");
    // Safe here: `set_var` is unsafe in edition 2024 because it races any
    // other thread reading the environment, and this is a single-threaded
    // test that sets the variable before the code under test runs.
    unsafe { std::env::set_var("KOMPRESSOR_BODY_CAPTURE", &file) };

    // Narration past the grace, then a retrieve call the relay can no longer
    // take: the shape that leaked a call to codex on a live run.
    let narration: String = (0..40)
        .map(|i| {
            format!(
                "data: {{\"id\":\"c1\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{}\"}}}}]}}\n\n",
                "x".repeat(300) + &i.to_string()
            )
        })
        .collect();
    let stream = format!(
        "{narration}data: {{\"id\":\"c1\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
    );
    let harness = Harness::new().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .mount(&harness.server)
        .await;

    let mut body = support::chat_body(&log_payload(400));
    body["stream"] = serde_json::json!(true);
    harness.post("/v1/chat/completions", &body).await;
    // Safe here: `set_var` is unsafe in edition 2024 because it races any
    // other thread reading the environment, and this is a single-threaded
    // test that sets the variable before the code under test runs.
    unsafe { std::env::remove_var("KOMPRESSOR_BODY_CAPTURE") };

    let written = std::fs::read_to_string(&file).expect("a capture file");
    let frames: Vec<serde_json::Value> = written
        .lines()
        .map(|line| serde_json::from_str(line).expect("every line is JSON"))
        .filter(|r: &serde_json::Value| r["dir"] == "upstream->client")
        .collect();

    assert!(
        !frames.is_empty(),
        "the response frames must be recorded, not just the request bodies"
    );
    let content = |r: &serde_json::Value| r["content_bytes"].as_u64().unwrap_or(0);
    assert!(
        frames.iter().any(|r| content(r) > 0),
        "the running content total has to move, or it says nothing about why \
         the relay gave up"
    );
    // The running total is recorded *before* the frame it describes is added,
    // so the trace stops one frame short of the grace rather than crossing it.
    // That is the right place to record it -- it is the total the decision was
    // taken against -- and it means the assertion is "it got there", not "it
    // went past".
    let grace = frames[0]["content_grace"].as_u64().unwrap();
    let peak = frames.iter().map(content).max().unwrap_or(0);
    assert!(
        peak > grace / 2,
        "the trace must show the total approaching the grace ({grace}), or it \
         cannot explain the give-up: peak {peak}"
    );
    let gave_up: Vec<&serde_json::Value> = frames
        .iter()
        .filter(|r| r["state"] == "Passthrough")
        .collect();
    assert!(
        !gave_up.is_empty(),
        "and the state it reached is the answer to 'why was the call relayed'"
    );
    // The reason, not just the numbers. A record carrying the content totals
    // beside every give-up invites reading content as the cause even when the
    // turn ended on a tool call -- which is what happened on the first live
    // capture, where 28 of 31 turns ended on `OtherCall` with content at 0.
    assert_eq!(
        gave_up[0]["reason"], "narration passed the content grace",
        "this fixture narrates past the grace, so that must be the reason given"
    );
}

/// What a replay of the file needs beside the bodies: the beta set each
/// request travelled with, as a fingerprint, and every keep-warm ping the
/// proxy sent — which request it followed and what the provider read.
#[tokio::test]
async fn the_capture_records_beta_sets_and_keep_warm_pings() {
    use serde_json::json;
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("bodies.jsonl");
    // Safe for the reason given above: set before the code under test runs.
    unsafe { std::env::set_var("KOMPRESSOR_BODY_CAPTURE", &file) };

    let interval = std::time::Duration::from_millis(100);
    let harness = Harness::with(|config| {
        config.proxy.cache_keep_warm = true;
        config.proxy.cache_keep_warm_interval = interval;
        config.proxy.cache_keep_warm_max_pings = 1;
    })
    .await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant",
            "content": [{"type": "tool_use", "id": "toolu_x", "name": "bash",
                         "input": {"command": "make"}}],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 3, "cache_read_input_tokens": 4000,
                      "cache_creation_input_tokens": 200, "output_tokens": 40}
        })))
        .mount(&harness.server)
        .await;
    let body = json!({
        "model": "claude-opus-5-5", "max_tokens": 32000,
        "system": [{"type": "text", "text": "You are an agent.",
                    "cache_control": {"type": "ephemeral"}}],
        "messages": [{"role": "user", "content": "Build the project."}]
    });
    for betas in [
        "b-2025-01-01, a-2025-01-01",
        "a-2025-01-01,b-2025-01-01",
        "c-2025-01-01",
    ] {
        harness
            .post_with("/v1/messages", &body, &[("anthropic-beta", betas)])
            .await;
    }
    tokio::time::sleep(interval * 6).await;
    unsafe { std::env::remove_var("KOMPRESSOR_BODY_CAPTURE") };

    let written = std::fs::read_to_string(&file).expect("the capture file must exist");
    let lines: Vec<serde_json::Value> = written
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is JSON"))
        .collect();
    let of = |dir: &str| -> Vec<&serde_json::Value> {
        lines.iter().filter(|line| line["dir"] == dir).collect()
    };
    let incoming = of("client->proxy");
    assert_eq!(incoming.len(), 3);
    let beta = |line: &serde_json::Value| line["beta"].as_str().expect("a fingerprint").to_string();
    assert_eq!(beta(incoming[0]).len(), 16);
    assert_eq!(
        beta(incoming[0]),
        beta(incoming[1]),
        "the same set, spelt differently"
    );
    assert_ne!(beta(incoming[0]), beta(incoming[2]));
    assert!(
        !written.contains("a-2025-01-01"),
        "no header value is recorded"
    );
    assert!(
        of("proxy->upstream")
            .iter()
            .all(|line| line.get("beta").is_none())
    );

    let pings = of("keep-warm");
    assert_eq!(pings.len(), 1, "{written:.400}");
    let ping = pings[0];
    assert_eq!(
        ping["request_id"],
        json!(format!(
            "{}.keepwarm1",
            incoming[2]["request_id"].as_str().unwrap()
        )),
        "the latest request is the one kept warm"
    );
    assert_eq!(
        (&ping["status"], &ping["read"]),
        (&json!(200), &json!(4000))
    );
    assert!(ping.get("body").is_none());
    assert!(
        ping["at_ms"].as_u64() >= incoming[2]["at_ms"].as_u64(),
        "in the file's own time order"
    );
}
