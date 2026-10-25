//! What the provider receives, and what the client gets back.

mod support;

use std::time::Duration;

use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, ResponseTemplate};

use support::{
    Harness, NO_TIME_AT_ALL, anthropic_body, chat_body, first_handle, log_payload, responses_body,
};

use kompressor_core::config::RetrieveTool;
use kompressor_proxy::failopen::Stage;

/// A provider answer with no tool call in it.
fn answered() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "chatcmpl-1",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "It ran out of disk."},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 900, "completion_tokens": 12}
    }))
}

/// What the upstream saw, with the retrieve tool taken back out.
///
/// The pipe guarantee these tests hold is "unchanged **apart from a tools block
/// that is the same on every turn**", which is weaker than the byte-identity
/// they used to assert and bought deliberately: a tools block that comes and
/// goes mid-conversation is part of the prompt the provider caches, and moving
/// it costs the cache from that point on (see
/// `the_turn_that_mints_the_first_handle_leaves_the_tools_block_alone`).
///
/// Removing only our own entry, not the whole field, so a client's own tools are
/// still compared: dropping those would let a real mangling through.
fn without_our_tool(body: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };
    let Some(tools) = value.get_mut("tools").and_then(Value::as_array_mut) else {
        return body.to_string();
    };
    tools.retain(|tool| {
        tool["function"]["name"] != "optscale_kompressor_retrieve"
            && tool["name"] != "optscale_kompressor_retrieve"
    });
    if tools.is_empty() {
        value.as_object_mut().map(|object| object.remove("tools"));
    }
    value.to_string()
}

async fn answering(harness: &Harness) {
    Mock::given(any())
        .respond_with(answered())
        .mount(&harness.server)
        .await;
}

#[tokio::test]
async fn an_upstream_using_the_default_family_detects_known_endpoints() {
    let harness = Harness::with(|config| {
        config.proxy.upstreams[0].api_family =
            kompressor_core::config::Upstream::default().api_family;
    })
    .await;
    answering(&harness).await;
    let input = log_payload(400);
    harness
        .post("/v1/chat/completions", &chat_body(&input))
        .await;
    let sent = &harness.upstream_json().await[0];
    assert!(sent["messages"][3]["content"].as_str().unwrap().len() < input.len());
}

#[tokio::test]
async fn an_ordinary_request_is_untouched_by_the_websocket_relay() {
    // The relay ships on, and "on" has to mean *as well as*, not instead.
    //
    // It gates exactly one branch, behind `websocket::is_upgrade`, which needs
    // both `Connection: upgrade` and `Upgrade: websocket`. An ordinary POST
    // carries neither and never reaches it. That is worth a test rather than a
    // comment because the failure would be total and quiet — every HTTP
    // request relayed or refused by a proxy whose health checks all stay green
    // — and because the two transports are meant to run side by side: a codex
    // on a subscription socket and an API-key client on HTTP, through one
    // listener, at the same time.
    assert!(
        kompressor_core::config::Config::default()
            .proxy
            .websocket_relay,
        "this asserts the relay does not disturb the HTTP path, which is only \
         worth asserting while it defaults on"
    );

    let harness = Harness::new().await;
    answering(&harness).await;

    let input = log_payload(400);
    let reply = harness
        .post("/v1/chat/completions", &chat_body(&input))
        .await;

    assert_eq!(reply.status, 200);
    let sent = &harness.upstream_json().await[0];
    assert!(
        sent["messages"][3]["content"].as_str().unwrap().len() < input.len(),
        "with the relay on, a plain request must still be routed and compressed"
    );
}

#[tokio::test]
async fn a_request_below_the_size_gate_reaches_the_upstream_byte_identical() {
    let harness = Harness::new().await;
    answering(&harness).await;

    let body = chat_body("a short tool result");
    let reply = harness.post("/v1/chat/completions", &body).await;

    assert_eq!(reply.status, 200);
    assert_eq!(
        without_our_tool(&harness.upstream_bodies().await[0]),
        body.to_string(),
        "under the gate the proxy is a pipe apart from the stable tools block: \
         not a re-serialization, not a reordering"
    );
}

#[tokio::test]
async fn a_zstd_encoded_request_is_compressed_like_any_other() {
    // codex-cli 0.155.1 sends `content-encoding: zstd` on every request. Before
    // this was handled the body failed to parse, the proxy forwarded it whole,
    // and a wrapped codex session compressed nothing at all while reporting
    // success — so this is the test that the client works, not a codec detail.
    let harness = Harness::new().await;
    answering(&harness).await;

    let payload = log_payload(400);
    let sent = harness
        .post_zstd("/v1/chat/completions", &chat_body(&payload))
        .await;
    assert_eq!(sent.status, 200);

    // The upstream must receive something it can read: same coding in, same
    // coding out, because the client's `content-encoding` is forwarded.
    let raw = &harness.upstream_raw_bodies().await[0];
    let decoded = zstd::stream::decode_all(&raw[..]).expect("still zstd on the wire");
    let upstream: serde_json::Value = serde_json::from_slice(&decoded).unwrap();

    let compressed = upstream["messages"][3]["content"].as_str().unwrap();
    assert!(
        compressed.len() < payload.len(),
        "an encoded body is compressed like any other, or the client gets nothing"
    );
    assert!(compressed.contains("ref:k_"));
}

#[tokio::test]
async fn an_encoded_request_the_engine_declines_reaches_the_upstream_byte_identical() {
    // The fail-open promise has to survive the detour through the decoder:
    // under the size gate nothing is rewritten, so the bytes that arrived are
    // the bytes that leave — not a re-encode that happens to mean the same.
    //
    // Asserted with the tool off, because that is the only state in which the
    // proxy touches nothing at all. With it on, the request is rewritten by
    // design and the byte-identity below could not hold; the second half checks
    // what still must — that rewriting does not lose the client's coding.
    let harness = Harness::with(|config| config.proxy.retrieve_tool = RetrieveTool::Never).await;
    answering(&harness).await;

    let body = chat_body("a short tool result");
    let encoded = zstd::stream::encode_all(body.to_string().as_bytes(), 3).unwrap();
    let reply = harness.post_zstd("/v1/chat/completions", &body).await;

    assert_eq!(reply.status, 200);
    assert_eq!(
        harness.upstream_raw_bodies().await[0],
        encoded,
        "an untouched request is forwarded exactly as it arrived, coding and all"
    );

    // And with the tool on: still zstd, still the same request underneath.
    let injecting = Harness::new().await;
    answering(&injecting).await;
    assert_eq!(
        injecting
            .post_zstd("/v1/chat/completions", &body)
            .await
            .status,
        200
    );
    let sent = &injecting.upstream_raw_bodies().await[0];
    let decoded = zstd::stream::decode_all(sent.as_slice())
        .expect("a rewritten body must still be zstd, or the coding header lies about it");
    assert_eq!(
        without_our_tool(&String::from_utf8(decoded).unwrap()),
        body.to_string(),
        "the rewrite adds the tool and changes nothing else"
    );
}

#[tokio::test]
async fn above_the_gate_the_tool_result_is_compressed_and_every_other_byte_is_unchanged() {
    let harness = Harness::new().await;
    answering(&harness).await;

    let payload = log_payload(400);
    let sent = harness
        .post("/v1/chat/completions", &chat_body(&payload))
        .await;
    assert_eq!(sent.status, 200);

    let upstream = &harness.upstream_json().await[0];
    let messages = upstream["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0]["content"], "You are a careful assistant.");
    assert_eq!(messages[1]["content"], "What went wrong in this log?");
    assert_eq!(
        messages[2]["tool_calls"][0]["function"]["arguments"], "{\"path\":\"/var/log/app.log\"}",
        "an assistant turn's tool arguments are never touched"
    );

    let compressed = messages[3]["content"].as_str().unwrap();
    assert!(
        compressed.len() < payload.len(),
        "the tool result is what we came for"
    );
    assert!(
        compressed.contains("ref:k_"),
        "recoverable mode leaves a handle behind, or there is nothing to retrieve"
    );
    assert!(
        sent.header("x-kompressor-saved-tokens")
            .parse::<u32>()
            .unwrap()
            > 0,
        "the client is told what was removed"
    );
}

#[tokio::test]
async fn an_anthropic_frozen_prefix_and_its_cache_control_block_reach_the_upstream_unchanged() {
    let harness = Harness::new().await;
    answering(&harness).await;

    let pinned = log_payload(400);
    let live = log_payload(400);
    let body = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_0",
                 "cache_control": {"type": "ephemeral"}, "content": pinned}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": live}
            ]}
        ]
    });

    harness.post("/v1/messages", &body).await;
    let upstream = &harness.upstream_json().await[0];

    assert_eq!(
        upstream["messages"][0]["content"][0]["content"], pinned,
        "a block inside the cached prefix keeps every byte: rewriting one drops \
         the provider's cache hit rate to zero and raises the bill"
    );
    assert_eq!(
        upstream["messages"][0]["content"][0]["cache_control"]["type"], "ephemeral",
        "and keeps its breakpoint"
    );
    assert!(
        upstream["messages"][1]["content"][0]["content"]
            .as_str()
            .unwrap()
            .len()
            < live.len(),
        "everything after the floor is still compressed"
    );
}

/// A configuration with no `proxy.providers` table — every one written before
/// it existed — treats Anthropic and OpenAI traffic alike: the deployment's
/// mode and its `compress` switch reach both families.
#[tokio::test]
async fn an_old_configuration_treats_both_families_alike() {
    let anthropic = |input: &str| {
        json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_0", "content": input}
            ]}]
        })
    };
    let input = log_payload(400);

    let aggressive = Harness::with(|config| {
        config.engine.mode = kompressor_engine::Mode::Aggressive;
    })
    .await;
    answering(&aggressive).await;
    let chat = aggressive
        .post("/v1/chat/completions", &chat_body(&input))
        .await;
    let messages = aggressive.post("/v1/messages", &anthropic(&input)).await;
    assert_eq!(chat.header("x-kompressor-mode"), "aggressive");
    assert_eq!(messages.header("x-kompressor-mode"), "aggressive");

    let off = Harness::with(|config| config.proxy.compress = false).await;
    answering(&off).await;
    let chat = off.post("/v1/chat/completions", &chat_body(&input)).await;
    let messages = off.post("/v1/messages", &anthropic(&input)).await;
    assert_eq!(chat.header("x-kompressor-mode"), "off");
    assert_eq!(messages.header("x-kompressor-mode"), "off");
    let sent = off.upstream_json().await;
    assert_eq!(sent[0]["messages"][3]["content"], input);
    assert_eq!(sent[1]["messages"][0]["content"][0]["content"], input);
}

/// A provider table inherits every setting it does not name: here only
/// OpenAI's `compress` is scoped, and Anthropic still gets the deployment's
/// one-hour TTL and compression.
#[tokio::test]
async fn a_provider_table_inherits_the_settings_it_does_not_name() {
    let harness = Harness::with(|config| {
        config.proxy.cache_ttl = kompressor_core::config::CacheTtl::OneHour;
        config.proxy.providers.insert(
            "openai".into(),
            kompressor_core::config::ProviderOverrides {
                compress: Some(false),
                ..Default::default()
            },
        );
    })
    .await;
    answering(&harness).await;
    let input = log_payload(400);
    let body = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 1024,
        "system": [{"type": "text", "text": "rules", "cache_control": {"type": "ephemeral"}}],
        "messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_0", "content": input}
        ]}]
    });
    let reply = harness.post("/v1/messages", &body).await;
    assert_ne!(reply.header("x-kompressor-mode"), "off");
    let sent = &harness.upstream_json().await[0];
    assert_eq!(
        sent["system"][0]["cache_control"]["ttl"], "1h",
        "the global TTL"
    );
    assert!(
        sent["messages"][0]["content"][0]["content"]
            .as_str()
            .unwrap()
            .len()
            < input.len(),
        "and global compression"
    );
    let reply = harness
        .post("/v1/chat/completions", &chat_body(&input))
        .await;
    assert_eq!(
        reply.header("x-kompressor-mode"),
        "off",
        "while openai's own switch holds"
    );
}

/// `proxy.providers` applies to its provider's traffic and to nothing else:
/// here Anthropic asks for one-hour entries and OpenAI is not compressed, from
/// one proxy serving both.
#[tokio::test]
async fn provider_settings_apply_to_that_provider_only() {
    let harness = Harness::with(|config| {
        use kompressor_core::config::{CacheTtl, ProviderOverrides};
        config.proxy.providers.insert(
            "anthropic".into(),
            ProviderOverrides {
                cache_ttl: Some(CacheTtl::OneHour),
                ..Default::default()
            },
        );
        config.proxy.providers.insert(
            "openai".into(),
            ProviderOverrides {
                compress: Some(false),
                ..Default::default()
            },
        );
    })
    .await;
    answering(&harness).await;

    let input = log_payload(400);
    let reply = harness
        .post("/v1/chat/completions", &chat_body(&input))
        .await;
    assert_eq!(
        reply.header("x-kompressor-mode"),
        "off",
        "openai is not compressed"
    );
    let sent = &harness.upstream_json().await[0];
    assert_eq!(
        sent["messages"][3]["content"], input,
        "and goes out as it came"
    );

    let body = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 1024,
        "system": [{"type": "text", "text": "rules", "cache_control": {"type": "ephemeral"}}],
        "messages": [
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_0", "content": input}
            ]}
        ]
    });
    let reply = harness.post("/v1/messages", &body).await;
    assert_ne!(
        reply.header("x-kompressor-mode"),
        "off",
        "anthropic still compresses"
    );
    let sent = &harness.upstream_json().await[1];
    assert_eq!(
        sent["system"][0]["cache_control"]["ttl"], "1h",
        "and asks for an hour"
    );
    assert!(
        sent["messages"][0]["content"][0]["content"]
            .as_str()
            .unwrap()
            .len()
            < input.len()
    );
}

#[tokio::test]
async fn a_one_hour_cache_ttl_reaches_the_upstream_only_when_asked_for() {
    let body = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 1024,
        "system": [{"type": "text", "text": "rules", "cache_control": {"type": "ephemeral"}}],
        "messages": [
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_0", "content": log_payload(400)},
                {"type": "text", "text": "next", "cache_control": {"type": "ephemeral"}}
            ]}
        ]
    });

    let asked = Harness::with(|config| {
        config.proxy.cache_ttl = kompressor_core::config::CacheTtl::OneHour;
    })
    .await;
    answering(&asked).await;
    asked.post("/v1/messages", &body).await;
    let upstream = &asked.upstream_json().await[0];
    assert_eq!(upstream["system"][0]["cache_control"]["ttl"], "1h");
    assert_eq!(
        upstream["messages"][0]["content"][1]["cache_control"]["ttl"],
        "1h"
    );

    // Compression off still asks for an hour when told to: the TTL is a
    // separate decision from compressing, and the one docs/CLAUDE-CODE.md
    // describes as working either way.
    let off = Harness::with(|config| {
        config.proxy.compress = false;
        config.proxy.cache_ttl = kompressor_core::config::CacheTtl::OneHour;
    })
    .await;
    answering(&off).await;
    off.post("/v1/messages", &body).await;
    let upstream = &off.upstream_json().await[0];
    assert_eq!(upstream["system"][0]["cache_control"]["ttl"], "1h");

    // The default sends the client's markers as written: the TTL is a price
    // decision, and nothing the operator did not ask for changes it.
    let default = Harness::new().await;
    answering(&default).await;
    default.post("/v1/messages", &body).await;
    let upstream = &default.upstream_json().await[0];
    assert!(upstream["system"][0]["cache_control"].get("ttl").is_none());
    assert!(
        upstream["messages"][0]["content"][1]["cache_control"]
            .get("ttl")
            .is_none()
    );
}

#[tokio::test]
async fn a_previous_response_id_protects_the_turn_but_not_the_tool_output() {
    // Earlier turns live on the provider's side, where we cannot see them, so
    // anything this request might be *re-sending* must not be rewritten. That
    // is the turn, not the tool output: the client produced the output by
    // running a tool after the response being quoted, so it cannot be in that
    // history and cannot contradict it.
    let harness = Harness::new().await;
    answering(&harness).await;

    let mut body = responses_body(&log_payload(400));
    body["previous_response_id"] = json!("resp_68a1f0");

    harness.post("/v1/responses", &body).await;
    let sent = &harness.upstream_json().await[0];

    assert_eq!(
        sent["input"][0]["content"][0]["text"], "What went wrong in this log?",
        "a turn that may be a replay must reach the provider unchanged"
    );
    let output = sent["input"][2]["output"]
        .as_str()
        .expect("the tool output");
    assert!(
        output.len() < log_payload(400).len(),
        "a fresh tool output is ours to compress: {} bytes arrived unchanged",
        output.len()
    );
    // The surrounding structure has to survive the rewrite intact.
    assert_eq!(sent["previous_response_id"], "resp_68a1f0");
    assert_eq!(sent["input"][1]["type"], "function_call");
    assert_eq!(sent["input"][2]["call_id"], "call_0");
}

#[tokio::test]
async fn a_previous_response_id_request_with_only_turns_is_forwarded_untouched() {
    // Nothing in it is ours, so it goes out byte for byte — the behaviour the
    // blanket rule used to give every such request.
    let harness = Harness::new().await;
    answering(&harness).await;

    let body = json!({
        "model": "gpt-5",
        "previous_response_id": "resp_68a1f0",
        "input": [{"type": "message", "role": "user",
                   "content": [{"type": "input_text", "text": log_payload(400)}]}]
    });

    harness.post("/v1/responses", &body).await;
    assert_eq!(
        without_our_tool(&harness.upstream_bodies().await[0]),
        body.to_string()
    );
}

#[tokio::test]
async fn the_retrieve_tool_is_offered_to_every_turn_of_a_recoverable_conversation() {
    // This used to assert the opposite — that a request carrying no handle got
    // no tool, on the reasoning that "a tool with nothing to fetch is prompt the
    // client pays for". True, and far cheaper than the alternative. A tools
    // block is part of the prompt the provider caches, so a tool that appears
    // partway through a conversation changes that prompt and the cache is lost
    // from the tools position on. Captured at the byte level on 2026-10-07: at a
    // turn where `cache_read` fell 11,648 -> 10,624 the conversation was
    // byte-identical and the agent's own tools block unchanged, while ours went
    // from 10,622 bytes without `optscale_kompressor_retrieve` to 13,207 with it.
    let harness = Harness::new().await;
    answering(&harness).await;

    harness
        .post("/v1/chat/completions", &chat_body(&log_payload(400)))
        .await;
    let with_handle = &harness.upstream_json().await[0];
    let tools = with_handle["tools"].as_array().expect("a tool list");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["function"]["name"], "optscale_kompressor_retrieve");

    // A payload the engine has nothing to elide still gets the tool, because the
    // block has to be the same on every turn of the conversation.
    let prose = "The quick brown fox jumps over the lazy dog. ".repeat(200);
    harness
        .post("/v1/chat/completions", &chat_body(&prose))
        .await;
    let without = &harness.upstream_json().await[1];
    assert_eq!(
        without["tools"], with_handle["tools"],
        "the tools block must not depend on whether this particular turn \
         elided anything: that is what moves the prefix"
    );
}

#[tokio::test]
async fn the_tool_is_never_offered_when_the_operator_turned_it_off() {
    let harness = Harness::with(|config| config.proxy.retrieve_tool = RetrieveTool::Never).await;
    answering(&harness).await;

    harness
        .post("/v1/chat/completions", &chat_body(&log_payload(400)))
        .await;
    let upstream = &harness.upstream_json().await[0];
    assert!(upstream.get("tools").is_none());
    assert!(
        upstream["messages"][3]["content"]
            .as_str()
            .unwrap()
            .contains("ref:k_"),
        "the compression still happens; only the offer is withheld"
    );
}

#[tokio::test]
async fn a_client_running_its_own_retrieve_loop_is_left_to_run_it() {
    let harness = Harness::new().await;
    // the client declares the tool and expects to answer the call itself
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1",
            "choices": [{"message": {"role": "assistant", "tool_calls": [
                {"id": "call_1", "type": "function",
                 "function": {"name": "optscale_kompressor_retrieve", "arguments": "{\"ref\":\"k_AAAAAAAAAAAA\"}"}}
            ]}, "finish_reason": "tool_calls"}]
        })))
        .mount(&harness.server)
        .await;

    let mut body = chat_body(&log_payload(400));
    body["tools"] = json!([{
        "type": "function",
        "function": {"name": "optscale_kompressor_retrieve", "description": "mine", "parameters": {}}
    }]);
    let reply = harness.post("/v1/chat/completions", &body).await;

    let upstream = &harness.upstream_json().await[0];
    assert_eq!(
        upstream["tools"].as_array().unwrap().len(),
        1,
        "two tools with one name is a request no provider can answer sensibly"
    );
    assert_eq!(upstream["tools"][0]["function"]["description"], "mine");
    assert_eq!(
        reply.json()["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "optscale_kompressor_retrieve",
        "the call belongs to the client that declared the tool"
    );
    assert_eq!(harness.upstream_bodies().await.len(), 1, "no loop ran here");
}

#[tokio::test]
async fn mode_off_forwards_the_request_exactly_as_it_arrived() {
    let harness = Harness::new().await;
    answering(&harness).await;

    let body = chat_body(&log_payload(400));
    let reply = harness
        .post_with(
            "/v1/chat/completions",
            &body,
            &[("x-kompressor-mode", "off")],
        )
        .await;

    assert_eq!(harness.upstream_bodies().await[0], body.to_string());
    assert_eq!(reply.header("x-kompressor-mode"), "off");
}

/// The uncompressed control arm: served, measured, and not compressing.
///
/// A baseline is only worth the money if it still produces the numbers the
/// compressed run is compared against, and if it can be told apart from a
/// compressed run afterwards. So this asserts all three: the upstream gets the
/// bytes that arrived, the response says `off`, and a row is still recorded
/// with the provider's usage on it.
///
/// `proxy.enabled = false` is not this switch and cannot be: `serve` does not
/// mount the proxy router at all when it is false, so every proxied path
/// answers 404 and the baseline loses the network hop it exists to keep.
#[tokio::test]
async fn the_control_arm_forwards_unchanged_says_so_and_still_records() {
    let harness = Harness::with(|config| config.proxy.compress = false).await;
    answering(&harness).await;

    let body = chat_body(&log_payload(400));
    let reply = harness.post("/v1/chat/completions", &body).await;

    assert_eq!(harness.upstream_bodies().await[0], body.to_string());
    assert_eq!(reply.header("x-kompressor-mode"), "off");

    let rows: Vec<_> = harness
        .events
        .drain()
        .into_iter()
        .filter_map(|event| match event {
            kompressor_stats::events::Event::ProxyRequest(e) => Some(e),
            _ => None,
        })
        .collect();
    assert_eq!(rows.len(), 1, "a forwarded request is still a recorded one");
    assert_eq!(
        rows[0].model.as_deref(),
        body["model"].as_str(),
        "the control arm is priced per model like the treated one"
    );
    assert!(
        rows[0].conversation.is_some(),
        "and grouped into conversations, or its cache losses go unseen"
    );
    assert!(
        rows[0].prompt_tokens.is_some_and(|tokens| tokens > 0),
        "the control arm's prompt tokens are the baseline -- without them the \
         run is an expensive way to prove the proxy can forward bytes"
    );
}

/// A deployment that says it does not compress is not talked out of it.
#[tokio::test]
async fn a_request_header_cannot_turn_compression_back_on() {
    let harness = Harness::with(|config| config.proxy.compress = false).await;
    answering(&harness).await;

    let body = chat_body(&log_payload(400));
    let reply = harness
        .post_with(
            "/v1/chat/completions",
            &body,
            &[("x-kompressor-mode", "recoverable")],
        )
        .await;

    assert_eq!(harness.upstream_bodies().await[0], body.to_string());
    assert_eq!(reply.header("x-kompressor-mode"), "off");
}

#[tokio::test]
async fn client_credentials_reach_the_provider_and_connection_headers_end_here() {
    let harness = Harness::new().await;
    answering(&harness).await;

    harness
        .post_with(
            "/v1/messages",
            &anthropic_body(&log_payload(400)),
            &[
                ("x-api-key", "ant-live-key"),
                ("anthropic-version", "2023-06-01"),
                ("anthropic-beta", "prompt-caching-2024-07-31"),
                ("authorization", "Bearer sk-live"),
                ("accept-encoding", "gzip"),
                ("x-kompressor-mode", "recoverable"),
            ],
        )
        .await;

    let headers = harness.upstream_headers().await;
    assert_eq!(headers["x-api-key"], "ant-live-key");
    assert_eq!(headers["anthropic-version"], "2023-06-01");
    assert_eq!(headers["anthropic-beta"], "prompt-caching-2024-07-31");
    assert_eq!(headers["authorization"], "Bearer sk-live");
    assert_eq!(
        headers["accept-encoding"], "identity",
        "a compressed response is one the retrieve loop cannot read"
    );
    assert!(!headers.contains_key("x-kompressor-mode"));
    assert!(headers.contains_key("x-request-id"));
}

#[tokio::test]
async fn the_response_says_what_the_proxy_did_and_carries_the_clients_own_request_id() {
    let harness = Harness::new().await;
    answering(&harness).await;

    let reply = harness
        .post_with(
            "/v1/chat/completions",
            &chat_body(&log_payload(400)),
            &[("x-request-id", "client-trace-42")],
        )
        .await;

    assert_eq!(reply.header("x-request-id"), "client-trace-42");
    assert_eq!(reply.header("x-kompressor-mode"), "recoverable");
    assert_eq!(reply.header("x-kompressor-retrieve-rounds"), "0");
    assert!(
        reply
            .header("x-kompressor-saved-tokens")
            .parse::<u32>()
            .unwrap()
            > 0
    );
    assert_eq!(
        harness.upstream_headers().await["x-request-id"],
        "client-trace-42",
        "one id joins the client's logs, ours and the provider's"
    );
}

#[tokio::test]
async fn a_compression_that_runs_past_its_deadline_forwards_the_original_prompt() {
    let harness = Harness::with(|config| config.proxy.compress_timeout = NO_TIME_AT_ALL).await;
    answering(&harness).await;

    let body = chat_body(&log_payload(400));
    let reply = harness.post("/v1/chat/completions", &body).await;

    assert_eq!(reply.status, 200);
    assert_eq!(
        without_our_tool(&harness.upstream_bodies().await[0]),
        body.to_string(),
        "the deadline costs the saving, never the request"
    );
    assert_eq!(harness.failopen.count(Stage::Compress), 1);
}

#[tokio::test]
async fn a_body_that_is_not_json_is_forwarded_as_the_bytes_it_arrived_as() {
    let harness = Harness::new().await;
    answering(&harness).await;

    let reply = harness
        .send(
            http::Request::post("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(axum::body::Body::from("{\"messages\": [ truncated"))
                .unwrap(),
        )
        .await;

    assert_eq!(reply.status, 200);
    assert_eq!(
        harness.upstream_bodies().await[0],
        "{\"messages\": [ truncated"
    );
    assert_eq!(harness.failopen.count(Stage::Parse), 1);
}

#[tokio::test]
async fn a_shape_no_walker_recognises_is_relayed_rather_than_guessed_at() {
    let harness = Harness::new().await;
    answering(&harness).await;

    let body = json!({"model": "gpt-5", "messages": "not the shape this api takes"});
    harness.post("/v1/chat/completions", &body).await;

    assert_eq!(harness.upstream_bodies().await[0], body.to_string());
    assert_eq!(harness.failopen.count(Stage::Walk), 1);
}

#[tokio::test]
async fn an_upstream_error_reaches_the_client_as_the_upstream_wrote_it() {
    let harness = Harness::new().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": {"message": "rate limit exceeded", "type": "rate_limit_error"}
        })))
        .mount(&harness.server)
        .await;

    let reply = harness
        .post("/v1/chat/completions", &chat_body(&log_payload(400)))
        .await;

    assert_eq!(reply.status, 429, "a provider error is not ours to reshape");
    assert_eq!(reply.json()["error"]["message"], "rate limit exceeded");
}

#[tokio::test]
async fn a_path_no_upstream_claims_is_a_404_rather_than_a_guess() {
    let harness = Harness::with(|config| {
        config.proxy.upstreams[0].path_prefix = "/openai".to_string();
    })
    .await;
    answering(&harness).await;

    let reply = harness
        .post("/somewhere/else", &json!({"hello": "world"}))
        .await;
    assert_eq!(reply.status, 404);
    assert!(harness.upstream_bodies().await.is_empty());
}

#[tokio::test]
async fn a_non_completion_endpoint_is_relayed_without_being_parsed() {
    let harness = Harness::new().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&harness.server)
        .await;

    let body = json!({"input": log_payload(400), "model": "text-embedding-3-large"});
    let reply = harness.post("/v1/embeddings", &body).await;

    assert_eq!(reply.status, 200);
    assert_eq!(harness.upstream_bodies().await[0], body.to_string());
}

#[tokio::test]
async fn a_mount_prefix_is_stripped_before_the_endpoint_is_recognised() {
    let harness = Harness::with(|config| {
        config.proxy.upstreams[0].path_prefix = "/openai".to_string();
        config.proxy.upstreams[0].strip_prefix = true;
    })
    .await;
    answering(&harness).await;

    harness
        .post("/openai/v1/chat/completions", &chat_body(&log_payload(400)))
        .await;

    let received = harness.server.received_requests().await.unwrap();
    assert_eq!(received[0].url.path(), "/v1/chat/completions");
    let upstream: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert!(
        upstream["messages"][3]["content"]
            .as_str()
            .unwrap()
            .contains("ref:k_"),
        "the family is recognised from the path the provider sees, not ours"
    );
}

#[tokio::test]
async fn a_request_larger_than_the_cap_is_refused_rather_than_half_forwarded() {
    let harness = Harness::with(|config| config.proxy.max_body_bytes = 512).await;
    answering(&harness).await;

    let reply = harness
        .post("/v1/chat/completions", &chat_body(&log_payload(400)))
        .await;

    assert_eq!(reply.status, 413);
    assert!(harness.upstream_bodies().await.is_empty());
}

#[tokio::test]
async fn an_unreachable_upstream_is_a_gateway_error_and_not_a_pretend_answer() {
    let harness = Harness::with(|config| {
        // a port nothing is listening on
        config.proxy.upstreams[0].base_url = "http://127.0.0.1:1".to_string();
    })
    .await;

    let reply = harness
        .post("/v1/chat/completions", &chat_body("short"))
        .await;
    assert_eq!(reply.status, 502);
    assert_eq!(reply.json()["error"]["type"], "kompressor_proxy_error");
}

#[tokio::test]
async fn a_tenants_configured_mode_applies_when_the_request_names_none() {
    use kompressor_core::config::{ApiKey, AuthConfig, AuthMode};

    let harness = Harness::with(|config| {
        config.auth = AuthConfig {
            mode: AuthMode::ApiKeys,
            keys: vec![ApiKey {
                key: "sk-team-a".into(),
                tenant: "team-a".into(),
                enabled: true,
                mode: Some(kompressor_engine::Mode::Aggressive),
            }],
            secret: String::new(),
        };
    })
    .await;
    answering(&harness).await;

    let reply = harness
        .post_with(
            "/v1/chat/completions",
            &chat_body(&log_payload(400)),
            &[("x-kompressor-key", "sk-team-a")],
        )
        .await;

    assert_eq!(reply.header("x-kompressor-mode"), "aggressive");
    let upstream = &harness.upstream_json().await[0];
    assert!(
        !upstream["messages"][3]["content"]
            .as_str()
            .unwrap()
            .contains("ref:k_"),
        "aggressive elides without retaining, so there is no handle to offer"
    );
    assert!(upstream.get("tools").is_none());
}

#[tokio::test]
async fn a_credential_the_proxy_cannot_resolve_costs_attribution_and_not_the_request() {
    use kompressor_core::config::{AuthConfig, AuthMode};

    let harness = Harness::with(|config| {
        config.auth = AuthConfig {
            mode: AuthMode::ApiKeys,
            keys: Vec::new(),
            secret: String::new(),
        };
    })
    .await;
    answering(&harness).await;

    // the client is presenting its *provider* key, which is none of our
    // business; answering 401 here would break a call the provider would have
    // answered
    let reply = harness
        .post_with(
            "/v1/chat/completions",
            &chat_body(&log_payload(400)),
            &[("authorization", "Bearer sk-openai-live")],
        )
        .await;

    assert_eq!(reply.status, 200);
    assert_eq!(harness.failopen.count(Stage::Auth), 1);
    assert_eq!(
        harness.upstream_headers().await["authorization"],
        "Bearer sk-openai-live"
    );
}

#[tokio::test]
async fn a_slow_provider_is_bounded_only_where_a_bound_cannot_truncate_an_answer() {
    let harness = Harness::with(|config| {
        config.proxy.upstream_read_timeout = Duration::from_millis(50);
    })
    .await;
    Mock::given(any())
        .respond_with(answered().set_delay(Duration::from_millis(400)))
        .mount(&harness.server)
        .await;

    let reply = harness
        .post("/v1/chat/completions", &chat_body("short"))
        .await;
    assert_eq!(reply.status, 502);
}

#[tokio::test]
async fn the_provider_wait_is_recorded_and_is_not_the_whole_request() {
    // `upstream_ms` shipped null on every row for the life of the column: it
    // was constructed as `None` and nothing ever assigned it. A Harbor run
    // therefore reported 337 s of wall clock with 3.3 s attributable to the
    // engine and 333.7 s attributable to nothing — which is the one number
    // that would have substantiated "the proxy adds no measurable latency".
    let harness = Harness::new().await;
    answering(&harness).await;

    harness
        .post("/v1/chat/completions", &chat_body(&log_payload(400)))
        .await;

    let waits: Vec<f64> = harness
        .events
        .drain()
        .into_iter()
        .filter_map(|event| match event {
            kompressor_stats::events::Event::ProxyRequest(e) => e.upstream_ms,
            _ => None,
        })
        .collect();

    assert_eq!(waits.len(), 1, "one request, one recorded wait");
    assert!(
        waits[0] > 0.0,
        "a request that reached the provider waited longer than zero"
    );
    // The bound that makes it a *measurement* rather than a copy of total_ms:
    // a local mock answers in single-digit milliseconds, so a value up near a
    // second would mean the timer is spanning something other than the send.
    assert!(
        waits[0] < 1000.0,
        "a local mock should answer in well under a second, got {}ms",
        waits[0]
    );
}

#[tokio::test]
async fn a_streamed_round_counts_each_input_token_once() {
    // Anthropic reports the full input side twice — once in `message_start`,
    // once again in `message_delta` — and both frames describe the one call.
    // The relay folded them with the same summing helper it uses *across*
    // retrieve rounds, where summing is right, and so doubled every input
    // counter for the life of the column.
    //
    // Measured on a Harbor run: the proxy filed 1,978,478 cache-read tokens
    // where the client reading the same responses saw 989,239 — a ratio of
    // exactly 2.0000 — while output tokens were correct, because
    // `output_tokens` is a placeholder in `message_start`. Halving the input
    // side reproduced the provider's bill to the cent.
    //
    // Driven through the relay rather than by calling the fold directly: a
    // test that folds two frames itself passes whichever helper the relay
    // actually reaches for, which is the bug this is about.
    let harness = Harness::new().await;
    let sse = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"usage":{"input_tokens":8,"cache_read_input_tokens":41116,"cache_creation_input_tokens":3600,"output_tokens":1}}}"#,
        "\n\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
        "\n\n",
        "event: message_delta\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":8,"cache_read_input_tokens":41116,"cache_creation_input_tokens":3600,"output_tokens":214}}"#,
        "\n\n",
        "event: message_stop\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(sse.as_bytes().to_vec(), "text/event-stream"),
        )
        .mount(&harness.server)
        .await;

    let mut body = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 64,
        "stream": true,
        "messages": [{"role": "user", "content": log_payload(400)}]
    });
    body["stream"] = json!(true);
    harness.post("/v1/messages", &body).await;

    let rows: Vec<_> = harness
        .events
        .drain()
        .into_iter()
        .filter_map(|event| match event {
            kompressor_stats::events::Event::ProxyRequest(e) => Some(e),
            _ => None,
        })
        .collect();
    assert_eq!(rows.len(), 1, "one request, one row");
    let row = &rows[0];
    assert_eq!(
        row.cache_read_tokens,
        Some(41116),
        "the two frames report one cache read, not two"
    );
    assert_eq!(row.prompt_tokens, Some(8), "one prompt, counted once");
    assert_eq!(
        row.cache_write_tokens,
        Some(3600),
        "one cache write, counted once"
    );
    assert_eq!(
        row.completion_tokens,
        Some(214),
        "and the output figure, not the placeholder `message_start` carries"
    );
}

#[tokio::test]
async fn a_provider_that_never_answers_still_records_the_wait() {
    // The failure took time and the row has to say so. Dropping the wait on
    // the error path would make every timeout look instantaneous, which is the
    // opposite of what someone reading these rows is looking for.
    let harness = Harness::with(|config| {
        // Nothing listens here.
        config.proxy.upstreams[0].base_url = "http://127.0.0.1:1".to_string();
    })
    .await;

    harness
        .post("/v1/chat/completions", &chat_body(&log_payload(400)))
        .await;

    let waits: Vec<Option<f64>> = harness
        .events
        .drain()
        .into_iter()
        .filter_map(|event| match event {
            kompressor_stats::events::Event::ProxyRequest(e) => Some(e.upstream_ms),
            _ => None,
        })
        .collect();

    assert_eq!(
        waits.len(),
        1,
        "an unreachable provider still records a row"
    );
    assert!(
        waits[0].is_some_and(|ms| ms > 0.0),
        "a refused connection still waited; got {:?}",
        waits[0]
    );
}

#[tokio::test]
async fn a_cold_prefix_is_compressed_even_where_cache_control_pinned_it() {
    // The whole point of `PrefixCache::Cold`, end to end. The enum, the
    // `effective_frozen` rule and its tests all shipped; nothing ever
    // constructed anything but `Warm`, so the cold path was unreachable.
    //
    // Turn one: the client pins a prefix with `cache_control` and the provider
    // reports it read nothing from cache. Turn two: the same prefix arrives,
    // and the freeze that was protecting a cache which does not exist is
    // lifted.
    //
    // Anthropic-shaped because `cache_control` is Anthropic's; an OpenAI Chat
    // body freezes nothing, so there is no prefix to have an opinion about.
    let harness = Harness::new().await;
    answering_warm_then_without_cache(&harness).await;

    let pinned = log_payload(400);
    let body = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_0",
                 "cache_control": {"type": "ephemeral"}, "content": pinned}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1",
                 "content": log_payload(400)}
            ]}
        ]
    });

    // Three turns, because a lapse takes two observations to establish. Turn
    // one is served from cache, so the prefix is known to be cacheable at all;
    // turn two is served nothing, which is the lapse; turn three acts on it.
    // A bare zero on turn one would mean "new", not "cold" — the provider
    // reads nothing because the entry does not exist yet.
    harness.post("/v1/messages", &body).await;
    harness.post("/v1/messages", &body).await;
    harness.post("/v1/messages", &body).await;

    let seen = harness.upstream_json().await;
    let sent = harness.upstream_bodies().await;
    assert_eq!(sent.len(), 3, "all three turns reached the provider");

    // Compared on the parsed field, not the serialized body: the payload has
    // newlines and the body escapes them, so a substring check against the raw
    // JSON fails on content that is in fact byte-identical.
    let pinned_first = seen[1]["messages"][0]["content"][0]["content"]
        .as_str()
        .expect("the pinned tool_result survives as text");
    assert_eq!(
        pinned_first, pinned,
        "the turns before the lapse must forward the pinned prefix untouched"
    );
    // Turn two is smaller. Both halves of `PrefixCache::Cold` are now in
    // place: the message freeze lifts via `effective_frozen`, and the slot
    // pinned by `cache_control` lifts because its protection says it is only
    // load-bearing `WhileCached`. Either half alone changes nothing here,
    // because on Anthropic the same marker does both jobs.
    //
    // Strictly smaller, not merely not-larger: `<=` is true when nothing
    // happens, which is how an earlier version of this test passed while the
    // cold path never ran.
    assert!(
        sent[2].len() < sent[1].len(),
        "the provider reported no cache read, so turn two should compress the \
         prefix it froze: {} then {}",
        sent[1].len(),
        sent[2].len()
    );
}

#[tokio::test]
async fn the_tail_the_provider_has_not_cached_yet_is_compressed() {
    // `FrozenPrefix::FirstSeen`, end to end.
    //
    // Claude Code marks `cache_control` on its FINAL message, so the
    // marker-derived floor is `max(i) + 1` — the whole conversation, including
    // what arrived this turn. Measured across a 20-turn Harbor run,
    // `frozen + opaque == messages_total` on every single turn: the engine was
    // handed nothing it was allowed to move, while 19,431 tokens of
    // conversation tail went into the cache untouched.
    //
    // Those tail bytes are in no cache entry when they arrive, so compressing
    // them invalidates nothing — and the prefix in front of them is left
    // exactly as it was, so the existing hit still lands.
    let harness = Harness::with(|config| {
        config.proxy.frozen_prefix = kompressor_core::config::FrozenPrefix::FirstSeen;
    })
    .await;
    answering(&harness).await;

    let opening = log_payload(400);
    let mut messages = vec![json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "toolu_0", "content": opening.clone()}
    ]})];
    // Turn one establishes the watermark: whatever the client had already
    // arranged to be cached before this proxy wrote anything.
    messages[0]["content"][0]["cache_control"] = json!({"type": "ephemeral"});
    let body = |messages: &Vec<Value>| json!({"model": "claude-sonnet-4-5", "max_tokens": 1024, "messages": messages});
    harness.post("/v1/messages", &body(&messages)).await;

    // Turn two: the breakpoint moves to the new final message, as an agent
    // advancing its cache does. Everything between is the tail.
    messages[0]["content"][0]
        .as_object_mut()
        .expect("a tool_result block")
        .remove("cache_control");
    let tail = log_payload(400);
    messages.push(json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "toolu_1", "content": tail.clone()},
    ]}));
    messages.push(json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "toolu_2",
         "cache_control": {"type": "ephemeral"}, "content": "the newest turn"}
    ]}));
    harness.post("/v1/messages", &body(&messages)).await;

    let seen = harness.upstream_json().await;
    assert_eq!(seen.len(), 2, "both turns reached the provider");

    // The half that protects the bill: message zero was cached before this
    // proxy touched anything, and must go out byte-identical or the provider
    // re-reads everything after it at full price.
    let cached = seen[1]["messages"][0]["content"][0]["content"]
        .as_str()
        .expect("the cached prefix survives as text");
    assert_eq!(
        cached, opening,
        "the prefix the provider already holds must not move"
    );

    // The half that earns it.
    let sent_tail = seen[1]["messages"][1]["content"][0]["content"]
        .as_str()
        .expect("the tail survives as text");
    assert!(
        sent_tail.len() < tail.len(),
        "the tail is in no cache entry yet, so it should have been compressed: \
         {} of {}",
        sent_tail.len(),
        tail.len()
    );
}

#[tokio::test]
async fn the_turn_that_mints_the_first_handle_leaves_the_tools_block_alone() {
    // The regression test for a cache break this proxy used to cause, kept in
    // the shape that first measured it. Everything below describes what went
    // wrong; the assertion at the end now holds it fixed.
    //
    // The cache break measured on both `first_seen` runs, isolated.
    //
    // Run A: turn five compressed a message with no handle and the cache held;
    // turn six minted the first handle and turn five's 4,324 written tokens
    // were gone. Run B: turn four minted the first handle and `cache_read`
    // went 16,101 -> **0** — the whole prefix, lost in one turn, accounting
    // for every one of that run's 19,705 excess writes.
    //
    // `retrieve_tool = "auto"` injects the tool only once the body carries a
    // handle. `tools` sits in front of `messages`, so on Anthropic — where the
    // client's breakpoints are on messages — adding an entry there moves bytes
    // that precede every cached segment.
    //
    // Built on the fixture that is known to compress, because an earlier
    // attempt at this test minted nothing, recorded no optimization events at
    // all, and therefore demonstrated nothing.
    let harness = Harness::with(|config| {
        config.proxy.frozen_prefix = kompressor_core::config::FrozenPrefix::FirstSeen;
    })
    .await;
    answering(&harness).await;

    let body = |messages: &Vec<Value>| {
        json!({
            "model": "claude-sonnet-4-5", "max_tokens": 1024,
            "tools": [{"name": "read_file", "description": "Read a file",
                       "input_schema": {"type": "object"}}],
            "messages": messages
        })
    };
    let mut messages = vec![json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "toolu_0",
         "cache_control": {"type": "ephemeral"}, "content": log_payload(400)}
    ]})];
    harness.post("/v1/messages", &body(&messages)).await;

    messages[0]["content"][0]
        .as_object_mut()
        .expect("a tool_result block")
        .remove("cache_control");
    messages.push(json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "toolu_1", "content": log_payload(400)},
    ]}));
    messages.push(json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "toolu_2",
         "cache_control": {"type": "ephemeral"}, "content": "the newest turn"}
    ]}));
    harness.post("/v1/messages", &body(&messages)).await;

    let minted: Vec<u32> = harness
        .events
        .drain()
        .into_iter()
        .filter_map(|event| match event {
            kompressor_stats::events::Event::Optimization(o) => Some(o.refs_emitted),
            _ => None,
        })
        .collect();
    assert!(
        minted.iter().any(|refs| *refs > 0),
        "the fixture has to actually mint a handle or this tests nothing: {minted:?}"
    );

    let seen = harness.upstream_json().await;
    let names = |turn: usize| {
        seen[turn]["tools"]
            .as_array()
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|tool| tool["name"].as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    // Was `assert_ne!`, recording the break as it behaved; the comment said
    // "if this ever comes back equal, the injection stopped perturbing the
    // prefix and the break described above is gone". It has, and it is.
    assert_eq!(
        names(0),
        names(1),
        "the tools array the provider cached on turn one must be the one it is \
         sent on the turn a handle first appears"
    );
}

#[tokio::test]
async fn a_turn_that_compresses_nothing_still_carries_the_same_tools() {
    // `tools` precedes `messages`, so an entry that comes and goes moves bytes
    // ahead of every cached message. Injection ran only at the end of
    // `prepare_inner`, past seven `return Prepared::passthrough` sites, so a
    // turn that declined or was discarded sent the client's own bytes with no
    // tool in them.
    //
    // Measured with `retrieve_tool = "always"`: the tool first appeared on
    // turn seven, because turn three was `no_compressible_content` and turns
    // four to six were discards. The array flipped mid-conversation, which is
    // the one thing this setting exists to prevent.
    //
    // Turn one is far below `min_tokens` and declines; turn two compresses.
    // Both must present the same tools.
    let harness = Harness::with(|config| {
        config.proxy.retrieve_tool = kompressor_core::config::RetrieveTool::Always;
        config.proxy.frozen_prefix = kompressor_core::config::FrozenPrefix::FirstSeen;
    })
    .await;
    answering(&harness).await;

    let body = |content: &str| {
        json!({
            "model": "claude-sonnet-4-5", "max_tokens": 1024,
            "tools": [{"name": "read_file", "description": "Read a file",
                       "input_schema": {"type": "object"}}],
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_0", "content": content}
            ]}]
        })
    };
    harness.post("/v1/messages", &body("tiny")).await;
    harness.post("/v1/messages", &body(&log_payload(400))).await;

    let seen = harness.upstream_json().await;
    assert_eq!(seen.len(), 2, "both turns reached the provider");
    let tools = |turn: usize| {
        seen[turn]["tools"]
            .as_array()
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|tool| tool["name"].as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    assert!(
        tools(0)
            .iter()
            .any(|name| name == "optscale_kompressor_retrieve"),
        "the declining turn must carry the tool too, or the array is unstable: {:?}",
        tools(0)
    );
    assert_eq!(
        tools(0),
        tools(1),
        "a turn that compressed nothing and one that compressed must present \
         the same tools"
    );
}

#[tokio::test]
async fn two_conversations_behind_one_preamble_file_different_identities() {
    // The other half of the identity contract, and the one that was missing.
    // Stability without discrimination is a key that names the CLIENT: an
    // agent harness opens every task with the same preamble, so hashing the
    // first message alone put unrelated conversations in one bucket.
    // Measured on a four-task Harbor run — all 76 turns of four tasks filed
    // ONE fingerprint, and the same one a different run had filed two hours
    // earlier.
    //
    // It costs more than tidy accounting. Under `first_seen` a shared key is
    // a shared watermark, and the floor may fall but never rise: the second
    // task opening at 0 drags the first task's floor down with it, and the
    // first task's next turn rewrites a prefix the provider is holding.
    let harness = Harness::new().await;
    answering(&harness).await;

    let preamble = "you are a terminal agent. answer briefly.";
    for task in ["fix the failing test in parser.rs", "add a changelog entry"] {
        harness
            .post(
                "/v1/chat/completions",
                &json!({"model": "gpt-5.5", "messages": [
                    {"role": "system", "content": preamble},
                    {"role": "user", "content": format!("{task}\n{}", log_payload(40))},
                ]}),
            )
            .await;
    }

    let filed: Vec<Option<String>> = harness
        .events
        .drain()
        .into_iter()
        .filter_map(|event| match event {
            kompressor_stats::events::Event::ProxyRequest(e) => Some(e.conversation),
            _ => None,
        })
        .collect();
    assert_eq!(filed.len(), 2, "two conversations, two rows");
    assert!(
        filed.iter().all(|c| c.is_some()),
        "both must be identified: {filed:?}"
    );
    assert_ne!(
        filed[0], filed[1],
        "two tasks behind one preamble must not share a conversation: {filed:?}"
    );
}

#[tokio::test]
async fn every_turn_of_one_conversation_files_the_same_identity() {
    // What makes the savings headline priceable. `tokens_in`/`tokens_out` are
    // per-request sums, so a message compressed on turn ten is counted again
    // on every turn after it and the total over-counts by roughly how long a
    // message survives; grouping by this column is what turns that windowed
    // sum into unique bytes.
    //
    // The property that matters is STABILITY across turns, which is the same
    // one the prefix tracker needs: an agent advances its breakpoint every
    // turn, so a naive key over the whole frozen prefix moves with it. Three
    // turns, one identity.
    let harness = Harness::new().await;
    answering(&harness).await;

    let mut messages: Vec<Value> = Vec::new();
    for turn in 0..3 {
        for message in messages.iter_mut() {
            message["content"][0]
                .as_object_mut()
                .expect("a tool_result block")
                .remove("cache_control");
        }
        messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": format!("toolu_{turn}"),
             "cache_control": {"type": "ephemeral"},
             "content": format!("turn {turn} {}", log_payload(40))}
        ]}));
        harness
            .post(
                "/v1/messages",
                &json!({"model": "claude-sonnet-4-5", "max_tokens": 1024,
                        "messages": messages}),
            )
            .await;
    }

    let filed: Vec<Option<String>> = harness
        .events
        .drain()
        .into_iter()
        .filter_map(|event| match event {
            kompressor_stats::events::Event::ProxyRequest(e) => Some(e.conversation),
            _ => None,
        })
        .collect();
    assert_eq!(filed.len(), 3, "three turns, three rows");
    assert!(
        filed.iter().all(|c| c.is_some()),
        "a frozen prefix has an identity: {filed:?}"
    );
    assert_eq!(
        filed
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1,
        "one conversation must file one identity, not one per turn: {filed:?}"
    );
    let id = filed[0].clone().expect("an identity");
    assert_eq!(
        id.len(),
        16,
        "fixed-width hex groups and sorts cleanly: {id}"
    );
}

#[tokio::test]
async fn a_conversation_with_nothing_frozen_still_has_an_identity() {
    // The gap the first version shipped with. The identity was taken from the
    // *frozen* head, and OpenAI has no `cache_control` markers, so its floor is
    // always 0 and every row filed a null conversation — measured at 66 of 66
    // on a gpt-5.5 run, i.e. the accounting was missing on exactly the provider
    // where the engine removes 61% of what it sees.
    let harness = Harness::new().await;
    answering(&harness).await;

    // A conversation grows: message zero stays put and later turns append,
    // which is what makes the head a stable key. A first version of this test
    // sent a different single message each turn and was really asserting that
    // two unrelated conversations collide.
    let mut messages = vec![json!({"role": "user", "content": "the opening turn"})];
    for turn in 0..2 {
        messages.push(json!({
            "role": "user",
            "content": format!("turn {turn} {}", log_payload(200))
        }));
        harness
            .post(
                "/v1/chat/completions",
                &json!({"model": "gpt-4o", "messages": messages}),
            )
            .await;
    }

    let filed: Vec<Option<String>> = harness
        .events
        .drain()
        .into_iter()
        .filter_map(|event| match event {
            kompressor_stats::events::Event::ProxyRequest(e) => Some(e.conversation),
            _ => None,
        })
        .collect();
    assert_eq!(filed.len(), 2, "two turns, two rows");
    assert!(
        filed.iter().all(|c| c.is_some()),
        "a conversation with no frozen prefix is still a conversation: {filed:?}"
    );
    assert_eq!(
        filed
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1,
        "and both turns belong to the same one: {filed:?}"
    );
}

#[tokio::test]
async fn a_first_turn_that_creates_cache_must_not_thaw_the_next_one() {
    // The field bug. A provider answering the FIRST request of a conversation
    // reports `cache_read = 0` with a nonzero `cache_creation`: it read
    // nothing because nothing was cached yet, and it is busy writing the
    // entry. Reading that zero as a lapse inverted the feature — the next turn
    // ran with the freeze lifted and rewrote the prefix just written.
    //
    // Measured on a Claude Code subscription session: turn one
    // `cache_read=0` / `cache_creation=22,363`; turn three then ran with
    // `frozen_messages=0` and the provider served only 14,176 of the 22,363 it
    // held — 8,187 tokens re-read at full price, once per conversation. Turn
    // four returned to `frozen_messages=2` as soon as a nonzero read arrived,
    // which is what identified this as the per-turn cache state rather than
    // the monotonic watermark.
    //
    // Same two-message shape as the cold-prefix test above, deliberately: a
    // first version used one message, which never reached the lift at all and
    // passed with the guard removed.
    let harness = Harness::new().await;
    answering_by_creating_cache(&harness).await;

    let pinned = log_payload(400);
    let body = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_0",
                 "cache_control": {"type": "ephemeral"}, "content": pinned}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1",
                 "content": log_payload(400)}
            ]}
        ]
    });
    harness.post("/v1/messages", &body).await;
    harness.post("/v1/messages", &body).await;

    let seen = harness.upstream_json().await;
    assert_eq!(seen.len(), 2, "both turns reached the provider");
    let sent = |turn: usize| {
        seen[turn]["messages"][0]["content"][0]["content"]
            .as_str()
            .expect("the pinned prefix survives as text")
            .to_string()
    };
    assert_eq!(
        sent(0),
        pinned,
        "turn one forwards the pinned prefix untouched"
    );
    assert_eq!(
        sent(1),
        pinned,
        "turn one CREATED that cache entry, so turn two must not rewrite it — \
         zero reads on a first turn mean new, not lapsed"
    );
}

/// A provider that always serves this prefix from cache.
async fn answering_from_cache(harness: &Harness) {
    use wiremock::matchers::method;
    use wiremock::{Mock, ResponseTemplate};
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "usage": {"input_tokens": 2, "output_tokens": 2,
                      "cache_read_input_tokens": 20733,
                      "cache_creation_input_tokens": 2870}
        })))
        .mount(&harness.server)
        .await;
}

#[tokio::test]
async fn a_discard_does_not_cache_raw_bytes_a_later_turn_will_rewrite() {
    // The second transition. Discarding forwards the client's ORIGINAL bytes,
    // so the provider caches those; a later turn that compresses the same
    // messages sends different bytes and the prefix misses from there on.
    //
    // Measured on a 39-request Opus run: turns four and five discarded, turn
    // six compressed, and the cache fell back to turn four's prefix — losing
    // exactly `cw[4] + cw[5]` = 8,299 tokens, 11.1% of everything that run
    // wrote. The flip is raw-to-compressed, so suppressing the reverse would
    // not have helped.
    //
    // The guard stays in force while nothing is cached, which is why this test
    // establishes a cached prefix first.
    let harness = Harness::with(|config| {
        config.proxy.frozen_prefix = kompressor_core::config::FrozenPrefix::FirstSeen;
        // The shape the field runs discarded on: a small request whose saving
        // is outweighed by the injected retrieve tool, so the serialized body
        // comes out larger than it went in.
        config.proxy.retrieve_tool = kompressor_core::config::RetrieveTool::Always;
        config.proxy.min_tokens = 16;
        config.engine.min_compress_chars = 64;
    })
    .await;
    answering_from_cache(&harness).await;

    let mut messages = vec![json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "toolu_0",
         "cache_control": {"type": "ephemeral"}, "content": "the opening turn"}
    ]})];
    let body = |messages: &Vec<Value>| {
        json!({
            "model": "claude-sonnet-4-5", "max_tokens": 1024,
            // A `tools` array for the retrieve tool to be appended to: with no
            // tools field there is nothing to inject into, the body never
            // grows, and the guard this test is about never fires.
            "tools": [{"name": "read_file", "description": "Read a file",
                       "input_schema": {"type": "object"}}],
            "messages": messages
        })
    };
    // Turn one teaches the tracker the prefix is cached.
    harness.post("/v1/messages", &body(&messages)).await;

    // Then a compressible message arrives and stays for the rest of the
    // conversation. Whatever the proxy sends for it on the turn it arrives, it
    // must keep sending.
    // Just big enough to be looked at, small enough that what it saves is
    // outweighed by the injected retrieve tool -- so the serialized body comes
    // out LARGER and the guard discards the whole request.
    let payload = log_payload(22);
    messages.push(json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "toolu_1", "content": payload}
    ]}));
    harness.post("/v1/messages", &body(&messages)).await;
    messages.push(json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "toolu_2", "content": log_payload(200)}
    ]}));
    harness.post("/v1/messages", &body(&messages)).await;

    let seen = harness.upstream_json().await;
    assert_eq!(seen.len(), 3, "three turns reached the provider");
    let sent = |turn: usize| {
        seen[turn]["messages"][1]["content"][0]["content"]
            .as_str()
            .expect("the message survives as text")
            .to_string()
    };
    assert_eq!(
        sent(1),
        sent(2),
        "the bytes sent for a message on the turn it arrives are the bytes the \
         provider cached; sending different ones later misses the prefix"
    );
}

#[tokio::test]
async fn the_marker_rule_still_freezes_the_whole_conversation() {
    // The control, and the shipped default. Same traffic, same assertions,
    // `FrozenPrefix::Marker` — the tail must come back untouched, or the test
    // above is measuring something other than the setting it names.
    let harness = Harness::new().await;
    answering(&harness).await;

    let opening = log_payload(400);
    let mut messages = vec![json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "toolu_0",
         "cache_control": {"type": "ephemeral"}, "content": opening.clone()}
    ]})];
    let body = |messages: &Vec<Value>| json!({"model": "claude-sonnet-4-5", "max_tokens": 1024, "messages": messages});
    harness.post("/v1/messages", &body(&messages)).await;

    messages[0]["content"][0]
        .as_object_mut()
        .expect("a tool_result block")
        .remove("cache_control");
    let tail = log_payload(400);
    messages.push(json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "toolu_1", "content": tail.clone()},
    ]}));
    messages.push(json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "toolu_2",
         "cache_control": {"type": "ephemeral"}, "content": "the newest turn"}
    ]}));
    harness.post("/v1/messages", &body(&messages)).await;

    let seen = harness.upstream_json().await;
    let sent_tail = seen[1]["messages"][1]["content"][0]["content"]
        .as_str()
        .expect("the tail survives as text");
    assert_eq!(
        sent_tail, tail,
        "under the marker rule the newest breakpoint freezes the tail too"
    );
}

/// A provider that answers with usage reporting **zero** cache reads.
/// A provider that serves this prefix from cache once, then stops.
///
/// Warm THEN zero, because a bare zero is not a lapse. The first request of any
/// conversation reads nothing — the entry does not exist yet — so treating that
/// as evidence of a cold prefix thaws a cache the previous turn had just
/// created. A lapse is a prefix that *was* served and now is not, and only that
/// sequence produces `PrefixCache::Cold`.
async fn answering_warm_then_without_cache(harness: &Harness) {
    use wiremock::matchers::method;
    use wiremock::{Mock, ResponseTemplate};
    let usage = |read: u32| {
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "usage": {"input_tokens": 1000, "output_tokens": 2,
                      "cache_read_input_tokens": read,
                      "cache_creation_input_tokens": 0}
        }))
    };
    // Mounted first and capped, so it answers the opening turn only.
    Mock::given(method("POST"))
        .respond_with(usage(41_116))
        .up_to_n_times(1)
        .mount(&harness.server)
        .await;
    Mock::given(method("POST"))
        .respond_with(usage(0))
        .mount(&harness.server)
        .await;
}

/// A provider that never serves from cache, and says so by *creating* one.
///
/// The first turn of every conversation looks like this, which is why zero
/// reads alone must not be read as a lapse.
async fn answering_by_creating_cache(harness: &Harness) {
    use wiremock::matchers::method;
    use wiremock::{Mock, ResponseTemplate};
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "usage": {"input_tokens": 1000, "output_tokens": 2,
                      "cache_read_input_tokens": 0,
                      "cache_creation_input_tokens": 22363}
        })))
        .mount(&harness.server)
        .await;
}

#[tokio::test]
async fn a_growing_conversation_keeps_one_cache_identity() {
    // The fix, end to end. An agent advances its cache breakpoint as the
    // conversation extends, so the *frozen prefix* grows every turn — measured
    // on a Claude Code run, `frozen_messages` went 0, 3, 4, 5, 7 … 26 across
    // 24 requests. The first fingerprint hashed that whole prefix, so the key
    // moved with it and evidence filed on one turn was looked up under a key
    // the next turn never asked about: 23 keys, one conversation, never a
    // match.
    //
    // The breakpoint has to *move* for this to test anything. A first version
    // pinned only message zero, which froze exactly one message every turn and
    // therefore passed with the old key too.
    let harness = Harness::new().await;
    answering_warm_then_without_cache(&harness).await;

    let mut messages: Vec<Value> = Vec::new();
    for turn in 0..3 {
        // Everything so far loses the breakpoint; the newest turn carries it.
        for message in messages.iter_mut() {
            message["content"][0]
                .as_object_mut()
                .expect("a tool_result block")
                .remove("cache_control");
        }
        messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": format!("toolu_{turn}"),
             "cache_control": {"type": "ephemeral"},
             "content": log_payload(200)}
        ]}));
        harness
            .post(
                "/v1/messages",
                &json!({"model": "claude-sonnet-4-5", "max_tokens": 1024,
                        "messages": messages}),
            )
            .await;
    }
    assert_eq!(harness.upstream_bodies().await.len(), 3);

    // One conversation, one entry. With the whole-prefix key this is three.
    assert_eq!(
        harness.state.prefixes().tracked(),
        1,
        "a growing conversation must keep one cache identity"
    );
}

/// A turn that asks about elided content gets the content, not the marker.
///
/// This is the recovery path for transports with no retrieve loop. Over HTTP a
/// model that meets a marker it needs calls `optscale_kompressor_retrieve`; over a
/// relayed websocket the tool is deliberately not injected, so a marker the
/// model cannot resolve is content it has simply lost. Deciding on the way in
/// that a fragment is wanted is the only way back.
#[tokio::test]
async fn a_turn_that_asks_about_elided_content_is_sent_the_content() {
    let harness = Harness::new().await;
    Mock::given(any())
        .respond_with(answered())
        .mount(&harness.server)
        .await;

    // Turn one: the log is elided behind a handle.
    let payload = log_payload(400);
    let body = chat_body(&payload);
    assert_eq!(
        harness.post("/v1/chat/completions", &body).await.status,
        200
    );
    let first = harness.upstream_json().await;
    let out = first[0].to_string();
    let handle = first_handle(&out).expect("turn one elided the log");
    let lines = |body: &str| body.matches("heartbeat seq=").count();
    let elided = lines(&out);
    assert!(
        elided < 100,
        "turn one must really have elided it ({elided} of 400)"
    );

    // Turn two: the same conversation, and a question made of words that are in
    // the fragment. Nothing was ever retrieved — there is no retrieve loop on
    // the transport this exists for.
    // The model could not fetch it, so it said so and named the handle. That
    // reply is in the client's history, which is how the marker comes back —
    // the client's own tool output is stored uncompressed, so a marker in a
    // request means the *model* wrote it.
    let mut asking = body.clone();
    let turns = asking["messages"].as_array_mut().unwrap();
    turns.push(json!({
        "role": "assistant",
        "content": format!("Part of that log was omitted (ref:{handle}); I could not read it.")
    }));
    turns.push(json!({
        "role": "user",
        "content": "which worker logged the heartbeat, and what was the latency?"
    }));
    assert_eq!(
        harness.post("/v1/chat/completions", &asking).await.status,
        200
    );

    let rounds = harness.upstream_json().await;
    let second = rounds[first.len()].to_string();
    // The handle still appears once: the assistant's own reply names it, and
    // that message is never compressed. What must be gone is the *marker* the
    // compressor would have minted, so the count is the assertion, not absence.
    assert_eq!(
        second.matches(&handle).count(),
        1,
        "only the assistant's echo should name this handle, not a fresh marker"
    );
    assert!(
        lines(&second) > elided * 2,
        "the log ships whole instead (turn one carried {} lines, turn two {})",
        elided,
        lines(&second)
    );
}

/// The control. A turn about something else leaves the compression alone.
///
/// Without it the test above would pass for a proxy that simply stopped
/// eliding once a conversation had two turns.
#[tokio::test]
async fn a_turn_about_something_else_leaves_the_elision_alone() {
    let harness = Harness::new().await;
    Mock::given(any())
        .respond_with(answered())
        .mount(&harness.server)
        .await;

    let payload = log_payload(400);
    let body = chat_body(&payload);
    assert_eq!(
        harness.post("/v1/chat/completions", &body).await.status,
        200
    );
    let first = harness.upstream_json().await;

    let handle = first_handle(&first[0].to_string()).expect("turn one elided the log");
    let mut asking = body.clone();
    let turns = asking["messages"].as_array_mut().unwrap();
    // Same shape as the test above — the marker is present — so the only thing
    // that differs is whether the question is about it.
    turns.push(json!({
        "role": "assistant",
        "content": format!("Part of that log was omitted (ref:{handle}).")
    }));
    turns.push(json!({
        "role": "user",
        "content": "unrelated question about Glasgow weather forecasts"
    }));
    assert_eq!(
        harness.post("/v1/chat/completions", &asking).await.status,
        200
    );

    let rounds = harness.upstream_json().await;
    let second = rounds[first.len()].to_string();
    assert_eq!(
        second.matches(&handle).count(),
        2,
        "the assistant's echo plus a fresh marker: this turn is not about the fragment"
    );
    assert!(
        second.matches("heartbeat seq=").count() < 100,
        "and so does not ship the log whole"
    );
}
