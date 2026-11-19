//! `proxy.cold_recompact`: what happens on the turn that finds the provider's
//! cache gone, and — the half that makes it safe — on every turn after it.
//!
//! The conversation is the one the read lifecycle exists for: the agent reads
//! a file, edits it, and carries the pre-edit bytes for the rest of the
//! session. Under `frozen_prefix = "first_seen"` that read cannot be replaced
//! while the cache is alive, because replacing it rewrites a cached prefix.
//! The turn that finds the cache gone can, and what it writes must then be
//! what every later turn sends.

mod support;

use std::time::Duration;

use serde_json::{Value, json};
use support::{Harness, proxy_rows};
use wiremock::matchers::any;
use wiremock::{Mock, ResponseTemplate};

use kompressor_core::config::{ColdRecompact, Config, FrozenPrefix};

const MESSAGES: &str = "/v1/messages";
const STALE: &str = "[Read content stale: src/a.rs";

fn usage(read: u32, written: u32) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "msg_1", "type": "message", "role": "assistant",
        "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn",
        "usage": {"input_tokens": 12, "output_tokens": 2,
                  "cache_read_input_tokens": read,
                  "cache_creation_input_tokens": written}
    }))
}

/// Answer the turns in order with these `(read, written)` usages, and every
/// turn after them as a warm one.
async fn answering(harness: &Harness, turns: &[(u32, u32)]) {
    for (read, written) in turns {
        Mock::given(any())
            .respond_with(usage(*read, *written))
            .up_to_n_times(1)
            .mount(&harness.server)
            .await;
    }
    Mock::given(any())
        .respond_with(usage(30_000, 200))
        .mount(&harness.server)
        .await;
}

fn file() -> String {
    (0..300)
        .map(|line| format!("    let value_{line} = compute(input_{line}, {line});\n"))
        .collect()
}

/// A read of `src/a.rs`, an edit of it, and `turns` further exchanges, with
/// the cache breakpoint on the final message, where Claude Code puts it.
fn conversation(turns: usize) -> Value {
    let mut messages = vec![
        json!({"role": "user", "content": "Fix the bug in src/a.rs."}),
        json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "t0", "name": "Read", "input": {"file_path": "src/a.rs"}}
        ]}),
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t0", "content": file()}
        ]}),
        json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "t1", "name": "Edit",
             "input": {"file_path": "src/a.rs", "old_string": "a", "new_string": "b"}}
        ]}),
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "The file was updated."}
        ]}),
    ];
    for turn in 0..turns {
        messages.push(json!({"role": "assistant", "content": [
            {"type": "text", "text": format!("Step {turn} is done.")}
        ]}));
        messages.push(json!({"role": "user", "content": [
            {"type": "text", "text": format!("Go on with step {}.", turn + 1)}
        ]}));
    }
    let last = messages.len() - 1;
    messages[last]["content"][0]["cache_control"] = json!({"type": "ephemeral"});
    json!({"model": "claude-opus-5-5", "max_tokens": 1024, "messages": messages})
}

fn first_seen(recompact: ColdRecompact) -> impl FnOnce(&mut Config) {
    move |config| {
        config.proxy.frozen_prefix = FrozenPrefix::FirstSeen;
        config.proxy.cold_recompact = recompact;
    }
}

/// The messages one request carried upstream, breakpoints taken out: the
/// client moves its marker every turn, and a marker is not part of what the
/// provider caches.
fn sent_messages(body: &Value) -> Vec<Value> {
    fn unmark(value: &mut Value) {
        match value {
            Value::Object(object) => {
                object.remove("cache_control");
                object.values_mut().for_each(unmark);
            }
            Value::Array(items) => items.iter_mut().for_each(unmark),
            _ => {}
        }
    }
    let mut messages = body["messages"].clone();
    unmark(&mut messages);
    messages.as_array().cloned().unwrap_or_default()
}

/// What the read of `src/a.rs` went upstream as.
fn the_read(body: &Value) -> String {
    body["messages"][2]["content"][0]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The fingerprint the proxy filed the conversation under.
fn conversation_of(harness: &Harness) -> u64 {
    let rows = proxy_rows(&harness.events);
    let hex = rows
        .last()
        .and_then(|row| row.conversation.clone())
        .expect("the request was filed under a conversation");
    u64::from_str_radix(&hex, 16).expect("a hex fingerprint")
}

#[tokio::test]
async fn the_turn_that_finds_the_cache_gone_is_rewritten_once_and_then_never_again() {
    let harness = Harness::with(first_seen(ColdRecompact::Observed)).await;
    // Written, read back, then a turn that reads nothing and writes nothing:
    // the provider's own word that the entry is gone.
    answering(&harness, &[(0, 30_000), (30_000, 200), (0, 0), (0, 31_000)]).await;

    for turns in 0..3 {
        harness.post(MESSAGES, &conversation(turns)).await;
    }
    let before = harness.upstream_json().await;
    for body in &before {
        assert_eq!(
            the_read(body),
            file(),
            "while the cache is alive the read is not replaced, edit or no edit"
        );
    }
    proxy_rows(&harness.events);

    // The cold turn.
    harness.post(MESSAGES, &conversation(3)).await;
    let cold_row = proxy_rows(&harness.events)
        .pop()
        .and_then(|row| row.prefix)
        .expect("observed");
    assert_eq!(cold_row.recompacted.as_deref(), Some("observed"));
    assert_eq!(cold_row.ledger_reads, 1);

    // Three warm turns after it.
    for turns in 4..7 {
        harness.post(MESSAGES, &conversation(turns)).await;
    }
    let sent = harness.upstream_json().await;
    let cold = sent_messages(&sent[3]);
    // It did replace the read, or everything below proves nothing.
    assert!(
        the_read(&sent[3]).starts_with(STALE),
        "{}",
        the_read(&sent[3])
    );
    assert!(
        the_read(&sent[3]).contains("ref:k_"),
        "and it is retrievable"
    );
    assert!(sent[3].to_string().len() < sent[2].to_string().len());

    for later in &sent[4..] {
        let messages = sent_messages(later);
        assert_eq!(
            messages[..cold.len()],
            cold[..],
            "every message the cold turn sent goes out as that turn sent it"
        );
    }
    let later_rows = proxy_rows(&harness.events);
    assert!(
        later_rows.iter().all(|row| row
            .prefix
            .as_ref()
            .is_some_and(|seen| seen.recompacted.is_none())),
        "one recompaction, on the turn that found the lapse"
    );
    assert!(
        later_rows
            .iter()
            .all(|row| row.prefix.as_ref().is_some_and(|seen| seen.drift_out == 0)),
        "and nothing moved on the way out afterwards"
    );
}

#[tokio::test]
async fn a_predicted_lapse_is_recompacted_on_the_turn_that_finds_it() {
    let harness = Harness::with(first_seen(ColdRecompact::Predicted)).await;
    // The lapsed turn reads nothing and writes the prefix again, as Anthropic
    // does: by the time that answer arrives, the turn has already been sent.
    answering(
        &harness,
        &[(0, 30_000), (30_000, 200), (30_200, 200), (0, 31_000)],
    )
    .await;
    harness.post(MESSAGES, &conversation(0)).await;
    harness.post(MESSAGES, &conversation(1)).await;
    let conversation_id = conversation_of(&harness);

    // Six minutes idle against a five-minute entry and a one-minute margin
    // is not yet a lapse; seven is.
    let prefixes = harness.state.prefixes();
    prefixes.age_for_test(conversation_id, Duration::from_secs(355));
    harness.post(MESSAGES, &conversation(2)).await;
    assert!(!the_read(&harness.upstream_json().await[2]).starts_with(STALE));
    proxy_rows(&harness.events);

    prefixes.age_for_test(conversation_id, Duration::from_secs(420));
    harness.post(MESSAGES, &conversation(3)).await;
    prefixes.age_for_test(conversation_id, Duration::ZERO);
    let row = proxy_rows(&harness.events)
        .pop()
        .and_then(|row| row.prefix)
        .expect("observed");
    assert_eq!(row.recompacted.as_deref(), Some("predicted"));
    assert_eq!(harness.state.recompact().wrong_predictions(), 0);

    harness.post(MESSAGES, &conversation(4)).await;
    let sent = harness.upstream_json().await;
    assert!(the_read(&sent[3]).starts_with(STALE));
    let cold = sent_messages(&sent[3]);
    assert_eq!(sent_messages(&sent[4])[..cold.len()], cold[..]);
}

/// `observed` waits for the provider's answer; idle time alone moves nothing.
#[tokio::test]
async fn without_predicted_an_idle_conversation_is_left_alone() {
    let harness = Harness::with(first_seen(ColdRecompact::Observed)).await;
    answering(&harness, &[(0, 30_000), (30_000, 200)]).await;
    harness.post(MESSAGES, &conversation(0)).await;
    harness.post(MESSAGES, &conversation(1)).await;
    let conversation_id = conversation_of(&harness);
    harness
        .state
        .prefixes()
        .age_for_test(conversation_id, Duration::from_secs(4_000));
    harness.post(MESSAGES, &conversation(2)).await;
    assert_eq!(the_read(&harness.upstream_json().await[2]), file());
}

/// A prediction the provider contradicts is counted, and the third in an hour
/// ends prediction: the conversation is then left as it is.
#[tokio::test]
async fn predictions_that_keep_reading_from_cache_end_prediction() {
    let harness = Harness::with(first_seen(ColdRecompact::Predicted)).await;
    // Every turn after the first is served from cache, lapse predicted or not.
    answering(&harness, &[(0, 30_000)]).await;
    harness.post(MESSAGES, &conversation(0)).await;
    harness.post(MESSAGES, &conversation(1)).await;
    let conversation_id = conversation_of(&harness);
    let prefixes = harness.state.prefixes();
    let recompact = harness.state.recompact();

    for wrong in 1..=3 {
        prefixes.age_for_test(conversation_id, Duration::from_secs(420));
        harness.post(MESSAGES, &conversation(1 + wrong)).await;
        assert_eq!(recompact.wrong_predictions(), wrong as u64);
    }
    assert!(!recompact.predicting());
    proxy_rows(&harness.events);

    harness.post(MESSAGES, &conversation(5)).await;
    let row = proxy_rows(&harness.events)
        .pop()
        .and_then(|row| row.prefix)
        .expect("observed");
    assert_eq!(row.recompacted, None, "nothing is predicted any more");
}

/// With the setting off, `first_seen` behaves as it did: the read is never
/// replaced, lapse or no lapse.
#[tokio::test]
async fn by_default_a_lapse_changes_nothing_about_the_read() {
    let harness = Harness::with(first_seen(ColdRecompact::Off)).await;
    answering(&harness, &[(0, 30_000), (30_000, 200), (0, 0), (0, 31_000)]).await;
    for turns in 0..6 {
        harness.post(MESSAGES, &conversation(turns)).await;
    }
    for body in harness.upstream_json().await {
        assert!(!the_read(&body).starts_with(STALE));
    }
    assert_eq!(harness.state.recompact().tracked(), 0);
}

/// Under the `marker` freeze there is no watermark to lower, so a
/// recompaction could not be held on the next turn. The setting does nothing.
#[tokio::test]
async fn under_the_marker_freeze_the_setting_does_nothing() {
    let harness = Harness::with(|config| {
        config.proxy.cold_recompact = ColdRecompact::Predicted;
    })
    .await;
    answering(&harness, &[(0, 30_000), (30_000, 200), (0, 0), (0, 31_000)]).await;
    for turns in 0..5 {
        harness.post(MESSAGES, &conversation(turns)).await;
    }
    let rows = proxy_rows(&harness.events);
    assert!(rows.iter().all(|row| {
        row.prefix
            .as_ref()
            .is_some_and(|seen| seen.recompacted.is_none())
    }));
    assert_eq!(harness.state.recompact().tracked(), 0);
}

// ---------------------------------------------------------------------------
// superseded and matured reads
// ---------------------------------------------------------------------------

/// A conversation that reads files and edits nothing: one `Read` per entry of
/// `reads` — its arguments and what came back — then `turns` exchanges.
fn reading(reads: &[(Value, String)], turns: usize) -> Value {
    let mut messages = vec![json!({"role": "user", "content": "Look into the handlers."})];
    for (index, (input, content)) in reads.iter().enumerate() {
        let id = format!("r{index}");
        messages.push(json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": id, "name": "Read", "input": input}
        ]}));
        messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": content}
        ]}));
    }
    for turn in 0..turns {
        messages.push(json!({"role": "assistant", "content": [
            {"type": "text", "text": format!("Step {turn} is done.")}
        ]}));
        messages.push(json!({"role": "user", "content": [
            {"type": "text", "text": format!("Go on with step {}.", turn + 1)}
        ]}));
    }
    let last = messages.len() - 1;
    messages[last]["content"][0]["cache_control"] = json!({"type": "ephemeral"});
    json!({"model": "claude-opus-5-5", "max_tokens": 1024, "messages": messages})
}

/// Written, read back, lost, then the cold turn and three warm ones, over
/// `body(turns)` for seven consecutive values of `turns` from `from`. Returns
/// what went upstream, the cold turn fourth.
async fn through_a_lapse(
    harness: &Harness,
    from: usize,
    body: impl Fn(usize) -> Value,
) -> Vec<Value> {
    answering(harness, &[(0, 30_000), (30_000, 200), (0, 0), (0, 31_000)]).await;
    for turns in from..from + 7 {
        harness.post(MESSAGES, &body(turns)).await;
    }
    harness.upstream_json().await
}

/// Every message the cold turn sent goes out unchanged on the turns after it.
fn assert_held(sent: &[Value]) {
    let cold = sent_messages(&sent[3]);
    for later in &sent[4..] {
        assert_eq!(sent_messages(later)[..cold.len()], cold[..]);
    }
}

#[tokio::test]
async fn a_read_a_later_read_contains_is_replaced_on_the_cold_turn_and_stays_so() {
    let harness = Harness::with(|config| {
        first_seen(ColdRecompact::Observed)(config);
        config.proxy.superseded_reads = true;
    })
    .await;
    let reads = [
        (
            json!({"file_path": "src/a.rs", "offset": 40, "limit": 20}),
            file(),
        ),
        (json!({"file_path": "src/a.rs"}), file()),
    ];
    let sent = through_a_lapse(&harness, 0, |turns| reading(&reads, turns)).await;

    for warm in &sent[..3] {
        assert_eq!(
            the_read(warm),
            file(),
            "nothing is decided while the cache is alive"
        );
    }
    let first = the_read(&sent[3]);
    assert!(
        first.starts_with("[Read superseded: src/a.rs was read again later"),
        "{first}"
    );
    assert!(first.contains("ref:k_"));
    let second = sent[3]["messages"][4]["content"][0]["content"]
        .as_str()
        .unwrap_or_default();
    assert!(
        !second.starts_with("[Read superseded"),
        "the later read is the one kept"
    );
    assert_held(&sent);
}

#[tokio::test]
async fn a_partial_read_does_not_supersede_the_whole_file() {
    let harness = Harness::with(|config| {
        first_seen(ColdRecompact::Observed)(config);
        config.proxy.superseded_reads = true;
    })
    .await;
    let reads = [
        (json!({"file_path": "src/a.rs"}), file()),
        (
            json!({"file_path": "src/a.rs", "offset": 40, "limit": 20}),
            file(),
        ),
    ];
    let sent = through_a_lapse(&harness, 0, |turns| reading(&reads, turns)).await;
    assert!(!the_read(&sent[3]).starts_with("[Read superseded"));
}

fn handlers() -> String {
    (0..12)
        .map(|n| {
            let steps: String = (0..40)
                .map(|step| format!("    let stage_{step} = transform_{n}_{step}(input, state)?;\n"))
                .collect();
            format!(
                "pub fn handler_{n}(input: &Request, state: &mut State) -> Result<Reply, Error> {{\n\
                 {steps}    Ok(build_{n}(stage_39))\n}}\n\n"
            )
        })
        .collect()
}

/// The Anthropic profile: source is left alone, because a body elided from a
/// file in use is fetched straight back.
fn maturing(matured: bool) -> impl FnOnce(&mut Config) {
    move |config| {
        first_seen(ColdRecompact::Observed)(config);
        config.engine.enable_code_aware = false;
        config.proxy.matured_reads = matured;
    }
}

#[cfg(feature = "code-aware")]
#[tokio::test]
async fn a_read_left_alone_is_compressed_as_source_on_the_cold_turn_and_stays_so() {
    let harness = Harness::with(maturing(true)).await;
    let reads = [(json!({"file_path": "src/handlers.rs"}), handlers())];
    // Six exchanges have passed by the first of these turns: the file has
    // been quiet for long enough well before the cache lapses.
    let sent = through_a_lapse(&harness, 6, |turns| reading(&reads, turns)).await;

    for warm in &sent[..3] {
        assert_eq!(
            the_read(warm),
            handlers(),
            "left whole while the cache is alive"
        );
    }
    let matured = the_read(&sent[3]);
    assert!(
        matured.len() < handlers().len() / 2,
        "{} bytes",
        matured.len()
    );
    assert!(matured.contains("pub fn handler_7("), "the signatures stay");
    assert!(
        matured.contains("body lines omitted from handler_7"),
        "{matured}"
    );
    assert_held(&sent);
}

#[cfg(feature = "code-aware")]
#[tokio::test]
async fn a_read_still_in_use_and_one_with_the_setting_off_are_left_whole() {
    // Quiet for only three exchanges when the cache lapses.
    let harness = Harness::with(maturing(true)).await;
    let reads = [(json!({"file_path": "src/handlers.rs"}), handlers())];
    let sent = through_a_lapse(&harness, 0, |turns| reading(&reads, turns)).await;
    assert_eq!(the_read(&sent[3]), handlers());

    let harness = Harness::with(maturing(false)).await;
    let sent = through_a_lapse(&harness, 6, |turns| reading(&reads, turns)).await;
    assert_eq!(the_read(&sent[3]), handlers());
}
