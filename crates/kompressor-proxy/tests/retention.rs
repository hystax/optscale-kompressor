//! `proxy.prompt_cache_retention`: what OpenAI is asked to keep, and what
//! happens when it refuses.

mod support;

use serde_json::{Value, json};
use wiremock::matchers::{any, body_string_contains};
use wiremock::{Mock, ResponseTemplate};

use kompressor_core::config::{Config, PromptCacheRetention, ProviderOverrides, RetrieveTool};
use support::{Harness, chat_body, responses_body};

const FIELD: &str = "prompt_cache_retention";

fn answered() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "chatcmpl-1",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "done"},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 900, "completion_tokens": 12}
    }))
}

fn refused() -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({
        "error": {"message": "Unknown parameter: 'prompt_cache_retention'.",
                  "type": "invalid_request_error", "param": FIELD}
    }))
}

/// The OpenAI upstream, with the setting on, under its own provider override.
/// The retrieve tool is off so that the field is the only thing added.
fn extended(config: &mut Config) {
    config.proxy.upstreams[0].name = "openai".to_string();
    config.proxy.retrieve_tool = RetrieveTool::Never;
    config.proxy.providers.insert(
        "openai".to_string(),
        ProviderOverrides {
            prompt_cache_retention: Some(PromptCacheRetention::Extended),
            ..Default::default()
        },
    );
}

fn small(model: &str) -> Value {
    let mut body = responses_body("short");
    body["model"] = json!(model);
    body
}

async fn answering(harness: &Harness) {
    Mock::given(any())
        .respond_with(answered())
        .mount(&harness.server)
        .await;
}

#[tokio::test]
async fn a_supported_model_on_openai_is_asked_to_keep_its_prefix_a_day() {
    let harness = Harness::with(extended).await;
    answering(&harness).await;
    for (path, mut body) in [
        ("/v1/responses", small("gpt-5.5")),
        ("/v1/chat/completions", chat_body("short")),
    ] {
        body["model"] = json!("gpt-5.5");
        let reply = harness.post(path, &body).await;
        assert_eq!(reply.status, 200);
    }
    let sent = harness.upstream_json().await;
    assert_eq!(sent.len(), 2);
    for request in &sent {
        assert_eq!(request[FIELD], "24h");
    }
    // Nothing else moved: the prompt is what the cache is keyed on.
    let mut without = sent[0].clone();
    without.as_object_mut().unwrap().remove(FIELD);
    assert_eq!(without, small("gpt-5.5"));
}

#[tokio::test]
async fn nothing_is_added_unless_every_condition_holds() {
    // Off by default.
    let harness = Harness::with(|config| {
        config.proxy.upstreams[0].name = "openai".to_string();
    })
    .await;
    answering(&harness).await;
    harness.post("/v1/responses", &small("gpt-5.5")).await;
    assert!(harness.upstream_json().await[0].get(FIELD).is_none());

    // On, but a model that takes another field, or is not documented.
    let harness = Harness::with(extended).await;
    answering(&harness).await;
    for model in ["gpt-5.6", "gpt-5-mini", "glm-5.3"] {
        harness.post("/v1/responses", &small(model)).await;
    }
    for request in harness.upstream_json().await {
        assert!(request.get(FIELD).is_none(), "{}", request["model"]);
    }

    // On, but an upstream that is not OpenAI's API.
    let harness = Harness::with(|config| {
        extended(config);
        config.proxy.upstreams[0].name = "gateway".to_string();
    })
    .await;
    answering(&harness).await;
    harness.post("/v1/responses", &small("gpt-5.5")).await;
    assert!(harness.upstream_json().await[0].get(FIELD).is_none());

    // On, but a ChatGPT-subscription caller: Codex with no API key.
    let harness = Harness::with(extended).await;
    answering(&harness).await;
    harness
        .post_with(
            "/v1/responses",
            &small("gpt-5.5"),
            &[("user-agent", "codex_exec/0.130.0")],
        )
        .await;
    assert!(harness.upstream_json().await[0].get(FIELD).is_none());
}

#[tokio::test]
async fn a_retention_the_client_named_goes_out_as_it_named_it() {
    let harness = Harness::with(extended).await;
    answering(&harness).await;
    let mut body = small("gpt-5.5");
    body[FIELD] = json!("24h");
    harness.post("/v1/responses", &body).await;
    let mut chat = chat_body("short");
    chat["model"] = json!("gpt-5.4");
    chat[FIELD] = json!("in_memory");
    harness.post("/v1/chat/completions", &chat).await;
    let sent = harness.upstream_bodies().await;
    // Untouched, byte for byte: the proxy had nothing to add.
    assert_eq!(sent[0], serde_json::to_string(&body).unwrap());
    assert_eq!(sent[1], serde_json::to_string(&chat).unwrap());
}

#[tokio::test]
async fn a_refusal_costs_a_round_trip_not_the_request_and_is_remembered() {
    let harness = Harness::with(extended).await;
    Mock::given(body_string_contains(FIELD))
        .respond_with(refused())
        .mount(&harness.server)
        .await;
    answering(&harness).await;

    let mut streamed = small("gpt-5.5");
    streamed["stream"] = json!(true);
    for body in [small("gpt-5.5"), streamed, small("gpt-5.5")] {
        let reply = harness.post("/v1/responses", &body).await;
        assert_eq!(reply.status, 200, "{}", reply.text());
    }
    let sent = harness.upstream_json().await;
    let carried: Vec<bool> = sent.iter().map(|r| r.get(FIELD).is_some()).collect();
    // Refused once, sent again without; never asked again for that model.
    assert_eq!(carried, [true, false, false, false]);

    // Another model on the same upstream is still asked.
    harness.post("/v1/responses", &small("gpt-5.4")).await;
    let sent = harness.upstream_json().await;
    assert_eq!(sent.len(), 6);
    assert_eq!(sent[4][FIELD], "24h");
    assert!(sent[5].get(FIELD).is_none());
}

#[tokio::test]
async fn any_other_bad_request_reaches_the_client_as_the_provider_sent_it() {
    let harness = Harness::with(extended).await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"message": "max_output_tokens is below the minimum of 16."}
        })))
        .mount(&harness.server)
        .await;
    let reply = harness.post("/v1/responses", &small("gpt-5.5")).await;
    assert_eq!(reply.status, 400);
    assert!(
        reply
            .text()
            .contains("max_output_tokens is below the minimum")
    );
    // One request: nothing was retried, and the field stays on.
    assert_eq!(harness.upstream_json().await.len(), 1);
    harness.post("/v1/responses", &small("gpt-5.5")).await;
    assert_eq!(harness.upstream_json().await[1][FIELD], "24h");
}

#[tokio::test]
async fn a_compressed_request_body_keeps_its_coding_with_the_field_added() {
    let harness = Harness::with(extended).await;
    answering(&harness).await;
    let reply = harness.post_zstd("/v1/responses", &small("gpt-5.5")).await;
    assert_eq!(reply.status, 200);
    let raw = &harness.upstream_raw_bodies().await[0];
    let plain = zstd::decode_all(raw.as_slice()).expect("still zstd");
    let sent: Value = serde_json::from_slice(&plain).unwrap();
    assert_eq!(sent[FIELD], "24h");
}

#[tokio::test]
async fn the_deployment_wide_setting_applies_when_no_provider_overrides_it() {
    let harness = Harness::with(|config| {
        config.proxy.upstreams[0].name = "openai".to_string();
        config.apply_env(&[(
            "KOMPRESSOR__PROXY__PROMPT_CACHE_RETENTION".into(),
            "24h".into(),
        )]);
    })
    .await;
    answering(&harness).await;
    harness.post("/v1/responses", &small("gpt-5.5")).await;
    assert_eq!(harness.upstream_json().await[0][FIELD], "24h");
}
