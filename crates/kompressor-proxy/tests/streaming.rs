//! The retrieve loop on a *streamed* response.
//!
//! A server-side retrieve loop that runs only on buffered responses is worse
//! than no loop at all, so these are the tests that hold it to streams.
//!
//! In the service this replaces, the server-side loop silently never ran on
//! streams: nine `optscale_kompressor_retrieve` calls in one afternoon reached clients
//! that had never heard of the tool. Every test here asserts the same thing
//! from a different family's wire: the client's stream contains the answer and
//! no trace of the tool call that produced it.

mod support;

use std::time::Duration;

use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, Request, ResponseTemplate};

use support::{
    Harness, anthropic_body, chat_body, first_handle, log_payload, responses_body, round_of,
};

fn body_text(request: &Request) -> String {
    String::from_utf8_lossy(&request.body).into_owned()
}

fn handle_in(request: &Request) -> String {
    first_handle(&body_text(request)).expect("the outgoing prompt carries a handle")
}

fn sse(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.to_string(), "text/event-stream")
}

/// A streamed OpenAI Chat turn that calls the retrieve tool.
const CHAT_ASKS: &str = concat!(
    "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
    "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_r1\",\"type\":\"function\",\"function\":{\"name\":\"optscale_kompressor_retrieve\",\"arguments\":\"\"}}]}}]}\n\n",
    "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"ref\\\":\\\"HANDLE\\\"}\"}}]}}]}\n\n",
    "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    "data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":1000,\"completion_tokens\":20}}\n\n",
    "data: [DONE]\n\n",
);

/// The same turn, answering instead — and with CRLF frame terminators, which
/// some proxies in front of a provider produce.
const CHAT_ANSWERS: &str = concat!(
    "data: {\"id\":\"c2\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"The disk \"}}]}\r\n\r\n",
    "data: {\"id\":\"c2\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"filled.\"}}]}\r\n\r\n",
    "data: {\"id\":\"c2\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\r\n\r\n",
    "data: {\"id\":\"c2\",\"choices\":[],\"usage\":{\"prompt_tokens\":1200,\"completion_tokens\":30}}\r\n\r\n",
    "data: [DONE]\r\n\r\n",
);

const ANTHROPIC_ASKS: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":1000,\"output_tokens\":1}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_r1\",\"name\":\"optscale_kompressor_retrieve\",\"input\":{}}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"ref\\\":\\\"HANDLE\\\"}\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":20}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

const ANTHROPIC_ANSWERS: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_2\",\"usage\":{\"input_tokens\":1200,\"output_tokens\":1}}}\n\n",
    ": ping\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"The disk filled.\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":30}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

#[tokio::test]
async fn anthropic_handover_mid_call_drops_later_deltas_and_stop() {
    // Force hand-over after the first argument delta. The opening block was
    // buffered; the remaining delta and stop arrive in passthrough.
    let harness = Harness::with(|config| config.proxy.stream_intercept_bytes = 1).await;
    let asks = ANTHROPIC_ASKS.replace(
        "event: content_block_stop",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\" \"}}\n\nevent: content_block_stop",
    );
    Mock::given(any())
        .respond_with(move |request: &Request| sse(&asks.replace("HANDLE", &handle_in(request))))
        .mount(&harness.server)
        .await;
    let reply = harness
        .post(
            "/v1/messages",
            &streaming(anthropic_body(&log_payload(400))),
        )
        .await;
    let text = reply.text();
    assert!(
        !text.contains("content_block_"),
        "orphaned block event: {text}"
    );
    assert!(!text.contains("tool_use"), "{text}");
    assert!(
        text.contains("end_turn") && text.contains("message_stop"),
        "{text}"
    );
}

/// Responses splits its `data:` across two lines here, which the spec says to
/// join with a newline and which a stricter parser would drop.
const RESPONSES_ASKS: &str = concat!(
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_r1\",\"name\":\"optscale_kompressor_retrieve\",\"arguments\":\"\"}}\n\n",
    "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_1\",\n",
    "data:  \"arguments\":\"{\\\"ref\\\":\\\"HANDLE\\\"}\"}\n\n",
    "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_r1\",\"name\":\"optscale_kompressor_retrieve\",\"arguments\":\"{\\\"ref\\\":\\\"HANDLE\\\"}\"}}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":1000,\"output_tokens\":20}}}\n\n",
);

const RESPONSES_ANSWERS: &str = concat!(
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_2\"}}\n\n",
    "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"The disk filled.\"}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\",\"usage\":{\"input_tokens\":1200,\"output_tokens\":30}}}\n\n",
);

/// Ask on the first request, answer on the rest.
async fn mount_stream(harness: &Harness, asks: &'static str, answers: &'static str) {
    Mock::given(any())
        .respond_with(move |request: &Request| {
            if round_of(&body_text(request)) == 0 {
                return sse(&asks.replace("HANDLE", &handle_in(request)));
            }
            sse(answers)
        })
        .mount(&harness.server)
        .await;
}

fn streaming(mut body: Value) -> Value {
    body["stream"] = json!(true);
    body
}

#[tokio::test]
async fn the_retrieve_loop_runs_on_a_streamed_openai_chat_response() {
    let harness = Harness::new().await;
    mount_stream(&harness, CHAT_ASKS, CHAT_ANSWERS).await;

    let reply = harness
        .post(
            "/v1/chat/completions",
            &streaming(chat_body(&log_payload(400))),
        )
        .await;
    let stream = reply.text();

    assert_eq!(reply.status, 200);
    assert!(
        !stream.contains("optscale_kompressor_retrieve"),
        "the client never declared the tool and must never see its call: {stream}"
    );
    assert!(stream.contains("The disk "));
    assert!(stream.contains("filled."));
    assert!(stream.trim_end().ends_with("data: [DONE]"));
    assert_eq!(
        harness.upstream_bodies().await.len(),
        2,
        "the retrieve loop has to run on streamed responses too"
    );

    let second = &harness.upstream_json().await[1];
    let result: Value =
        serde_json::from_str(second["messages"][5]["content"].as_str().unwrap()).unwrap();
    assert_eq!(result["found"], true);
    assert!(
        result["content"]
            .as_str()
            .unwrap()
            .contains("heartbeat seq=")
    );
}

#[tokio::test]
async fn the_retrieve_loop_runs_on_a_streamed_anthropic_response_and_starts_one_message_only() {
    let harness = Harness::new().await;
    mount_stream(&harness, ANTHROPIC_ASKS, ANTHROPIC_ANSWERS).await;

    let reply = harness
        .post(
            "/v1/messages",
            &streaming(anthropic_body(&log_payload(400))),
        )
        .await;
    let stream = reply.text();

    assert!(!stream.contains("tool_use"), "{stream}");
    assert!(stream.contains("The disk filled."));
    assert_eq!(
        stream.matches("\"type\":\"message_start\"").count(),
        1,
        "a client cannot be handed two message_start events for one message"
    );
    assert!(
        stream.contains("msg_2") && !stream.contains("msg_1"),
        "the one message_start the client sees is the final round's"
    );
    assert_eq!(harness.upstream_bodies().await.len(), 2);

    let second = &harness.upstream_json().await[1];
    assert_eq!(second["messages"][4]["content"][0]["type"], "tool_result");
}

#[tokio::test]
async fn the_retrieve_loop_runs_on_a_streamed_openai_responses_response() {
    let harness = Harness::new().await;
    mount_stream(&harness, RESPONSES_ASKS, RESPONSES_ANSWERS).await;

    let reply = harness
        .post(
            "/v1/responses",
            &streaming(responses_body(&log_payload(400))),
        )
        .await;
    let stream = reply.text();

    assert!(!stream.contains("function_call"), "{stream}");
    assert!(stream.contains("The disk filled."));
    assert_eq!(harness.upstream_bodies().await.len(), 2);

    let second = &harness.upstream_json().await[1];
    let input = second["input"].as_array().unwrap();
    assert_eq!(input.last().unwrap()["type"], "function_call_output");
    assert_eq!(input.last().unwrap()["call_id"], "call_r1");
}

#[tokio::test]
async fn a_streamed_turn_that_also_calls_another_tool_keeps_the_other_call_and_drops_ours() {
    let harness = Harness::new().await;
    let mixed = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"optscale_kompressor_retrieve\",\"arguments\":\"{}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(any())
        .respond_with(move |_: &Request| sse(mixed))
        .mount(&harness.server)
        .await;

    let reply = harness
        .post(
            "/v1/chat/completions",
            &streaming(chat_body(&log_payload(400))),
        )
        .await;

    let text = reply.text();
    // Half a turn cannot be satisfied, so the client's own call reaches it
    // untouched and the round is not intercepted.
    assert!(
        text.contains("read_file") && text.contains("call_b"),
        "the client's call has to arrive so it can answer it: {text}"
    );
    assert_eq!(harness.upstream_bodies().await.len(), 1);
    // Ours does not. Handing it over is what produced "unsupported call:
    // optscale_kompressor_retrieve" on a real run: a wasted round trip, and a model told
    // the tool it was just offered does not exist.
    assert!(
        !text.contains("optscale_kompressor_retrieve") && !text.contains("call_a"),
        "our own call must not reach a client that never declared it: {text}"
    );
    // Another call is still in the turn, so it really did end in a tool call.
    assert!(
        text.contains("\"finish_reason\":\"tool_calls\""),
        "the turn still ends the way the client expects: {text}"
    );
}

#[tokio::test]
async fn a_streamed_answer_with_no_tool_call_is_relayed_as_the_bytes_it_arrived_as() {
    let harness = Harness::new().await;
    Mock::given(any())
        .respond_with(move |_: &Request| sse(CHAT_ANSWERS))
        .mount(&harness.server)
        .await;

    let mut body = streaming(chat_body(&log_payload(400)));
    // the client asked for usage itself, so nothing about the stream is ours
    body["stream_options"] = json!({"include_usage": true});
    let reply = harness.post("/v1/chat/completions", &body).await;

    assert_eq!(
        reply.text(),
        CHAT_ANSWERS,
        "a passthrough relay re-encodes nothing, CRLF terminators included"
    );
}

#[tokio::test]
async fn include_usage_is_asked_for_and_its_extra_chunk_is_stripped_when_the_client_did_not_ask() {
    let harness = Harness::new().await;
    Mock::given(any())
        .respond_with(move |_: &Request| sse(CHAT_ANSWERS))
        .mount(&harness.server)
        .await;

    let reply = harness
        .post(
            "/v1/chat/completions",
            &streaming(chat_body(&log_payload(400))),
        )
        .await;

    assert_eq!(
        harness.upstream_json().await[0]["stream_options"]["include_usage"],
        true,
        "the saving has to be measured against what the provider billed"
    );
    let stream = reply.text();
    assert!(
        !stream.contains("\"usage\""),
        "a client that did not ask for usage may not expect a choice-less chunk: {stream}"
    );
    assert!(stream.contains("filled."));
    assert!(stream.trim_end().ends_with("data: [DONE]"));
}

#[tokio::test]
async fn a_client_that_asks_for_usage_is_told_what_every_hidden_round_cost() {
    let harness = Harness::new().await;
    mount_stream(&harness, CHAT_ASKS, CHAT_ANSWERS).await;

    let mut body = streaming(chat_body(&log_payload(400)));
    body["stream_options"] = json!({"include_usage": true});
    let reply = harness.post("/v1/chat/completions", &body).await;

    let usage = reply
        .text()
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .find(|frame| frame.get("usage").is_some_and(|usage| usage.is_object()))
        .expect("the usage frame the client asked for");

    assert_eq!(
        usage["usage"]["prompt_tokens"], 2200,
        "the client pays for the hidden round too, so it is told about it"
    );
    assert_eq!(usage["usage"]["completion_tokens"], 50);
}

#[tokio::test]
async fn a_buffering_window_that_overflows_falls_back_to_relaying_rather_than_to_memory() {
    let harness = Harness::with(|config| config.proxy.stream_buffer_bytes = 8).await;
    mount_stream(&harness, CHAT_ASKS, CHAT_ANSWERS).await;

    let reply = harness
        .post(
            "/v1/chat/completions",
            &streaming(chat_body(&log_payload(400))),
        )
        .await;

    // What this is about is the fallback, not the leak. It used to assert that
    // `optscale_kompressor_retrieve` reached the client, because an overflowing buffer
    // relays and nothing then removed our call — so the leak was standing in
    // for "it relayed". Our call is now filtered out of a relayed Chat frame
    // too, so the fallback is asserted directly: the turn arrives, it ends, and
    // exactly one request was made because no retrieve round ran.
    let text = reply.text();
    assert!(
        text.contains("\"finish_reason\":\"tool_calls\"") || text.contains("[DONE]"),
        "an overflowing buffer relays; it does not grow and it does not stall: {text}"
    );
    assert!(
        !text.contains("optscale_kompressor_retrieve"),
        "and relaying is still not a reason to show the client a tool it never \
         declared: {text}"
    );
    assert_eq!(harness.upstream_bodies().await.len(), 1);
}

#[tokio::test]
async fn a_slow_retrieve_round_keeps_the_clients_connection_warm() {
    let harness = Harness::with(|config| {
        config.proxy.stream_keepalive = Duration::from_millis(10);
    })
    .await;
    Mock::given(any())
        .respond_with(move |request: &Request| {
            if round_of(&body_text(request)) == 0 {
                return sse(&CHAT_ASKS.replace("HANDLE", &handle_in(request)));
            }
            // the provider is thinking; the client's SDK is counting the
            // seconds since its last byte
            sse(CHAT_ANSWERS).set_delay(Duration::from_millis(120))
        })
        .mount(&harness.server)
        .await;

    let reply = harness
        .post(
            "/v1/chat/completions",
            &streaming(chat_body(&log_payload(400))),
        )
        .await;

    let stream = reply.text();
    assert!(
        stream.contains(": kompressor retrieving"),
        "a comment is a frame every client ignores, which is what makes it a \
         keep-alive: {stream}"
    );
    assert!(stream.contains("filled."));
}

#[tokio::test]
async fn a_streamed_upstream_error_reaches_the_client_as_the_upstream_wrote_it() {
    let harness = Harness::new().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "error": {"message": "overloaded", "type": "overloaded_error"}
        })))
        .mount(&harness.server)
        .await;

    let reply = harness
        .post(
            "/v1/messages",
            &streaming(anthropic_body(&log_payload(400))),
        )
        .await;

    assert_eq!(reply.status, 503);
    assert_eq!(reply.json()["error"]["message"], "overloaded");
}

#[tokio::test]
async fn the_round_cap_trips_on_a_stream_too_and_the_client_still_gets_an_answer() {
    let harness = Harness::with(|config| config.proxy.retrieve_max_rounds = 1).await;
    Mock::given(any())
        .respond_with(move |request: &Request| {
            let text = body_text(request);
            if text.contains("retrieval budget exhausted") {
                return sse(CHAT_ANSWERS);
            }
            sse(&CHAT_ASKS.replace("HANDLE", &handle_in(request)))
        })
        .mount(&harness.server)
        .await;

    let reply = harness
        .post(
            "/v1/chat/completions",
            &streaming(chat_body(&log_payload(400))),
        )
        .await;

    assert!(reply.text().contains("filled."));
    assert!(
        !reply.text().contains("optscale_kompressor_retrieve"),
        "a refused round is still a round the client is kept out of"
    );
    assert_eq!(
        harness.upstream_bodies().await.len(),
        3,
        "one real round, one refusal, one answer"
    );
}

/// A smoke run on real traffic produced one request that was compressed and
/// forwarded and then vanished: the agent hit its SSE idle timeout, hung up
/// mid-stream, and the proxy recorded nothing at all for it. It was invisible in
/// the status counts, and the tokens it had already bought were invisible too.
///
/// Two things have to hold now. The relay must notice the client leaving
/// *without* having to write to it — while buffering it writes nothing, which is
/// exactly when the disconnect used to go unseen — and the row it leaves behind
/// must say the client was gone, because a silent 200 for a completion nobody
/// received is worse than no row at all.
#[tokio::test]
async fn a_client_that_hangs_up_mid_stream_still_leaves_a_row_that_says_so() {
    let harness = Harness::new().await;
    Mock::given(any())
        // Long enough that the client is provably gone before the relay has
        // anything to emit. That the relay also notices a departure it never
        // writes to is covered by `pull_tests` in the crate itself, where a
        // stream that never speaks can be held still.
        .respond_with(sse(CHAT_ANSWERS).set_delay(Duration::from_millis(200)))
        .mount(&harness.server)
        .await;

    harness
        .post_and_abandon(
            "/v1/chat/completions",
            &streaming(chat_body(&log_payload(400))),
        )
        .await;

    // the relay has to end promptly rather than waiting the provider out
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let row = loop {
        if let Some(row) = support::proxy_rows(&harness.events).pop() {
            break row;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no proxy_requests row was recorded for the abandoned request"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    assert_eq!(
        row.error.as_deref(),
        Some("client disconnected before the stream completed"),
        "the row has to distinguish an abandoned stream from a completed one"
    );
}

/// The other half of the same guarantee. A client that reads the answer to its
/// end then closes — which is what every SSE client does — must record a clean
/// row, with the usage that arrives in the last frame.
#[tokio::test]
async fn a_stream_the_client_read_to_the_end_is_not_called_abandoned() {
    let harness = Harness::new().await;
    Mock::given(any())
        .respond_with(sse(CHAT_ANSWERS))
        .mount(&harness.server)
        .await;

    let reply = harness
        .post(
            "/v1/chat/completions",
            &streaming(chat_body(&log_payload(400))),
        )
        .await;
    assert!(reply.text().contains("data:"), "the client got its answer");

    let rows = support::proxy_rows(&harness.events);
    let row = rows.last().expect("a completed request records a row");
    assert_eq!(
        row.error, None,
        "closing after the terminal frame is how a request ends, not how it fails"
    );
}

/// The keepalive is an SSE comment, and a client is free to ignore comments
/// when judging a stream idle — one agent does, and gave up after five minutes
/// having been sent thirty of them. So the relay also bounds how *long* it will
/// hold frames back, not only how many bytes: past the bound it hands over what
/// it has, because frames the client can see are the only thing guaranteed to
/// reset its timer.
#[tokio::test]
async fn a_long_hold_flushes_to_the_client_rather_than_waiting_it_out() {
    let harness = Harness::with(|config| {
        // Immediately: every frame arrives after the bound has passed.
        config.proxy.stream_buffer_timeout = Duration::from_millis(1);
        // Far above anything here, so the byte escape cannot be what fires.
        config.proxy.stream_buffer_bytes = 64 * 1024 * 1024;
    })
    .await;
    Mock::given(any())
        .respond_with(sse(CHAT_ANSWERS))
        .mount(&harness.server)
        .await;

    let reply = harness
        .post(
            "/v1/chat/completions",
            &streaming(chat_body(&log_payload(400))),
        )
        .await;
    assert!(
        reply.text().contains("data:"),
        "the client gets the frames rather than silence"
    );
}

/// And the bound is off when it is zero, leaving the byte escape as the only
/// one — which is what a deployment that would rather keep every interception
/// sets.
#[tokio::test]
async fn a_zero_bound_leaves_the_hold_to_the_byte_limit_alone() {
    let harness = Harness::with(|config| {
        config.proxy.stream_buffer_timeout = Duration::ZERO;
    })
    .await;
    Mock::given(any())
        .respond_with(move |request: &Request| {
            if round_of(&body_text(request)) == 0 {
                return sse(&CHAT_ASKS.replace("HANDLE", &handle_in(request)));
            }
            sse(CHAT_ANSWERS)
        })
        .mount(&harness.server)
        .await;

    let reply = harness
        .post(
            "/v1/chat/completions",
            &streaming(chat_body(&log_payload(400))),
        )
        .await;
    assert!(
        !reply.text().contains("optscale_kompressor_retrieve"),
        "with no time bound the retrieve turn is still swallowed whole"
    );
    assert_eq!(
        harness.upstream_bodies().await.len(),
        2,
        "and the round was run"
    );
}

/// With the shipped default the same turn is swallowed whole, which is the
/// whole point of buffering.
#[tokio::test]
async fn the_shipped_default_still_intercepts_the_retrieve_turn() {
    let harness = Harness::new().await;
    Mock::given(any())
        .respond_with(move |request: &Request| {
            if round_of(&body_text(request)) == 0 {
                return sse(&CHAT_ASKS.replace("HANDLE", &handle_in(request)));
            }
            sse(CHAT_ANSWERS)
        })
        .mount(&harness.server)
        .await;

    let reply = harness
        .post(
            "/v1/chat/completions",
            &streaming(chat_body(&log_payload(400))),
        )
        .await;
    assert!(
        !reply.text().contains("optscale_kompressor_retrieve"),
        "the client must never see our own tool call"
    );
    assert_eq!(harness.upstream_bodies().await.len(), 2, "the round ran");
}

/// The exact shape that produced `unsupported call: optscale_kompressor_retrieve` on a
/// paid run: a Responses turn carrying our call *and* one the client owns.
///
/// The retrieve call is not answerable by the relay — the client's own call has
/// to reach it — but handing ours over with it costs a round trip and tells the
/// model the tool it was just offered does not exist.
#[tokio::test]
async fn a_responses_turn_mixing_our_call_with_the_clients_hands_over_only_theirs() {
    let harness = Harness::new().await;
    let mixed = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_a\",\"name\":\"optscale_kompressor_retrieve\",\"arguments\":\"\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"ref\\\":\\\"k_AbCdEfGhIjKl\\\"}\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_a\",\"name\":\"optscale_kompressor_retrieve\",\"arguments\":\"{}\"}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"fc_2\",\"call_id\":\"call_b\",\"name\":\"read_file\",\"arguments\":\"\"}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"fc_2\",\"call_id\":\"call_b\",\"name\":\"read_file\",\"arguments\":\"{}\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[{\"type\":\"function_call\",\"call_id\":\"call_a\",\"name\":\"optscale_kompressor_retrieve\"},{\"type\":\"function_call\",\"call_id\":\"call_b\",\"name\":\"read_file\"}],\"usage\":{\"input_tokens\":1000,\"output_tokens\":20}}}\n\n",
    );
    Mock::given(any())
        .respond_with(move |_: &Request| sse(mixed))
        .mount(&harness.server)
        .await;

    let reply = harness
        .post(
            "/v1/responses",
            &streaming(responses_body(&log_payload(400))),
        )
        .await;
    let text = reply.text();

    assert!(
        text.contains("read_file") && text.contains("call_b"),
        "the client's own call has to arrive: {text}"
    );
    assert!(
        !text.contains("optscale_kompressor_retrieve"),
        "ours must not, in any frame — including the terminal one, which repeats \
         the whole output list: {text}"
    );
    assert!(
        !text.contains("call_a"),
        "and neither may its id, or the client answers a call it never saw: {text}"
    );
    assert!(
        text.contains("\"type\":\"response.completed\""),
        "the turn still ends: {text}"
    );
    assert_eq!(harness.upstream_bodies().await.len(), 1, "no round was run");
}

/// The same turn with the two calls the other way round.
///
/// The ordering matters because the relay hands over as soon as it sees a call
/// the client owns, and `hand_over` can only inspect the frames it is holding.
/// With the client's call first, ours had not arrived yet — so the relay
/// decided nothing of ours needed removing and went to passthrough with
/// scrubbing off. Our call's frames, and the terminal frame that repeats the
/// whole output list, then reached a client that never declared the tool.
#[tokio::test]
async fn a_responses_turn_whose_client_call_comes_first_still_hides_ours() {
    let harness = Harness::new().await;
    let mixed = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_2\",\"call_id\":\"call_b\",\"name\":\"read_file\",\"arguments\":\"\"}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_2\",\"call_id\":\"call_b\",\"name\":\"read_file\",\"arguments\":\"{}\"}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_a\",\"name\":\"optscale_kompressor_retrieve\",\"arguments\":\"\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"ref\\\":\\\"k_AbCdEfGhIjKl\\\"}\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_a\",\"name\":\"optscale_kompressor_retrieve\",\"arguments\":\"{}\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[{\"type\":\"function_call\",\"call_id\":\"call_b\",\"name\":\"read_file\"},{\"type\":\"function_call\",\"call_id\":\"call_a\",\"name\":\"optscale_kompressor_retrieve\"}],\"usage\":{\"input_tokens\":1000,\"output_tokens\":20}}}\n\n",
    );
    Mock::given(any())
        .respond_with(move |_: &Request| sse(mixed))
        .mount(&harness.server)
        .await;

    let reply = harness
        .post(
            "/v1/responses",
            &streaming(responses_body(&log_payload(400))),
        )
        .await;
    let text = reply.text();

    assert!(
        text.contains("read_file") && text.contains("call_b"),
        "the client's own call has to arrive: {text}"
    );
    assert!(
        !text.contains("optscale_kompressor_retrieve"),
        "ours must not, in any frame — the ordering must not decide this: {text}"
    );
    assert!(!text.contains("call_a"), "and neither may its id: {text}");
    assert!(
        text.contains("\"type\":\"response.completed\""),
        "the turn still ends: {text}"
    );
}

/// A turn with no call of ours must keep its own stop reason.
///
/// Scrubbing used to be armed by the *hand-over* rather than by a removal, so a
/// turn that never carried a `optscale_kompressor_retrieve` call still had its ending
/// rewritten. On Responses that is harmless — `scrub` there only removes items
/// named ours — but the Chat and Anthropic arms rewrite `finish_reason` /
/// `stop_reason` from the foreign-call flag alone. A legitimate client tool call
/// arrived with `finish_reason: stop`, and a client that dispatches on the stop
/// reason does not run it.
///
/// Narration first so the relay hands over before the call is seen, which is
/// the ordering that armed it.
#[tokio::test]
async fn a_turn_carrying_only_the_clients_call_keeps_its_finish_reason() {
    // A tight content grace so the relay hands over on the narration, BEFORE
    // the client's call is seen. That is the ordering that armed the rewrite:
    // at hand-over the turn has no foreign call yet, so `others_remain` is
    // false and `scrub`'s Chat arm rewrites the finish reason of a call it
    // never saw. With the grace left at its default the hand-over happens on
    // the call itself, `others_remain` is already true, and the bug hides.
    let harness = Harness::with(|config| config.proxy.stream_content_grace_bytes = 8).await;
    let narrated = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"let me look that up, it will take a moment\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_b\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(any())
        .respond_with(move |_: &Request| sse(narrated))
        .mount(&harness.server)
        .await;

    let reply = harness
        .post(
            "/v1/chat/completions",
            &streaming(chat_body(&log_payload(400))),
        )
        .await;
    let text = reply.text();

    assert!(
        text.contains("read_file") && text.contains("call_b"),
        "the client's own call has to arrive: {text}"
    );
    assert!(
        text.contains("\"finish_reason\":\"tool_calls\""),
        "and the turn must still say it ended in a tool call, or the client \
         never dispatches it: {text}"
    );
}

/// Chat coalesces several calls into one delta, and both orders were wrong.
///
/// `observe_chat` classified a frame by whichever call came *last*, so
/// `[ours, theirs]` read as `OtherCall` and hand-over forwarded the whole frame
/// — our undeclared call with it — while `[theirs, ours]` read as
/// `RetrieveCall` and the relay ran the *client's* call through the retriever
/// instead of letting the client execute it. Classification now gives a foreign
/// call precedence, and ours is filtered out of the bytes rather than the frame
/// being dropped whole, which would take the client's call with it.
#[tokio::test]
async fn a_chat_delta_carrying_both_calls_keeps_theirs_and_drops_ours() {
    for (label, calls) in [
        (
            "ours first",
            concat!(
                "{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"optscale_kompressor_retrieve\",\"arguments\":\"{}\"}},",
                "{\"index\":1,\"id\":\"call_b\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{}\"}}"
            ),
        ),
        (
            "theirs first",
            concat!(
                "{\"index\":0,\"id\":\"call_b\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{}\"}},",
                "{\"index\":1,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"optscale_kompressor_retrieve\",\"arguments\":\"{}\"}}"
            ),
        ),
    ] {
        let harness = Harness::new().await;
        let mixed = format!(
            concat!(
                "data: {{\"choices\":[{{\"delta\":{{\"role\":\"assistant\"}}}}]}}\n\n",
                "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{calls}]}}}}]}}\n\n",
                "data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n",
                "data: [DONE]\n\n",
            ),
            calls = calls
        );
        Mock::given(any())
            .respond_with(move |_: &Request| sse(&mixed))
            .mount(&harness.server)
            .await;

        let reply = harness
            .post(
                "/v1/chat/completions",
                &streaming(chat_body(&log_payload(400))),
            )
            .await;
        let text = reply.text();

        assert!(
            text.contains("read_file") && text.contains("call_b"),
            "[{label}] the client's own call has to survive: {text}"
        );
        assert!(
            !text.contains("optscale_kompressor_retrieve"),
            "[{label}] and ours must not reach a client that never declared it: {text}"
        );
        assert!(
            !text.contains("call_a"),
            "[{label}] nor its id, or the client answers a call it never saw: {text}"
        );
    }
}

/// A terminal frame that needs scrubbing still reports the hidden round.
///
/// `emit` is where a hidden retrieval round's usage is folded into the frame the
/// client sees. Every rewritten frame used to go straight to `send`, skipping
/// it — and on Responses the terminal `response.completed` is a single frame
/// carrying *both* the output list our call has to come out of and the usage.
/// So the client was told only the visible round's cost while the proxy's own
/// accounting counted both. Chat and Anthropic put those in separate frames,
/// which is why this is the family that shows it.
#[tokio::test]
async fn a_scrubbed_terminal_frame_still_carries_the_hidden_rounds_usage() {
    let harness = Harness::new().await;
    Mock::given(any())
        .respond_with(move |request: &Request| {
            let text = body_text(request);
            if round_of(&text) == 0 {
                // The hidden round: our call, answered by the proxy. 20 out.
                return sse(&RESPONSES_ASKS.replace("HANDLE", &handle_in(request)));
            }
            // The visible turn: the client's call, ours still in the output
            // list of a terminal frame that also carries 30 output tokens.
            sse(concat!(
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_2\",\"call_id\":\"call_b\",\"name\":\"read_file\",\"arguments\":\"\"}}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_2\",\"call_id\":\"call_b\",\"name\":\"read_file\",\"arguments\":\"{}\"}}\n\n",
                // Ours arrives after the hand-over, so it is dropped in
                // passthrough — which is what arms the terminal scrub.
                "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_a\",\"name\":\"optscale_kompressor_retrieve\",\"arguments\":\"\"}}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_a\",\"name\":\"optscale_kompressor_retrieve\",\"arguments\":\"{}\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\",\"output\":[{\"type\":\"function_call\",\"call_id\":\"call_b\",\"name\":\"read_file\"},{\"type\":\"function_call\",\"call_id\":\"call_a\",\"name\":\"optscale_kompressor_retrieve\"}],\"usage\":{\"input_tokens\":1200,\"output_tokens\":30}}}\n\n",
            ))
        })
        .mount(&harness.server)
        .await;

    let reply = harness
        .post(
            "/v1/responses",
            &streaming(responses_body(&log_payload(400))),
        )
        .await;
    let text = reply.text();

    assert!(
        !text.contains("optscale_kompressor_retrieve"),
        "the terminal frame still has to have our call taken out of it: {text}"
    );
    let completed = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .find(|frame| frame.get("type").and_then(Value::as_str) == Some("response.completed"))
        .expect("the terminal frame");
    let out = completed["response"]["usage"]["output_tokens"]
        .as_u64()
        .unwrap_or(0);
    assert!(
        out > 30,
        "scrubbing that frame must not lose the hidden round's usage with it; \
         got {out}, expected the 30 visible plus the 20 hidden"
    );
}

#[tokio::test]
async fn a_model_ignoring_streamed_refusals_ends_with_an_error_for_every_family() {
    for (path, body, asks, terminal) in [
        (
            "/v1/chat/completions",
            chat_body(&log_payload(400)),
            CHAT_ASKS,
            "[DONE]",
        ),
        (
            "/v1/responses",
            responses_body(&log_payload(400)),
            RESPONSES_ASKS,
            "response.completed",
        ),
        (
            "/v1/messages",
            anthropic_body(&log_payload(400)),
            ANTHROPIC_ASKS,
            "message_stop",
        ),
    ] {
        for cap in [0, 1] {
            let harness = Harness::with(|config| config.proxy.retrieve_max_rounds = cap).await;
            Mock::given(any())
                .respond_with(move |request: &Request| {
                    sse(&asks.replace("HANDLE", &handle_in(request)))
                })
                .mount(&harness.server)
                .await;
            let reply = harness.post(path, &streaming(body.clone())).await;
            assert!(
                reply.text().contains("retrieve_loop_not_terminating"),
                "{}",
                reply.text()
            );
            assert!(
                !reply
                    .text()
                    .contains("\"name\":\"optscale_kompressor_retrieve\"")
            );
            assert!(
                !reply.text().contains(terminal),
                "a failure must not end as a completed answer"
            );
            assert_eq!(harness.upstream_bodies().await.len(), cap as usize + 3);
        }
    }
}

#[tokio::test]
async fn a_failed_hidden_stream_round_is_an_error_instead_of_an_empty_answer() {
    let harness = Harness::new().await;
    Mock::given(any())
        .respond_with(move |request: &Request| {
            if round_of(&body_text(request)) == 0 {
                sse(&CHAT_ASKS.replace("HANDLE", &handle_in(request)))
            } else {
                ResponseTemplate::new(503)
            }
        })
        .mount(&harness.server)
        .await;
    let reply = harness
        .post(
            "/v1/chat/completions",
            &streaming(chat_body(&log_payload(400))),
        )
        .await;
    assert!(reply.text().contains("retrieve_round_failed"));
    assert!(!reply.text().contains("[DONE]"));
    assert!(
        !reply
            .text()
            .contains("\"name\":\"optscale_kompressor_retrieve\"")
    );
}

/// A turn that narrates the way a real model does before it calls the tool.
///
/// Every other streaming test here calls the tool within a handful of frames.
/// A live `gpt-5.5` turn does not: it emits reasoning and prose first, in many
/// small deltas, and `content_gives_up` counts **raw frame bytes** —
/// `frame.raw().len()`, envelope included — not the text inside them. So a
/// couple of kilobytes of narration can be tens of kilobytes of frames, and the
/// 8 KiB default grace is spent long before the call arrives.
///
/// Observed on a live run: the model called `optscale_kompressor_retrieve` with a valid
/// handle, the relay had already gone passthrough, the call reached codex, and
/// the answer that came back was `"unsupported call: optscale_kompressor_retrieve"` —
/// the error now permanent in that conversation's history, and `retrievals 0`
/// in the run's statistics while the model had plainly asked.
#[tokio::test]
async fn a_turn_that_narrates_before_calling_is_still_intercepted() {
    // ~2 KB of text as 60 deltas, which is what the envelope overhead makes
    // expensive: each frame carries far more JSON than content.
    let narration: String = (0..120)
        .map(|i| {
            format!(
                "data: {{\"id\":\"c1\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"thinking about step {i} of the task \"}}}}]}}\n\n"
            )
        })
        .collect();
    let asks: &'static str = Box::leak(
        format!(
            "data: {{\"id\":\"c1\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\"}}}}]}}\n\n{narration}{}",
            CHAT_ASKS
                .split_once("\n\n")
                .map(|(_, rest)| rest)
                .unwrap_or(CHAT_ASKS)
        )
        .into_boxed_str(),
    );

    let harness = Harness::new().await;
    mount_stream(&harness, asks, CHAT_ANSWERS).await;

    let reply = harness
        .post(
            "/v1/chat/completions",
            &streaming(chat_body(&log_payload(400))),
        )
        .await;
    assert_eq!(reply.status, 200);

    // The fixture has to straddle the distinction or it tests nothing: more
    // than the grace in *frame* bytes, which is what the old rule counted and
    // what made it give up, and less than the grace in *text*, which is what
    // the rule counts now.
    let narration_bytes = narration.len();
    let text_bytes: usize = (0..120)
        .map(|i| format!("thinking about step {i} of the task ").len())
        .sum();
    assert!(
        narration_bytes > 8 * 1024,
        "under the old frame-counting rule this fixture must exceed the grace, \
         or it cannot catch a regression to it: {narration_bytes} frame bytes"
    );
    assert!(
        text_bytes < 8 * 1024,
        "and under the new text-counting rule it must not, or it would give up \
         for a legitimate reason and prove nothing: {text_bytes} text bytes"
    );

    // Two upstream requests means the proxy answered the call itself and ran a
    // round. One means it gave up and relayed the call to a client that cannot
    // answer it.
    let rounds = harness.upstream_bodies().await.len();
    assert_eq!(
        rounds, 2,
        "the model narrated {narration_bytes} bytes of frames before calling, \
         and the relay must still intercept: {rounds} upstream request(s)"
    );
    assert!(
        !reply.text().contains("optscale_kompressor_retrieve"),
        "and our tool call must never reach the client"
    );
}
