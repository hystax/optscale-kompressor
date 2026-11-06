//! The one request the proxy sends, and the rounds that follow it.
//!
//! Held as a struct rather than rebuilt per round because a retrieve round is
//! the *same* request with a longer tail: same URL, same headers, same base
//! prompt. Rebuilding it would be an invitation to let something drift between
//! rounds, and a base prompt that drifts is a provider cache prefix that
//! misses — which costs more than the whole compression saved.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use serde_json::Value;

use crate::bodycodec::BodyEncoding;
use crate::retention;

/// A request in flight to the provider.
pub(crate) struct Outbound {
    client: reqwest::Client,
    method: Method,
    url: String,
    headers: HeaderMap,
    body: Bytes,
    /// The parsed body, kept only when the proxy may need to extend it.
    json: Option<Value>,
    /// Applied to buffered requests only: a streamed response is long-lived by
    /// design, and a total timeout on it would cut off the answer mid-sentence.
    timeout: Option<Duration>,
    /// The coding `body` is in, and the one any replacement has to be put back
    /// into. The client's `content-encoding` is forwarded unchanged, so a body
    /// this proxy rewrites has to keep the promise that header makes.
    encoding: BodyEncoding,
    /// Microseconds spent waiting on the provider, accumulated across every
    /// send this request makes.
    ///
    /// Shared rather than returned because the retrieve loop sends more than
    /// once — `nonstream` can go several rounds against the provider inside one
    /// client request, and a number that described only the last of them would
    /// understate the wait by exactly the part the loop added. Accumulating at
    /// the one place every path goes through is what stops the two disagreeing.
    waited_us: Arc<AtomicU64>,
    /// Set when the proxy added `prompt_cache_retention`, so a refusal of it
    /// can be answered by sending without it. See [`crate::retention`].
    retention: Option<retention::Guard>,
}

impl Outbound {
    pub(crate) fn new(
        client: reqwest::Client,
        method: Method,
        url: String,
        headers: HeaderMap,
        body: Bytes,
        encoding: BodyEncoding,
    ) -> Outbound {
        Outbound {
            client,
            method,
            url,
            headers,
            body,
            json: None,
            timeout: None,
            encoding,
            waited_us: Arc::new(AtomicU64::new(0)),
            retention: None,
        }
    }

    /// The body carries a `prompt_cache_retention` the proxy added.
    pub(crate) fn with_retention(mut self, guard: Option<retention::Guard>) -> Outbound {
        self.retention = guard;
        self
    }

    /// Keep the parsed body so later rounds can append to it.
    pub(crate) fn with_json(mut self, json: Value) -> Outbound {
        self.json = Some(json);
        self
    }

    /// Bound a buffered request's total time.
    pub(crate) fn with_timeout(mut self, timeout: Duration) -> Outbound {
        self.timeout = Some(timeout);
        self
    }

    /// The parsed body, if one was kept.
    pub(crate) fn json(&self) -> Option<&Value> {
        self.json.as_ref()
    }

    /// Replace the body with an extended one.
    ///
    /// Returns false when the new body will not serialize, in which case the
    /// request in flight is unchanged and the caller falls back to what it
    /// already has.
    pub(crate) fn set_json(&mut self, value: Value) -> bool {
        let bytes = match serde_json::to_vec(&value) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(%error, "could not serialize the next retrieve round");
                return false;
            }
        };
        // Put it back in the coding the request arrived in. A retrieve round
        // re-sends this body to the same upstream under the same headers, so a
        // plain one here would be read as whatever `content-encoding` claims.
        let Some(encoded) = self.encoding.encode(&bytes) else {
            tracing::warn!(
                encoding = ?self.encoding,
                "could not re-encode the next retrieve round"
            );
            return false;
        };
        self.body = encoded;
        self.json = Some(value);
        true
    }

    /// Send it.
    ///
    /// The time this takes is **to the response head**, not to the last byte:
    /// `reqwest` resolves here once the status and headers arrive and streams
    /// the body lazily afterwards. For a streamed completion that is the right
    /// boundary and not a shortcut — what follows it is the model generating
    /// while this proxy relays, so counting it as provider latency would make
    /// every long answer look like a slow upstream. What the caller gets is the
    /// wait before the provider said anything, which is the half a proxy can
    /// be blamed for being slow on top of.
    ///
    /// A body carrying a `prompt_cache_retention` this proxy added is sent
    /// without it once the upstream has refused it, and the request that is
    /// refused is sent again without it, so the setting can cost a round trip
    /// but never a request. Every other answer comes back as it arrived.
    pub(crate) async fn send(&self) -> reqwest::Result<reqwest::Response> {
        let Some(guard) = &self.retention else {
            return self.send_body(self.body.clone()).await;
        };
        if guard.refused() {
            // A retrieve round extends the body it was given, field and all.
            let body = guard
                .strip(&self.body, self.encoding)
                .unwrap_or_else(|| self.body.clone());
            return self.send_body(body).await;
        }
        let response = self.send_body(self.body.clone()).await?;
        if response.status() != StatusCode::BAD_REQUEST {
            return Ok(response);
        }
        // An error is small; reading it is the only way to know whose it is.
        let status = response.status();
        let version = response.version();
        let headers = response.headers().clone();
        let bytes = response.bytes().await?;
        if retention::names_field(&bytes)
            && let Some(stripped) = guard.strip(&self.body, self.encoding)
        {
            guard.refuse();
            return self.send_body(stripped).await;
        }
        let mut rebuilt = http::Response::new(bytes);
        *rebuilt.status_mut() = status;
        *rebuilt.version_mut() = version;
        *rebuilt.headers_mut() = headers;
        Ok(reqwest::Response::from(rebuilt))
    }

    async fn send_body(&self, body: Bytes) -> reqwest::Result<reqwest::Response> {
        let mut request = self
            .client
            .request(self.method.clone(), &self.url)
            .headers(self.headers.clone())
            .body(body);
        if let Some(timeout) = self.timeout {
            request = request.timeout(timeout);
        }
        let began = Instant::now();
        let result = request.send().await;
        if let Err(error) = &result {
            // Display alone drops the nested transport cause. Keep phase and
            // classification without logging credentials, payloads or URL queries.
            tracing::warn!(
                phase = "send", connect = error.is_connect(), timeout = error.is_timeout(),
                request = error.is_request(), cause = ?std::error::Error::source(error),
                "upstream request failed"
            );
        }
        // Recorded whether or not the provider answered: a request that failed
        // after nine seconds waited nine seconds, and dropping that would make
        // the failures look instant.
        self.waited_us.fetch_add(
            began.elapsed().as_micros().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
        result
    }

    /// A handle on the accumulated provider wait, for the caller that records
    /// it. Cloned before the `Outbound` is moved into the path that sends it.
    pub(crate) fn waited(&self) -> Arc<AtomicU64> {
        self.waited_us.clone()
    }
}
