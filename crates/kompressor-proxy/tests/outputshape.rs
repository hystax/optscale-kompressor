//! `proxy.output_shaping`, end to end: what goes upstream for each arm, that a
//! treated conversation is sent the same block on every turn without moving a
//! byte the provider cached, and that the arm is on the statistics row.

mod support;

use serde_json::{Value, json};
use support::{Harness, proxy_rows};
use wiremock::matchers::any;
use wiremock::{Mock, ResponseTemplate};

use kompressor_core::config::{Config, RetrieveTool};

const MESSAGES: &str = "/v1/messages";
const TEXT: &str = "Answer in as few words as the work allows.";

async fn answering(harness: &Harness) {
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn",
            "usage": {"input_tokens": 12, "output_tokens": 7,
                      "cache_read_input_tokens": 900, "cache_creation_input_tokens": 40}
        })))
        .mount(&harness.server)
        .await;
}

fn system() -> Value {
    json!([
        {"type": "text", "text": "You are a coding agent."},
        {"type": "text", "text": "Follow the repository's rules.",
         "cache_control": {"type": "ephemeral"}}
    ])
}

/// Conversation `task` after `turns` exchanges, with Claude Code's breakpoint
/// on the last message.
fn conversation(task: usize, turns: usize) -> Value {
    let user = |text: String| json!({"role": "user", "content": [{"type": "text", "text": text}]});
    let mut messages = vec![user(format!("Task {task}: fix the bug."))];
    for turn in 0..turns {
        messages
            .push(json!({"role": "assistant", "content": format!("step {turn} of task {task}")}));
        messages.push(user(format!("go on with {turn}")));
    }
    messages.last_mut().unwrap()["content"][0]["cache_control"] = json!({"type": "ephemeral"});
    json!({"model": "claude-opus-5-5", "max_tokens": 4096,
           "thinking": {"type": "enabled", "budget_tokens": 2048},
           "system": system(),
           "tools": [{"name": "read_file", "description": "Read a file.",
                      "input_schema": {"type": "object",
                                       "properties": {"path": {"type": "string"}}}}],
           "messages": messages})
}

fn shaping(config: &mut Config) {
    config.proxy.retrieve_tool = RetrieveTool::Never;
    config.proxy.output_shaping = true;
    config.proxy.output_shaping_holdout_pct = 30;
    config.proxy.output_shaping_text = TEXT.to_string();
}

fn arm(row: &kompressor_stats::events::ProxyRequestEvent) -> Option<&str> {
    row.output_shaping.as_deref()
}

/// Longest common prefix of two byte strings.
fn shared(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// Open twenty conversations and sort them by the arm the proxy put them in.
async fn opened(harness: &Harness) -> (Vec<usize>, Vec<usize>) {
    for task in 0..20 {
        harness.post(MESSAGES, &conversation(task, 0)).await;
    }
    let rows = proxy_rows(&harness.events);
    assert_eq!(rows.len(), 20);
    let mut treated = Vec::new();
    let mut held = Vec::new();
    for (task, row) in rows.iter().enumerate() {
        match arm(row) {
            Some("treated") => treated.push(task),
            Some("holdout") => held.push(task),
            other => panic!("task {task} opened outside the experiment: {other:?}"),
        }
    }
    assert!(
        !treated.is_empty() && !held.is_empty(),
        "{treated:?} {held:?}"
    );
    (treated, held)
}

#[tokio::test]
async fn off_by_default_the_request_goes_upstream_as_it_arrived() {
    let harness = Harness::with(|config| config.proxy.retrieve_tool = RetrieveTool::Never).await;
    answering(&harness).await;
    let bodies = [conversation(1, 0), conversation(1, 1), conversation(1, 2)];
    for body in &bodies {
        harness.post(MESSAGES, body).await;
    }
    for (sent, body) in harness.upstream_bodies().await.iter().zip(&bodies) {
        assert_eq!(*sent, body.to_string(), "the bytes that arrived");
    }
    assert!(
        proxy_rows(&harness.events)
            .iter()
            .all(|row| arm(row).is_none())
    );
}

#[tokio::test]
async fn a_treated_conversation_gets_the_same_block_every_turn_and_no_cached_byte_moves() {
    let harness = Harness::with(shaping).await;
    answering(&harness).await;
    let (treated, _) = opened(&harness).await;
    let task = treated[0];
    for turns in 1..4 {
        harness.post(MESSAGES, &conversation(task, turns)).await;
    }
    let rows = proxy_rows(&harness.events);
    assert!(rows.iter().all(|row| arm(row) == Some("treated")));

    let raw = harness.upstream_bodies().await;
    let sent: Vec<Value> = harness.upstream_json().await;
    // the opening request, then the three later turns
    let mut turns: Vec<(&String, &Value)> = vec![(&raw[task], &sent[task])];
    turns.extend(raw[20..].iter().zip(&sent[20..]));
    assert_eq!(turns.len(), 4);

    let mut expected_system = system();
    expected_system
        .as_array_mut()
        .unwrap()
        .push(json!({"type": "text", "text": TEXT}));
    for (n, (_, body)) in turns.iter().enumerate() {
        let client = conversation(task, n);
        assert_eq!(body["system"], expected_system, "turn {n}");
        assert_eq!(body["messages"], client["messages"], "turn {n}");
        assert_eq!(body["tools"], client["tools"], "turn {n}");
        assert_eq!(body["max_tokens"], client["max_tokens"], "left alone");
        assert_eq!(body["thinking"], client["thinking"], "left alone");
    }
    // The system blocks up to and including the breakpoint — the entry every
    // conversation shares — are the client's, byte for byte.
    let client_system = serde_json::to_string(&system()).unwrap();
    for (raw, _) in &turns {
        let marked = client_system.trim_end_matches(']');
        assert!(raw.contains(marked), "{raw}");
    }
    // Prefix-diff: each turn's upstream bytes are the previous turn's, then
    // only what the turn appended. Messages are the body's last key, so the
    // previous body matches in everything but its closing `]}`.
    for pair in turns.windows(2) {
        let (before, after) = (pair[0].0.as_bytes(), pair[1].0.as_bytes());
        // the previous turn's last message carried the breakpoint; this turn
        // re-sends it without one, which is the client's own change
        let breakpoint = pair[0]
            .0
            .rfind(",\"cache_control\"")
            .expect("the last message carries one");
        assert!(
            shared(before, after) >= breakpoint,
            "the bodies diverge at byte {} of {}, before the client's own \
             breakpoint at {breakpoint}",
            shared(before, after),
            before.len()
        );
    }
    let text_count = raw.iter().filter(|body| body.contains(TEXT)).count();
    assert_eq!(
        text_count,
        treated.len() + 3,
        "only treated requests carry it"
    );
}

#[tokio::test]
async fn a_holdout_conversation_goes_upstream_as_it_arrived() {
    let harness = Harness::with(shaping).await;
    answering(&harness).await;
    let (_, held) = opened(&harness).await;
    let task = held[0];
    let bodies = [conversation(task, 1), conversation(task, 2)];
    for body in &bodies {
        harness.post(MESSAGES, body).await;
    }
    let raw = harness.upstream_bodies().await;
    assert_eq!(raw[task], conversation(task, 0).to_string());
    for (sent, body) in raw[20..].iter().zip(&bodies) {
        assert_eq!(*sent, body.to_string(), "the bytes that arrived");
    }
    assert!(
        proxy_rows(&harness.events)
            .iter()
            .all(|row| arm(row) == Some("holdout"))
    );
}

/// The setting turned on while a conversation is under way — a restart with
/// the new configuration — finds it already several messages long. The
/// provider caches its prefix without the block, so it is left untreated for
/// the rest of its life, and filed outside the experiment.
#[tokio::test]
async fn a_conversation_under_way_when_the_setting_came_on_stays_untreated() {
    let harness = Harness::with(shaping).await;
    answering(&harness).await;
    let bodies: Vec<Value> = (2..5).map(|turns| conversation(3, turns)).collect();
    for body in &bodies {
        harness.post(MESSAGES, body).await;
    }
    for (sent, body) in harness.upstream_bodies().await.iter().zip(&bodies) {
        assert_eq!(*sent, body.to_string(), "the bytes that arrived");
    }
    assert!(
        proxy_rows(&harness.events)
            .iter()
            .all(|row| arm(row).is_none())
    );
}

/// A client that sends `system` as a string keeps sending a string, extended.
#[tokio::test]
async fn a_string_system_prompt_is_extended_in_place() {
    let harness = Harness::with(|config| {
        shaping(config);
        // nobody held out, so the one conversation is treated
        config.proxy.output_shaping_holdout_pct = 0;
    })
    .await;
    answering(&harness).await;
    for turns in 0..2 {
        let mut body = conversation(5, turns);
        body["system"] = json!("You are a coding agent.");
        harness.post(MESSAGES, &body).await;
    }
    let sent = harness.upstream_json().await;
    for body in &sent {
        assert_eq!(
            body["system"],
            json!(format!("You are a coding agent.\n\n{TEXT}"))
        );
    }
}

/// OpenAI families: the same experiment, the instruction where each family
/// puts the text ahead of the conversation.
mod openai {
    use super::*;

    async fn answering_openai(harness: &Harness) {
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "x", "object": "chat.completion",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                             "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}
            })))
            .mount(&harness.server)
            .await;
    }

    fn chat(task: usize, turns: usize) -> Value {
        let mut messages = vec![
            json!({"role": "system", "content": "You are a coding agent."}),
            json!({"role": "user", "content": format!("Task {task}: fix the bug.")}),
        ];
        for turn in 0..turns {
            messages.push(json!({"role": "assistant", "content": format!("step {turn}")}));
            messages.push(json!({"role": "user", "content": format!("go on {turn}")}));
        }
        json!({"model": "gpt-5.5", "messages": messages})
    }

    fn responses(task: usize, turns: usize) -> Value {
        let mut input = vec![json!({"role": "user", "content": format!("Task {task}: fix it.")})];
        for turn in 0..turns {
            input.push(json!({"role": "assistant", "content": format!("step {turn}")}));
            input.push(json!({"role": "user", "content": format!("go on {turn}")}));
        }
        json!({"model": "gpt-5.5", "instructions": "You are Codex.", "input": input})
    }

    /// Open twenty conversations on `path` and return a treated and a held-out task.
    async fn arms(
        harness: &Harness,
        path: &str,
        body: fn(usize, usize) -> Value,
    ) -> (usize, usize) {
        for task in 0..20 {
            harness.post(path, &body(task, 0)).await;
        }
        let rows = proxy_rows(&harness.events);
        let pick = |want: &str| {
            rows.iter()
                .position(|row| arm(row) == Some(want))
                .unwrap_or_else(|| panic!("no {want} conversation"))
        };
        (pick("treated"), pick("holdout"))
    }

    #[tokio::test]
    async fn a_treated_chat_conversation_carries_the_text_in_its_system_message_every_turn() {
        let harness = Harness::with(shaping).await;
        answering_openai(&harness).await;
        let (treated, held) = arms(&harness, "/v1/chat/completions", chat).await;
        for turns in 1..3 {
            harness
                .post("/v1/chat/completions", &chat(treated, turns))
                .await;
        }
        let sent = harness.upstream_json().await;
        let expected = format!("You are a coding agent.\n\n{TEXT}");
        assert_eq!(sent[treated]["messages"][0]["content"], expected.as_str());
        for later in &sent[20..] {
            assert_eq!(later["messages"][0]["content"], expected.as_str());
        }
        assert_eq!(
            sent[held]["messages"][0]["content"], "You are a coding agent.",
            "the holdout is untouched"
        );
    }

    #[tokio::test]
    async fn a_treated_responses_conversation_carries_the_text_in_its_instructions() {
        let harness = Harness::with(shaping).await;
        answering_openai(&harness).await;
        let (treated, held) = arms(&harness, "/v1/responses", responses).await;
        harness.post("/v1/responses", &responses(treated, 1)).await;
        let sent = harness.upstream_json().await;
        let expected = format!("You are Codex.\n\n{TEXT}");
        assert_eq!(sent[treated]["instructions"], expected.as_str());
        assert_eq!(sent[20]["instructions"], expected.as_str());
        assert_eq!(sent[held]["instructions"], "You are Codex.");
    }

    #[tokio::test]
    async fn an_openai_conversation_first_seen_under_way_is_outside_the_experiment() {
        let harness = Harness::with(shaping).await;
        answering_openai(&harness).await;
        let body = chat(3, 4);
        harness.post("/v1/chat/completions", &body).await;
        assert_eq!(
            harness.upstream_json().await[0]["messages"],
            body["messages"]
        );
        assert!(
            proxy_rows(&harness.events)
                .iter()
                .all(|row| arm(row).is_none())
        );
    }
}
