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

/// The fourth turn of [`through_a_lapse`] was recompacted, and no other was.
/// Without this a test of what a recompaction leaves alone passes just as
/// well when nothing was recompacted at all.
fn assert_recompacted(harness: &Harness) {
    let kinds: Vec<Option<String>> = proxy_rows(&harness.events)
        .into_iter()
        .map(|row| row.prefix.and_then(|seen| seen.recompacted))
        .collect();
    let mut expected = vec![None; 7];
    expected[3] = Some("observed".to_string());
    assert_eq!(kinds, expected);
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
    assert_recompacted(&harness);
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
    assert_recompacted(&harness);
    assert_eq!(the_read(&sent[3]), handlers());

    let harness = Harness::with(maturing(false)).await;
    let sent = through_a_lapse(&harness, 6, |turns| reading(&reads, turns)).await;
    assert_recompacted(&harness);
    assert_eq!(the_read(&sent[3]), handlers());
}

// ---------------------------------------------------------------------------
// what a response does and does not say about a recompaction
// ---------------------------------------------------------------------------

/// A lapsed conversation still reads its instructions from cache when other
/// conversations keep them there. That read says nothing about whether this
/// conversation's own entry was alive, and must not count against the
/// prediction — three of them used to switch prediction off.
#[tokio::test]
async fn a_lapse_that_still_reads_the_shared_opening_is_not_a_wrong_prediction() {
    let harness = Harness::with(first_seen(ColdRecompact::Predicted)).await;
    // The opening is about 10,000 tokens; each lapsed turn reads 9,000.
    answering(
        &harness,
        &[
            (0, 40_000),
            (40_000, 200),
            (9_000, 31_000),
            (9_000, 31_000),
            (9_000, 31_000),
        ],
    )
    .await;
    let with_system = |turns| {
        let mut body = conversation(turns);
        body["system"] = json!("You are a careful engineer. ".repeat(1_100));
        body
    };
    harness.post(MESSAGES, &with_system(0)).await;
    harness.post(MESSAGES, &with_system(1)).await;
    let conversation_id = conversation_of(&harness);
    let prefixes = harness.state.prefixes();
    for lapse in 0..3 {
        prefixes.age_for_test(conversation_id, Duration::from_secs(420));
        harness.post(MESSAGES, &with_system(2 + lapse)).await;
        let row = proxy_rows(&harness.events)
            .pop()
            .and_then(|row| row.prefix)
            .expect("observed");
        assert_eq!(
            row.recompacted.as_deref(),
            Some("predicted"),
            "lapse {lapse}"
        );
    }
    let recompact = harness.state.recompact();
    assert_eq!(recompact.wrong_predictions(), 0);
    assert!(recompact.predicting());
}

/// A recompaction whose response carried no usage leaves the tracker holding
/// what it held before: "read nothing, wrote nothing". The next turn must not
/// be taken for a second lapse and decided afresh.
#[tokio::test]
async fn a_recompaction_nobody_answered_is_not_repeated_on_the_next_turn() {
    let harness = Harness::with(first_seen(ColdRecompact::Observed)).await;
    for (read, written) in [(0, 30_000), (30_000, 200), (0, 0)] {
        Mock::given(any())
            .respond_with(usage(read, written))
            .up_to_n_times(1)
            .mount(&harness.server)
            .await;
    }
    // The cold turn's answer: a reply with no usage in it.
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn"
        })))
        .up_to_n_times(1)
        .mount(&harness.server)
        .await;
    Mock::given(any())
        .respond_with(usage(30_000, 200))
        .mount(&harness.server)
        .await;

    for turns in 0..6 {
        harness.post(MESSAGES, &conversation(turns)).await;
    }
    let kinds: Vec<Option<String>> = proxy_rows(&harness.events)
        .into_iter()
        .map(|row| row.prefix.and_then(|seen| seen.recompacted))
        .collect();
    assert_eq!(
        kinds,
        [None, None, None, Some("observed".to_string()), None, None],
        "once, on the turn that found the lapse"
    );
    let sent = harness.upstream_json().await;
    let cold = sent_messages(&sent[3]);
    for later in &sent[4..] {
        assert_eq!(sent_messages(later)[..cold.len()], cold[..]);
    }
}

/// Every row filed since the last look, as how its turn was recompacted, and
/// the conversation the last of them was filed under.
fn recompacted_rows(harness: &Harness) -> (Vec<Option<String>>, u64) {
    let rows = proxy_rows(&harness.events);
    let conversation = rows
        .last()
        .and_then(|row| row.conversation.as_deref())
        .map_or(0, |hex| {
            u64::from_str_radix(hex, 16).expect("a hex fingerprint")
        });
    let kinds = rows
        .into_iter()
        .map(|row| row.prefix.and_then(|seen| seen.recompacted))
        .collect();
    (kinds, conversation)
}

fn recompactions(kinds: &[Option<String>]) -> usize {
    kinds.iter().filter(|kind| kind.is_some()).count()
}

/// A request still in flight when the conversation is recompacted is answered
/// afterwards, about the prefix as it was. Its answer must neither end the
/// wait for the recompaction's own — which here never comes — nor stand as
/// the provider's word that the recompacted prefix is gone: either would have
/// the next turn recompacted a second time, deciding afresh over messages the
/// provider has just cached.
#[tokio::test]
async fn an_older_request_answered_after_a_recompaction_does_not_cause_another() {
    let harness = Harness::with(first_seen(ColdRecompact::Observed)).await;
    for (read, written) in [(0, 30_000), (30_000, 200)] {
        Mock::given(any())
            .respond_with(usage(read, written))
            .up_to_n_times(1)
            .mount(&harness.server)
            .await;
    }
    // The request that is held: read nothing, wrote nothing, a second late.
    Mock::given(any())
        .respond_with(usage(0, 0).set_delay(Duration::from_millis(1_000)))
        .up_to_n_times(1)
        .mount(&harness.server)
        .await;
    // Its twin, answered at once: the provider's word that the cache is gone.
    Mock::given(any())
        .respond_with(usage(0, 0))
        .up_to_n_times(1)
        .mount(&harness.server)
        .await;
    // The recompacted turn's answer carries no usage, so it settles nothing.
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn"
        })))
        .up_to_n_times(1)
        .mount(&harness.server)
        .await;
    Mock::given(any())
        .respond_with(usage(30_000, 200))
        .mount(&harness.server)
        .await;

    for turns in 0..2 {
        harness.post(MESSAGES, &conversation(turns)).await;
    }
    let third = conversation(2);
    let held = harness.post(MESSAGES, &third);
    let meanwhile = async {
        while harness.server.received_requests().await.unwrap().len() < 3 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        harness.post(MESSAGES, &third).await;
        harness.post(MESSAGES, &conversation(3)).await;
        // The recompaction is out and unanswered; the held request is not back.
        assert_eq!(recompactions(&recompacted_rows(&harness).0), 1);
    };
    let (held, ()) = tokio::join!(held, meanwhile);
    assert_eq!(held.status, 200);

    for turns in 4..6 {
        harness.post(MESSAGES, &conversation(turns)).await;
    }
    let (kinds, conversation_id) = recompacted_rows(&harness);
    assert_eq!(
        recompactions(&kinds),
        0,
        "no turn after the held request came back was recompacted again"
    );
    let sent = harness.upstream_json().await;
    let cold = sent_messages(&sent[4]);
    assert!(the_read(&sent[4]).starts_with(STALE));
    for later in &sent[5..] {
        assert_eq!(sent_messages(later)[..cold.len()], cold[..]);
    }
    assert_eq!(harness.state.recompact().epoch(conversation_id), 1);
}

/// The rewrite of a cold turn could not be put back into the coding the
/// client used, so the client's own bytes went out. Nothing was recompacted:
/// the conversation is not left reserved, its ledger is not started, and the
/// next request is compared with what was actually sent.
#[tokio::test]
async fn a_recompaction_that_cannot_be_encoded_is_not_recorded_as_sent() {
    let harness = Harness::with(first_seen(ColdRecompact::Observed)).await;
    answering(
        &harness,
        &[(0, 30_000), (30_000, 200), (0, 0), (30_000, 200), (0, 0)],
    )
    .await;
    for turns in 0..3 {
        harness.post_zstd(MESSAGES, &conversation(turns)).await;
    }
    harness.state.fail_next_encode_for_test();
    let reply = harness.post_zstd(MESSAGES, &conversation(3)).await;
    assert_eq!(reply.status, 200);
    let (kinds, conversation_id) = recompacted_rows(&harness);
    assert_eq!(kinds, [None, None, None, None]);
    let raw = harness.upstream_raw_bodies().await;
    let sent: Value =
        serde_json::from_slice(&zstd::stream::decode_all(raw[3].as_slice()).unwrap()).unwrap();
    assert_eq!(sent, conversation(3), "the client's own body went out");
    assert_eq!(harness.state.recompact().epoch(conversation_id), 0);
    assert_eq!(
        harness.state.recompact().tracked(),
        0,
        "and nothing is held"
    );

    // A warm turn after it. The failed turn went out without the retrieve
    // tool every other turn carries, so the tool coming back is the one
    // thing that moved on the way out — and no message did. Compared with
    // the rewrite that was never sent, this would read as the proxy putting
    // the stale read back: history drift that did not happen.
    harness.post_zstd(MESSAGES, &conversation(4)).await;
    let row = proxy_rows(&harness.events)
        .pop()
        .and_then(|row| row.prefix)
        .expect("observed");
    assert_eq!(
        (row.drift_out, row.history_out_at),
        (kompressor_stats::context::DRIFT_TOOL_SET, None),
        "compared with the bytes that were sent, not the rewrite that was not"
    );
    // That turn's answer says the cache is gone again, and this time the
    // rewrite goes out: the failed one left the conversation free.
    harness.post_zstd(MESSAGES, &conversation(5)).await;
    assert_eq!(recompacted_rows(&harness).0, [Some("observed".to_string())]);
    assert_eq!(harness.state.recompact().epoch(conversation_id), 1);
}

/// A turn found cold whose body the engine then leaves alone — here a
/// conversation too small to be worth compressing — recompacted nothing. The
/// conversation it reserved on the way in is let go on the way out, and no
/// ledger is started for it.
#[tokio::test]
async fn a_cold_turn_the_engine_declines_holds_nothing() {
    let harness = Harness::with(|config| {
        first_seen(ColdRecompact::Observed)(config);
        config.proxy.retrieve_tool = kompressor_core::config::RetrieveTool::Never;
    })
    .await;
    answering(&harness, &[(0, 30_000), (30_000, 200), (0, 0), (0, 0)]).await;
    let small = |turns: usize| {
        let mut messages = vec![json!({"role": "user", "content": "Say hello."})];
        for turn in 0..turns {
            messages.push(json!({"role": "assistant", "content": format!("Hello {turn}.")}));
            messages.push(json!({"role": "user", "content": "Again."}));
        }
        json!({"model": "claude-opus-5-5", "max_tokens": 64, "messages": messages})
    };
    for turns in 0..5 {
        let body = small(turns);
        harness.post(MESSAGES, &body).await;
        assert_eq!(
            harness.upstream_json().await[turns],
            body,
            "turn {turns} went out as it arrived"
        );
        assert_eq!(
            harness.state.recompact().tracked(),
            0,
            "turn {turns}: nothing reserved once the request is over"
        );
    }
    let (kinds, conversation_id) = recompacted_rows(&harness);
    assert_eq!(kinds, [None, None, None, None, None]);
    assert_eq!(harness.state.recompact().epoch(conversation_id), 0);
}

/// A predicted lapse is recorded with what made it predictable, so the report
/// scores the request the runtime acted on: a warm conversation, continued.
#[tokio::test]
async fn the_row_of_a_predicted_lapse_says_the_request_was_one_to_predict_on() {
    let harness = Harness::with(first_seen(ColdRecompact::Predicted)).await;
    answering(&harness, &[(0, 30_000), (30_000, 200), (0, 31_000)]).await;
    harness.post(MESSAGES, &conversation(0)).await;
    harness.post(MESSAGES, &conversation(1)).await;
    let conversation_id = conversation_of(&harness);
    harness
        .state
        .prefixes()
        .advance_clock_for_test(Duration::from_secs(420));
    harness.post(MESSAGES, &conversation(2)).await;
    let row = proxy_rows(&harness.events)
        .pop()
        .and_then(|row| row.prefix)
        .expect("observed");
    assert_eq!(row.recompacted.as_deref(), Some("predicted"));
    assert!(row.could_predict());
    assert_eq!((row.was_warm, row.continues), (true, Some(true)));
    assert!(row.idle_ms.is_some_and(|idle| idle >= 420_000));
    assert_eq!(row.lifetime_s, Some(300));
    // The same request again does not continue the conversation, and however
    // long it waits no lapse is predicted on it.
    harness
        .state
        .prefixes()
        .advance_clock_for_test(Duration::from_secs(4_000));
    harness.post(MESSAGES, &conversation(2)).await;
    let row = proxy_rows(&harness.events)
        .pop()
        .and_then(|row| row.prefix)
        .expect("observed");
    assert!(!row.could_predict());
    assert_eq!((row.recompacted, row.continues), (None, Some(false)));
    assert_eq!(harness.state.recompact().epoch(conversation_id), 1);
}
