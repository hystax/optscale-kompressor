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
//! Only after a response that stopped on `tool_use`. That is the one point at
//! which the client is known to come back with this conversation extended: it
//! owes the model a tool result. A turn that ended on `end_turn` may be the
//! last the conversation ever has, and pinging it would buy nothing for up to
//! [`ProxyConfig::cache_keep_warm_max_pings`] intervals.
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

    #[test]
    fn budgeted_thinking_has_no_cheap_ping() {
        let mut body = sent();
        body["thinking"] = json!({"type": "enabled", "budget_tokens": 4096});
        assert!(ping_for(&body).is_none());
    }
}
