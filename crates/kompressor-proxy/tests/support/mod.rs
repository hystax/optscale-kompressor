//! A proxy in front of a real HTTP upstream, for the tests.
//!
//! Everything here exists so a test can state a guarantee about what the
//! *provider* received and what the *client* got back, because those two are
//! the whole contract: what happens in between is this crate's business, and a
//! test that asserted on it would be asserting on the implementation.

#![allow(dead_code)]

pub mod cachesim;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use bytes::Bytes;
use http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::MockServer;

use kompressor_core::AppCore;
use kompressor_core::config::{Config, Upstream};
use kompressor_proxy::{FailOpen, ProxyState};
use kompressor_store::memory::MemoryRefStore;

/// A proxy, its upstream, and the fail-open counters it keeps.
pub struct Harness {
    pub server: MockServer,
    pub state: ProxyState,
    pub failopen: Arc<FailOpen>,
    /// Every statistics event the proxy recorded, for tests that ask what was
    /// *accounted*, not just what was returned.
    pub events: kompressor_stats::writer::CapturedEvents,
}

impl Harness {
    /// The default deployment: one upstream, everything else as shipped.
    pub async fn new() -> Harness {
        Harness::with(|_| {}).await
    }

    /// The default deployment, with the config adjusted.
    pub async fn with(configure: impl FnOnce(&mut Config)) -> Harness {
        let server = MockServer::start().await;
        let mut config = Config::default();
        config.stats.enabled = false;
        config.proxy.upstreams = vec![Upstream {
            name: "provider".to_string(),
            path_prefix: String::new(),
            base_url: server.uri(),
            api_family: "openai".to_string(),
            forward_auth: true,
            ..Default::default()
        }];
        configure(&mut config);

        let (sink, events) = kompressor_stats::writer::capturing_sink(256);
        let core = Arc::new(AppCore::new(config, Arc::new(MemoryRefStore::new()), sink));
        let state = ProxyState::new(core, reqwest::Client::new());
        let failopen = state.failopen();
        Harness {
            server,
            state,
            failopen,
            events,
        }
    }

    pub fn router(&self) -> Router {
        self.state.clone().into_router()
    }

    /// POST a JSON body through the proxy.
    pub async fn post(&self, path: &str, body: &Value) -> Reply {
        self.send(
            Request::post(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }

    /// POST a JSON body compressed the way codex sends it.
    ///
    /// Note the content type: codex declares `application/json` and then zstd
    /// encodes the bytes, so a proxy that decides by content type alone reads
    /// a body that will not parse.
    pub async fn post_zstd(&self, path: &str, body: &Value) -> Reply {
        let encoded = zstd::stream::encode_all(body.to_string().as_bytes(), 3).unwrap();
        self.send(
            Request::post(path)
                .header("content-type", "application/json")
                .header("content-encoding", "zstd")
                .body(Body::from(encoded))
                .unwrap(),
        )
        .await
    }

    /// POST with extra client headers.
    pub async fn post_with(&self, path: &str, body: &Value, extra: &[(&str, &str)]) -> Reply {
        let mut request = Request::post(path).header("content-type", "application/json");
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        self.send(request.body(Body::from(body.to_string())).unwrap())
            .await
    }

    /// POST, take the response headers, and walk away without reading the body.
    ///
    /// What a client does when it gives up: the connection is established, the
    /// proxy is mid-relay, and the body stream is dropped. Returns the status
    /// the client did see.
    pub async fn post_and_abandon(&self, path: &str, body: &Value) -> StatusCode {
        let response = self
            .router()
            .oneshot(
                Request::post(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .expect("the router answers with headers");
        let status = response.status();
        drop(response.into_body());
        status
    }

    /// Send a request the test built itself.
    pub async fn send(&self, request: Request<Body>) -> Reply {
        let response = self
            .router()
            .oneshot(request)
            .await
            .expect("the router answers");
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("a complete body")
            .to_bytes();
        Reply {
            status,
            headers,
            body,
        }
    }

    /// Every request the upstream saw, as text.
    pub async fn upstream_bodies(&self) -> Vec<String> {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .map(|request| String::from_utf8_lossy(&request.body).into_owned())
            .collect()
    }

    /// Every request the upstream saw, as the bytes that actually arrived.
    ///
    /// `upstream_bodies` lossily decodes to text, which is meaningless for a
    /// compressed body — these are the bytes themselves.
    pub async fn upstream_raw_bodies(&self) -> Vec<Vec<u8>> {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .map(|request| request.body.clone())
            .collect()
    }

    /// Every request the upstream saw, parsed.
    pub async fn upstream_json(&self) -> Vec<Value> {
        self.upstream_bodies()
            .await
            .iter()
            .map(|body| serde_json::from_str(body).unwrap_or(Value::Null))
            .collect()
    }

    /// The headers of the upstream's first request.
    pub async fn upstream_headers(&self) -> HeaderMap {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .first()
            .map(|request| request.headers.clone())
            .unwrap_or_default()
    }
}

/// What the client got.
pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn header(&self, name: &str) -> String {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }
}

/// A payload the engine will compress: repetitive machine output, well over the
/// size gate.
pub fn log_payload(lines: usize) -> String {
    (0..lines)
        .map(|i| {
            format!(
                "2026-01-02T03:04:{:02}Z INFO worker heartbeat seq={i} queue=default latency_ms={}",
                i % 60,
                i % 17
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// An OpenAI Chat request whose tool result is worth compressing.
pub fn chat_body(payload: &str) -> Value {
    json!({
        "model": "gpt-5",
        "messages": [
            {"role": "system", "content": "You are a careful assistant."},
            {"role": "user", "content": "What went wrong in this log?"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_0", "type": "function",
                 "function": {"name": "read_file", "arguments": "{\"path\":\"/var/log/app.log\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_0", "content": payload}
        ]
    })
}

/// An OpenAI Responses request with the same content.
pub fn responses_body(payload: &str) -> Value {
    json!({
        "model": "gpt-5",
        "instructions": "You are a careful assistant.",
        "input": [
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "What went wrong in this log?"}]},
            {"type": "function_call", "id": "fc_0", "call_id": "call_0",
             "name": "read_file", "arguments": "{\"path\":\"/var/log/app.log\"}"},
            {"type": "function_call_output", "call_id": "call_0", "output": payload}
        ]
    })
}

/// An Anthropic Messages request with the same content.
pub fn anthropic_body(payload: &str) -> Value {
    json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 1024,
        "system": "You are a careful assistant.",
        "messages": [
            {"role": "user", "content": "What went wrong in this log?"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_0", "name": "read_file",
                 "input": {"path": "/var/log/app.log"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_0", "content": payload}
            ]}
        ]
    })
}

/// The first fragment handle in a body, as the model would read it out of a
/// marker.
pub fn first_handle(body: &str) -> Option<String> {
    let start = body.find("ref:k_")? + "ref:".len();
    body.get(start..start + 14).map(str::to_string)
}

/// How many tool results this request already carries, which is the round the
/// upstream is being asked for.
pub fn round_of(body: &str) -> usize {
    body.matches("\\\"found\\\"").count()
}

/// A short deadline, for the timeout test.
pub const NO_TIME_AT_ALL: Duration = Duration::from_nanos(1);

/// The `proxy_requests` rows the proxy recorded, newest last.
pub fn proxy_rows(
    events: &kompressor_stats::writer::CapturedEvents,
) -> Vec<kompressor_stats::events::ProxyRequestEvent> {
    events
        .drain()
        .into_iter()
        .filter_map(|e| match e {
            kompressor_stats::events::Event::ProxyRequest(row) => Some(row),
            _ => None,
        })
        .collect()
}
