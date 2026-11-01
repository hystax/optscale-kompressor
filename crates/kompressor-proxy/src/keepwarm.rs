//! Keep a conversation's provider cache alive while the client runs a tool.
//!
//! Anthropic keeps a cache entry for five minutes from its last read. An agent
//! that hands a ten-minute build to its shell sends nothing in that time, so
//! the entry lapses and the next request writes the whole conversation again —
//! at $5 per million tokens on Claude Opus 5.5, against $0.20 to read it. On
//! the 89-task Harbor runs that was the largest single cost the proxy could
//! still influence: 19–38 pauses past 270s per run, worth $2.2–$3.4 between
//! the rewrites and the one-hour entries `cache_ttl = "adaptive"` bought to
//! avoid them, against $0.07–$0.26 a run that compression is worth at the
//! shipped Anthropic settings, net of fetches.
//!
//! A read refreshes the entry, so this re-sends the request the provider last
//! cached — the same bytes, with `max_tokens` of 1 and no streaming — every
//! [`ProxyConfig::cache_keep_warm_interval`] until the client's next request in
//! that conversation arrives. Each ping costs the prefix at the cache-read
//! price plus one output token.
//!
//! # When it arms
//!
//! After a response that stopped on `tool_use`. That is the one point at which
//! the client is known to come back with this conversation extended: it owes
//! the model a tool result.
//!
//! And after one that stopped on `end_turn` while the conversation has a
//! background task running — one Claude Code reported starting ("running in
//! background with ID: …", "moved to the background (ID: …)", or a `Monitor`
//! watch, "Monitor started (task …") and has not yet reported on
//! (`<task-id>…</task-id>` in its notification, or "Successfully stopped task:
//! …" when the model stopped it), or while a wake the model set
//! with `ScheduleWakeup` has not yet been delivered. The model is
//! woken when the job ends, so the client will most likely be back; but an
//! agent can also give its final answer with a job still running, and then no
//! request ever comes. Those waits get their own cap,
//! [`ProxyConfig::cache_keep_warm_background_pings`]. In the second keep-warm
//! run they were 13 of the 30 pauses past five minutes; any other `end_turn`
//! may be the last turn the conversation has, and is not kept warm.
//!
//! It does not arm for a request that asked for one-hour entries, which outlast
//! any pause this would cover, nor for one whose `thinking` has a token budget:
//! `max_tokens` must exceed the budget there, and changing `thinking` changes
//! the cache key, so no cheap ping exists. Nor below [`MIN_PREFIX_TOKENS`].
//!
//! # When it stops
//!
//! At the client's next request in the conversation, which cancels the timer
//! before that request is sent; after the configured number of pings; and at
//! the first ping that fails, is refused, or finds nothing in the cache. A ping
//! that reads nothing has just written the prefix at full price, which is the
//! cost this exists to avoid, so the next one would only repeat it.
//!
//! [`ProxyConfig::cache_keep_warm_interval`]: kompressor_core::config::ProxyConfig::cache_keep_warm_interval
//! [`ProxyConfig::cache_keep_warm_max_pings`]: kompressor_core::config::ProxyConfig::cache_keep_warm_max_pings
//! [`ProxyConfig::cache_keep_warm_background_pings`]: kompressor_core::config::ProxyConfig::cache_keep_warm_background_pings

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::{HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tokio::task::AbortHandle;

use kompressor_stats::events::{Event, ProxyRequestEvent};

use crate::ProxyState;
use crate::headers;

/// The smallest prompt any Anthropic model caches; some models need more.
/// Below it there is no cache entry to keep alive.
pub const MIN_PREFIX_TOKENS: u32 = 1024;

/// How long one ping may take before it is abandoned. Generous for one output
/// token, and well inside the interval, so a slow ping cannot overlap the next.
const PING_TIMEOUT: Duration = Duration::from_secs(60);

/// The pending timers, one per conversation.
#[derive(Default)]
pub struct KeepWarm {
    armed: Mutex<HashMap<u64, Armed>>,
    next_generation: Mutex<u64>,
}

struct Armed {
    /// Distinguishes this timer from a later one for the same conversation, so
    /// a timer that ends on its own removes itself and not its successor.
    generation: u64,
    task: AbortHandle,
}

/// Everything needed to repeat one request as a ping.
pub(crate) struct Ping {
    pub(crate) url: String,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Bytes,
    pub(crate) request_id: String,
    pub(crate) tenant: String,
    pub(crate) model: Option<String>,
    pub(crate) upstream: String,
    pub(crate) conversation: u64,
}

impl Ping {
    /// The ping for the request that sent `body` upstream, or `None` when no
    /// cheap ping exists for it.
    ///
    /// The body is the one the provider cached — what this proxy sent, not
    /// what the client did — and every field that shapes the prompt is kept as
    /// it was. Only `max_tokens` and `stream` change, and neither is part of
    /// the cache key.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn for_request(
        sent: &Value,
        url: &str,
        upstream_headers: &HeaderMap,
        request_id: &str,
        tenant: &str,
        model: Option<String>,
        upstream: &str,
        conversation: u64,
    ) -> Option<Ping> {
        let mut body = sent.as_object()?.clone();
        if thinking_has_budget(sent) {
            return None;
        }
        body.insert("max_tokens".to_string(), json!(1));
        body.insert("stream".to_string(), json!(false));
        let body = serde_json::to_vec(&Value::Object(body)).ok()?;

        let mut headers = upstream_headers.clone();
        // The body is re-serialized here and goes out plain, whatever coding
        // the client's arrived in; `reqwest` sets the length.
        headers.remove(http::header::CONTENT_ENCODING);
        headers.remove(http::header::CONTENT_LENGTH);
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            http::header::ACCEPT,
            HeaderValue::from_static("application/json"),
        );
        Some(Ping {
            url: url.to_string(),
            headers,
            body: Bytes::from(body),
            request_id: request_id.to_string(),
            tenant: tenant.to_string(),
            model,
            upstream: upstream.to_string(),
            conversation,
        })
    }
}

/// What Claude Code says when a command starts in the background, is moved
/// there after outliving its timeout, or when its `Monitor` tool starts a
/// watch. The task id follows.
///
/// The `Monitor` one was missing, and a watch is a background wait like any
/// other: the model ends its turn and is woken by the watch's event. Measured
/// on the 89-task run of 2026-10-30 (`14-37-11`): winning-avg-corewars armed a
/// watch, ended its turn, and the event came 420 s later with no ping sent —
/// 50,616 tokens written again at the one-hour price.
const BACKGROUND_STARTED: &[&str] = &[
    "running in background with ID: ",
    "moved to the background (ID: ",
    "Monitor started (task ",
];
/// The tag a task notification names its task with, which it sends when the
/// task ends on its own: completed or failed.
const BACKGROUND_REPORTED: (&str, &str) = ("<task-id>", "</task-id>");
/// What the `TaskStop` tool answers, the task id following. A task the model
/// stops gets no notification, so this answer is its end. Measured on the
/// 89-task run of 2026-11-01 (`13-41-26`): crack-7z-hash, extract-moves-from-
/// video, reshard-c4-data and schemelike-metacircular-eval each stopped a task
/// that had outlived its timeout, finished, and kept 10–11 pings going after
/// the trial had ended — all 43 of the run's pings to finished trials.
const BACKGROUND_STOPPED: &str = "Successfully stopped task: ";

/// What Claude Code's `ScheduleWakeup` tool answers when it sets a wake, and
/// what the prompt that wake delivers says. A wake has no id: one is pending
/// when the last set comes after the last delivered, unless the model called
/// the tool with `stop` since.
///
/// It is a background wait without a task: the model ends its turn and the
/// client comes back when the wake fires. Measured on the 89-task run of
/// 2026-10-31 (`10-06-33`): crack-7z-hash detached its cracker, let its last
/// `Monitor` watch expire, set a 1200 s wake and ended its turn with no task
/// left unreported; the wake came 897 s later with no ping sent, and 40,104
/// tokens were written again at the one-hour price.
const WAKE_SCHEDULED: &str = "Next wakeup scheduled for ";
/// The delivered tick says this only for the `/loop` sentinel prompts. Any
/// other prompt comes back as a user message that is that prompt and nothing
/// else, so a delivery is also a user text equal to the last `prompt` the
/// tool was called with. Measured on the 89-task run of 2026-10-31
/// (`18-08-17`): mcmc-sampling-stan, train-fasttext and rstan-to-pystan each
/// got their wake as "check rstan install" or the like, answered it and
/// exited, and the wake, never seen delivered, kept 11 pings going each.
const WAKE_DELIVERED: &str = "You scheduled this tick via the ScheduleWakeup tool";
/// What follows [`WAKE_SCHEDULED`] up to the wake's delay in seconds.
const WAKE_DELAY: (&str, &str) = ("(in ", "s)");

/// What an `end_turn` is waiting on, when it is waiting on something.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Wait {
    /// A background task, which has no known end.
    Task,
    /// A `ScheduleWakeup` wake, due this long after it was set, or `None` when
    /// the tool's answer did not say.
    Wake(Option<Duration>),
}

impl Wait {
    /// How many pings at `interval` this wait can use, at most `cap`. A wake
    /// is due at a known time; if the client is not back by then it has gone,
    /// so the pings stop there rather than at the cap.
    pub(crate) fn max_pings(&self, interval: Duration, cap: u32) -> u32 {
        match self {
            Wait::Wake(Some(due)) if !interval.is_zero() => {
                let pings = (due.as_secs_f64() / interval.as_secs_f64()).ceil() as u32;
                pings.max(1).min(cap)
            }
            _ => cap,
        }
    }
}

/// What the conversation in `body` is waiting on: a background task it
/// started and has not been told the end of, or else a `ScheduleWakeup` wake
/// still to come.
///
/// Read from the request, not the response: a task the response started is
/// one the client has not yet run, and a notification can only arrive in a
/// later request. Every text the messages carry is read — a notification is
/// a user text block on some turns and a system message on others.
pub(crate) fn background_wait(body: &Value) -> Option<Wait> {
    let mut started = Vec::new();
    let mut reported = Vec::new();
    let mut wake_pending = false;
    let mut wake_due = None;
    let mut wake_prompt = None;
    for message in body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let content = message.get("content");
        if wake_stopped(content) {
            wake_pending = false;
        }
        if let Some(prompt) = wake_prompt_in(content) {
            wake_prompt = Some(prompt);
        }
        if message.get("role").and_then(Value::as_str) == Some("user")
            && wake_prompt
                .as_deref()
                .is_some_and(|prompt| user_texts(content).any(|text| text.trim() == prompt))
        {
            wake_pending = false;
        }
        texts(content, &mut |text| {
            let (set, delivered) = (text.rfind(WAKE_SCHEDULED), text.rfind(WAKE_DELIVERED));
            if set.is_some() || delivered.is_some() {
                wake_pending = set > delivered;
            }
            if let Some(at) = set.filter(|_| wake_pending) {
                wake_due = wake_delay(&text[at..]);
            }
            for marker in BACKGROUND_STARTED {
                started.extend(ids_after(text, marker));
            }
            reported.extend(ids_after(text, BACKGROUND_STOPPED));
            let (open, close) = BACKGROUND_REPORTED;
            let mut rest = text;
            while let Some(at) = rest.find(open) {
                rest = &rest[at + open.len()..];
                if let Some(end) = rest.find(close) {
                    reported.push(rest[..end].to_string());
                }
            }
        });
    }
    if started.iter().any(|id| !reported.contains(id)) {
        Some(Wait::Task)
    } else {
        wake_pending.then_some(Wait::Wake(wake_due))
    }
}

/// The delay in "Next wakeup scheduled for 09:59:00 (in 1202s)".
fn wake_delay(scheduled: &str) -> Option<Duration> {
    let (open, close) = WAKE_DELAY;
    let after = &scheduled[scheduled.find(open)? + open.len()..];
    after[..after.find(close)?]
        .parse()
        .ok()
        .map(Duration::from_secs)
}

/// The trimmed `prompt` of the last `ScheduleWakeup` call in `content` that
/// sets a wake.
fn wake_prompt_in(content: Option<&Value>) -> Option<String> {
    content?
        .as_array()?
        .iter()
        .filter(|block| {
            block.get("type").and_then(Value::as_str) == Some("tool_use")
                && block.get("name").and_then(Value::as_str) == Some("ScheduleWakeup")
        })
        .filter_map(|block| block.pointer("/input/prompt").and_then(Value::as_str))
        .map(str::trim)
        .rfind(|prompt| !prompt.is_empty())
        .map(str::to_string)
}

/// The texts a user message says itself: a plain string or its text blocks,
/// not what a tool result carries, whose text is under `content`.
fn user_texts(content: Option<&Value>) -> impl Iterator<Item = &str> {
    let (string, blocks) = match content {
        Some(Value::String(text)) => (Some(text.as_str()), None),
        Some(Value::Array(blocks)) => (None, Some(blocks)),
        _ => (None, None),
    };
    string.into_iter().chain(
        blocks
            .into_iter()
            .flatten()
            .filter_map(|block| block.get("text").and_then(Value::as_str)),
    )
}

/// Whether `content` has a `ScheduleWakeup` call that ends the loop.
fn wake_stopped(content: Option<&Value>) -> bool {
    content.and_then(Value::as_array).is_some_and(|blocks| {
        blocks.iter().any(|block| {
            block.get("type").and_then(Value::as_str) == Some("tool_use")
                && block.get("name").and_then(Value::as_str) == Some("ScheduleWakeup")
                && block.pointer("/input/stop").and_then(Value::as_bool) == Some(true)
        })
    })
}

/// Every task id that follows `marker` in `text`.
fn ids_after(text: &str, marker: &str) -> Vec<String> {
    text.match_indices(marker)
        .map(|(at, _)| {
            text[at + marker.len()..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .collect::<String>()
        })
        .filter(|id| !id.is_empty())
        .collect()
}

/// Call `each` on every text in a message's content: a plain string, a text
/// block, or the content of a tool result, however nested.
fn texts(content: Option<&Value>, each: &mut impl FnMut(&str)) {
    match content {
        Some(Value::String(text)) => each(text),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                texts(Some(block), each);
            }
        }
        Some(Value::Object(block)) => {
            texts(block.get("text"), each);
            texts(block.get("content"), each);
        }
        _ => {}
    }
}

/// `thinking` with a `budget_tokens`, which `max_tokens` must exceed.
fn thinking_has_budget(body: &Value) -> bool {
    body.pointer("/thinking/budget_tokens").is_some()
}

/// How a timer is to run.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Schedule {
    pub(crate) interval: Duration,
    pub(crate) max_pings: u32,
}

/// What one ping found.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// The provider served the prefix from cache, so it is good for another
    /// interval.
    Warm,
    /// Anything else: an error, a refusal, or a prefix that had already gone.
    Stop,
}

impl KeepWarm {
    /// Stop pinging `conversation`. Called as the client's next request in it
    /// arrives, before that request is sent.
    pub fn cancel(&self, conversation: u64) {
        if let Ok(mut armed) = self.armed.lock()
            && let Some(previous) = armed.remove(&conversation)
        {
            previous.task.abort();
        }
    }

    /// How many conversations have a timer running. For tests and `/metrics`.
    pub fn armed(&self) -> usize {
        self.armed.lock().map(|armed| armed.len()).unwrap_or(0)
    }

    /// Start pinging `ping`'s conversation, replacing any timer it had.
    ///
    /// `cached_at` is when the provider last read the prefix: the start of the
    /// request that cached it, not its end, because a long answer eats into
    /// the five minutes too.
    pub(crate) fn arm(
        &self,
        state: ProxyState,
        ping: Ping,
        schedule: Schedule,
        cached_at: Instant,
    ) {
        if schedule.max_pings == 0 {
            return;
        }
        let conversation = ping.conversation;
        let generation = {
            let Ok(mut next) = self.next_generation.lock() else {
                return;
            };
            *next += 1;
            *next
        };
        // Held across the spawn, so a timer that ends at once cannot look for
        // its entry before the entry exists.
        let Ok(mut armed) = self.armed.lock() else {
            return;
        };
        let keepwarm = state.keepwarm.clone();
        let task = tokio::spawn(async move {
            let mut due = cached_at + schedule.interval;
            for sent in 1..=schedule.max_pings {
                tokio::time::sleep_until(tokio::time::Instant::from_std(due)).await;
                let began = Instant::now();
                if send(&state, &ping, sent).await == Outcome::Stop {
                    break;
                }
                due = began + schedule.interval;
            }
            if let Ok(mut armed) = keepwarm.armed.lock()
                && armed
                    .get(&conversation)
                    .is_some_and(|current| current.generation == generation)
            {
                armed.remove(&conversation);
            }
        });
        if let Some(previous) = armed.insert(
            conversation,
            Armed {
                generation,
                task: task.abort_handle(),
            },
        ) {
            previous.task.abort();
        }
    }
}

/// Send one ping and file it.
async fn send(state: &ProxyState, ping: &Ping, sent: u32) -> Outcome {
    let request_id = format!("{}.keepwarm{sent}", ping.request_id);
    let mut headers = ping.headers.clone();
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        headers.insert(http::HeaderName::from_static(headers::REQUEST_ID), value);
    }
    let time_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let began = Instant::now();
    let result = state
        .client
        .post(&ping.url)
        .headers(headers)
        .body(ping.body.clone())
        .timeout(PING_TIMEOUT)
        .send()
        .await;
    let waited = began.elapsed();

    let mut event = ProxyRequestEvent {
        time_ms,
        tenant: ping.tenant.clone(),
        request_id,
        api_family: kompressor_wire::ApiFamily::AnthropicMessages
            .as_str()
            .to_string(),
        model: ping.model.clone(),
        upstream: ping.upstream.clone(),
        status: None,
        stream: false,
        strip_usage: false,
        prompt_tokens: None,
        completion_tokens: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        cache_write_1h_tokens: None,
        shadow_prompt_tokens: None,
        conversation: Some(format!("{:016x}", ping.conversation)),
        retrieve_rounds: 0,
        retrieve_tokens_injected: 0,
        upstream_ms: Some(waited.as_secs_f64() * 1000.0),
        total_ms: None,
        error: None,
        keep_warm: true,
    };
    let outcome = match result {
        Err(error) => {
            event.error = Some(error.to_string());
            Outcome::Stop
        }
        Ok(response) => {
            let status = response.status();
            event.status = Some(status.as_u16());
            let body = response.bytes().await.unwrap_or_default();
            let usage = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|value| value.get("usage").cloned());
            if let Some(usage) = &usage {
                let count = |key: &str| usage.get(key).and_then(Value::as_u64).map(|n| n as u32);
                event.prompt_tokens = count("input_tokens");
                event.completion_tokens = count("output_tokens");
                event.cache_read_tokens = count("cache_read_input_tokens");
                event.cache_write_tokens = count("cache_creation_input_tokens");
                event.cache_write_1h_tokens = usage
                    .pointer("/cache_creation/ephemeral_1h_input_tokens")
                    .and_then(Value::as_u64)
                    .map(|n| n as u32);
            }
            if !status.is_success() {
                // Said once per conversation, at the ping that ends it: a
                // provider that refuses `max_tokens: 1` for some request shape
                // would otherwise turn this off without a word.
                tracing::warn!(
                    status = status.as_u16(),
                    conversation = format!("{:016x}", ping.conversation),
                    "a keep-warm ping was refused; this conversation is no longer kept warm"
                );
                event.error = Some(format!("keep-warm ping refused with {status}"));
                Outcome::Stop
            } else if event.cache_read_tokens.unwrap_or(0) == 0 {
                Outcome::Stop
            } else {
                // The prefix was read just now, so a pause the adaptive TTL
                // would otherwise see is not one.
                state.prefixes.kept_warm(ping.conversation);
                Outcome::Warm
            }
        }
    };
    event.total_ms = Some(began.elapsed().as_secs_f64() * 1000.0);
    state.core.stats().send(Event::ProxyRequest(event));
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent() -> Value {
        json!({
            "model": "claude-opus-5-5",
            "max_tokens": 128000,
            "stream": true,
            "thinking": {"type": "adaptive"},
            "system": [{"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}],
            "messages": [{"role": "user", "content": "hi"}],
        })
    }

    fn ping_for(body: &Value) -> Option<Ping> {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_ENCODING,
            HeaderValue::from_static("zstd"),
        );
        headers.insert(
            http::header::CONTENT_LENGTH,
            HeaderValue::from_static("999"),
        );
        headers.insert("x-api-key", HeaderValue::from_static("sk-test"));
        Ping::for_request(
            body,
            "http://up/v1/messages",
            &headers,
            "req",
            "t",
            None,
            "up",
            7,
        )
    }

    #[test]
    fn a_ping_changes_only_what_the_cache_key_ignores() {
        let ping = ping_for(&sent()).expect("a ping");
        let body: Value = serde_json::from_slice(&ping.body).unwrap();
        assert_eq!(body["max_tokens"], 1);
        assert_eq!(body["stream"], false);
        let mut expected = sent();
        expected["max_tokens"] = json!(1);
        expected["stream"] = json!(false);
        assert_eq!(body, expected, "everything that shapes the prompt is kept");
    }

    #[test]
    fn a_ping_goes_out_plain_with_the_credentials_it_was_given() {
        let ping = ping_for(&sent()).expect("a ping");
        assert!(ping.headers.get(http::header::CONTENT_ENCODING).is_none());
        assert!(ping.headers.get(http::header::CONTENT_LENGTH).is_none());
        assert_eq!(ping.headers["x-api-key"], "sk-test");
    }

    fn background_task_running(body: &Value) -> bool {
        background_wait(body).is_some()
    }

    fn with_messages(messages: Value) -> Value {
        json!({"model": "claude-opus-5-5", "messages": messages})
    }

    #[test]
    fn a_started_background_task_is_running_until_it_is_reported() {
        let started = json!([
            {"role": "user", "content": "train it"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash",
                "input": {"command": "python train.py", "run_in_background": true}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1",
                "content": "Command running in background with ID: b2s048qdc. Output is being written to: /tmp/x.output."}]},
        ]);
        assert!(background_task_running(&with_messages(started.clone())));

        let mut reported = started.as_array().unwrap().clone();
        reported.push(json!({"role": "user", "content": [{"type": "text", "text":
            "<system-reminder>\n[SYSTEM NOTIFICATION - NOT USER INPUT]\n<task-notification>\n<task-id>b2s048qdc</task-id>\n<status>completed</status>"}]}));
        assert!(!background_task_running(&with_messages(json!(reported))));
    }

    #[test]
    fn a_command_moved_to_the_background_counts_and_so_does_any_one_of_several() {
        let body = with_messages(json!([
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                {"type": "text", "text": "Command did not complete within its 600s timeout and was moved to the background (ID: boc9wtynk). You will be notified when it completes."}]}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_2",
                "content": "Command running in background with ID: aaa111."}]},
            {"role": "system", "content": "<task-id>aaa111</task-id>"},
        ]));
        assert!(
            background_task_running(&body),
            "boc9wtynk has not been reported"
        );
    }

    #[test]
    fn a_task_the_model_stops_is_ended_by_the_stop() {
        let started = json!([
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1",
                "content": "Command did not complete within its 600s timeout and was moved to the background (ID: bxf73rkc1). You will be notified when it completes."}]},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_2", "name": "TaskStop",
                "input": {"task_id": "bxf73rkc1"}}]},
        ]);
        assert!(background_task_running(&with_messages(started.clone())));

        let mut stopped = started.as_array().unwrap().clone();
        stopped.push(json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_2",
            "content": "{\"message\":\"Successfully stopped task: bxf73rkc1 (cd /app && python compress.py)\",\"task_id\":\"bxf73rkc1\",\"task_type\":\"local_bash\"}"}]}));
        assert!(!background_task_running(&with_messages(json!(stopped))));
    }

    #[test]
    fn a_monitor_watch_is_running_until_its_first_event() {
        let started = json!([
            {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "Monitor",
                "input": {"description": "parameter search completion", "timeout_ms": 600000,
                    "command": "until [ -s /tmp/w/out.txt ]; do sleep 5; done"}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1",
                "content": "Monitor started (task b166z0pbd, expires in 10m unless the source ends first; you get one notice at expiry \u{2014} re-arm if you still need the watch). You will be notified on each event."}]},
        ]);
        assert!(background_task_running(&with_messages(started.clone())));

        let mut reported = started.as_array().unwrap().clone();
        reported.push(json!({"role": "user", "content":
            "<system-reminder>\n<task-notification>\n<task-id>b166z0pbd</task-id>\n<summary>Monitor event: \"parameter search completion\"</summary>"}));
        assert!(!background_task_running(&with_messages(json!(reported))));
    }

    #[test]
    fn a_scheduled_wake_is_a_wait_until_it_is_delivered_or_stopped() {
        let scheduled = json!([
            {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "ScheduleWakeup",
                "input": {"delaySeconds": 1200, "prompt": "<<autonomous-loop-dynamic>>", "reason": "fallback heartbeat"}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1",
                "content": "Next wakeup scheduled for 09:59:00 (in 1202s). If you owe the user a status update this tick, write it now as ordinary response text; then end the turn."}]},
            {"role": "assistant", "content": [{"type": "text", "text": "Waiting."}]},
        ]);
        assert!(background_task_running(&with_messages(scheduled.clone())));

        let mut delivered = scheduled.as_array().unwrap().clone();
        delivered.push(json!({"role": "user", "content": [{"type": "text", "text":
            "# Autonomous loop tick (dynamic pacing)\n\nYou scheduled this tick via the ScheduleWakeup tool (not a recurring cron)."}]}));
        assert!(!background_task_running(&with_messages(json!(
            delivered.clone()
        ))));

        let mut set_again = delivered.clone();
        set_again.push(scheduled[1].clone());
        assert!(background_task_running(&with_messages(json!(
            set_again.clone()
        ))));

        let mut stopped = set_again;
        stopped.push(
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_2",
            "name": "ScheduleWakeup", "input": {"stop": true}}]}),
        );
        stopped.push(
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_2",
            "content": "Loop stopped."}]}),
        );
        assert!(!background_task_running(&with_messages(json!(stopped))));
    }

    #[test]
    fn a_wake_with_its_own_prompt_is_delivered_as_that_prompt() {
        let scheduled = json!([
            {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "ScheduleWakeup",
                "input": {"delaySeconds": 600, "noop": true, "prompt": "check rstan install", "reason": "fallback"}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1",
                "content": "Next wakeup scheduled for 15:17:00 (in 620s). If you owe the user a status update this tick, write it now as ordinary response text; then end the turn."}]},
            {"role": "assistant", "content": [{"type": "text", "text": "Waiting."}]},
        ]);
        assert_eq!(
            background_wait(&with_messages(scheduled.clone())),
            Some(Wait::Wake(Some(Duration::from_secs(620))))
        );

        let mut quoted = scheduled.as_array().unwrap().clone();
        quoted.push(
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_2",
            "content": "check rstan install"}]}),
        );
        assert!(
            background_task_running(&with_messages(json!(quoted))),
            "a tool result is not the wake"
        );

        for delivered in [
            json!("check rstan install"),
            json!([{"type": "text", "text": "check rstan install\n"}]),
        ] {
            let mut messages = scheduled.as_array().unwrap().clone();
            messages.push(json!({"role": "user", "content": delivered}));
            assert!(!background_task_running(&with_messages(json!(messages))));
        }
    }

    #[test]
    fn a_wake_is_kept_warm_until_it_is_due_and_a_task_until_the_cap() {
        let interval = Duration::from_secs(270);
        let wake = |secs| Wait::Wake(Some(Duration::from_secs(secs)));
        assert_eq!(wake(230).max_pings(interval, 11), 1);
        assert_eq!(wake(620).max_pings(interval, 11), 3);
        assert_eq!(wake(1241).max_pings(interval, 11), 5);
        assert_eq!(wake(7200).max_pings(interval, 11), 11);
        assert_eq!(Wait::Wake(None).max_pings(interval, 11), 11);
        assert_eq!(Wait::Task.max_pings(interval, 11), 11);
    }

    #[test]
    fn a_task_running_beside_a_wake_is_waited_on_without_a_due_time() {
        let body = with_messages(json!([
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1",
                "content": "Command running in background with ID: aaa111."}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_2",
                "content": "Next wakeup scheduled for 15:17:00 (in 620s)."}]},
        ]));
        assert_eq!(background_wait(&body), Some(Wait::Task));
    }

    #[test]
    fn a_notification_for_another_task_does_not_end_this_one() {
        let body = with_messages(json!([
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t",
                "content": "Command running in background with ID: b_one-1."}]},
            {"role": "user", "content": "<task-notification><task-id>b_two-2</task-id></task-notification>"},
        ]));
        assert!(background_task_running(&body));
    }

    #[test]
    fn an_id_is_read_whole_and_a_marker_with_none_is_ignored() {
        assert_eq!(
            ids_after(
                "running in background with ID: b2s_04-8qdc. Output",
                BACKGROUND_STARTED[0]
            ),
            vec!["b2s_04-8qdc".to_string()]
        );
        let body = with_messages(json!([
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t",
                "content": "Command running in background with ID: ."}]},
        ]));
        assert!(!background_task_running(&body), "no id, nothing to wait on");
    }

    #[test]
    fn the_model_naming_a_task_is_not_the_client_starting_one() {
        // Only the client's own report starts a task; an assistant that says
        // it is waiting on one has not started anything.
        let body = with_messages(json!([
            {"role": "assistant", "content": [{"type": "text",
                "text": "Waiting on the quantized run (b2s048qdc)."}]},
        ]));
        assert!(!background_task_running(&body));
    }

    #[test]
    fn a_notification_sent_as_a_system_string_is_read() {
        let body = with_messages(json!([
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": [
                {"type": "text", "text": "Command did not complete within its 600s timeout and was moved to the background (ID: boc9wtynk)."}]}]},
            {"role": "system", "content": "[SYSTEM NOTIFICATION - NOT USER INPUT]\n<task-id>boc9wtynk</task-id>"},
        ]));
        assert!(!background_task_running(&body));
    }

    #[test]
    fn a_conversation_without_background_tasks_has_none_running() {
        let body = with_messages(json!([
            {"role": "user", "content": "run the tests"},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": "ok"}]},
        ]));
        assert!(!background_task_running(&body));
        assert!(!background_task_running(&json!({})));
    }

    #[test]
    fn budgeted_thinking_has_no_cheap_ping() {
        let mut body = sent();
        body["thinking"] = json!({"type": "enabled", "budget_tokens": 4096});
        assert!(ping_for(&body).is_none());
    }
}
