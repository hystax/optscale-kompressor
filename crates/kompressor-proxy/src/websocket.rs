//! Relaying a websocket upgrade to the upstream that can answer it.
//!
//! # What this fixes
//!
//! Codex opens `/v1/responses` over a websocket first and falls back to HTTPS
//! only when that fails. Without this module the upgrade reached the ordinary
//! forward path, where it is a GET with some curious headers: the switching
//! handshake never happens, the GET goes upstream on its own, and the provider
//! answers `405 Method Not Allowed` because that path takes POSTs. Measured
//! against a wrapped codex, which then logged seven failed connects, waited
//! about eight seconds, and fell back — every session, to reach a transport it
//! would have used immediately if the proxy had simply got out of the way.
//!
//! The eight seconds are the small part. The fallback is not ours and we do not
//! control it: a client that stops falling back, or ships with it off, talks to
//! the provider over a transport this proxy cannot see. Nothing is compressed,
//! every health check stays green, and no error is printed anywhere — the exact
//! silent failure the agent catalogue is written to avoid, reached through the
//! transport layer instead of through a variable name.
//!
//! # What it does, and what it does not
//!
//! Requests are compressed; everything else is relayed byte for byte. A
//! `response.create` frame on its way upstream goes through the same engine the
//! HTTP path uses — see `rewrite_frame` — and every other frame, in either
//! direction, is carried untouched.
//!
//! The retrieve loop is **not** implemented here. Over HTTP the proxy holds a
//! whole response, spots an `optscale_kompressor_retrieve` call and answers it inside the
//! round trip, invisibly. Over a websocket the turn is a live event stream, so
//! intercepting a tool call means participating in the protocol rather than
//! relaying it — a second implementation of CCR rather than a port of the
//! first. So this transport does not offer the tool, because a call nobody
//! answers is worse than no tool at all.
//!
//! **Content still comes back, by another route.** `crate::piggyback` watches
//! the model's output for a handle this connection elided and appends the
//! original to the client's next `response.create`. Not mid-turn and not a tool
//! — the restore lands on the following frame — but it is the difference
//! between a marker the model can recover from and one it cannot. Handles also
//! resolve through the other surfaces: the HTTP proxy, the API,
//! `kompressor mcp serve`.

use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use http::StatusCode;
use http::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use kompressor_core::RequestInfo;
use kompressor_core::auth::Identity;
use kompressor_core::config::RetrieveTool;
use kompressor_engine::estimate_tokens;
use kompressor_engine::messages::PrefixCache;
use kompressor_engine::options::EngineOptionsPatch;

/// Every frame on this transport is an OpenAI Responses request, so it runs
/// under `proxy.providers.openai`.
const PROVIDER: &str = "openai";
use kompressor_engine::refs::{RefId, refs_in};
use kompressor_stats::events::{Event, ProxyRequestEvent, Surface};
use kompressor_wire::{ApiFamily, walker_for};

use crate::ProxyState;
use crate::mode::ProxyMode;
use crate::piggyback::Piggyback;
use tokio_tungstenite::tungstenite::Message as Upstream;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

/// Announces on the handshake that this turn was relayed, not compressed.
///
/// The HTTP path reports what it did in `x-kompressor-mode`; a transport that
/// reports nothing would let a session look compressed because the proxy was in
/// the path at all.
pub const UPSTREAM_HEADER: &str = "x-kompressor-websocket";

/// Does this request want to become a websocket?
///
/// Both headers are required and both are matched case-insensitively on the
/// value as well as the name: `Connection` is a comma-separated list that may
/// read `keep-alive, Upgrade`, and browsers and CLIs disagree about the casing
/// of `websocket`. Matching only `connection == "upgrade"` misses the common
/// case, which is worse than not looking at all — the request would fall
/// through to the GET path that produced the 405 this module exists to remove.
pub fn is_upgrade(headers: &HeaderMap) -> bool {
    let connection_upgrades = headers
        .get(http::header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        });
    let wants_websocket = headers
        .get(http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("websocket"));
    connection_upgrades && wants_websocket
}

/// The upstream websocket URL for an already-routed HTTP one.
///
/// Only the scheme changes. The path and query are the ones routing already
/// decided, which matters under a subscription: the same `/v1/responses` goes
/// to a different host under a different prefix depending on the credential,
/// and re-deriving any of that here would be a second place for it to be wrong.
pub fn upstream_url(http_url: &str) -> String {
    if let Some(rest) = http_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = http_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        http_url.to_string()
    }
}

/// Headers to carry onto the upstream handshake.
///
/// Authorisation has to travel or the upstream refuses the handshake, and it is
/// the same credential the HTTP path forwards. Everything the handshake owns is
/// dropped: `tokio-tungstenite` generates its own `sec-websocket-key`, and
/// forwarding the client's `host`, `connection` or `upgrade` would describe our
/// listener to a server that is not it.
/// `upstream` applies the same credential policy the forward path does.
/// Without it this transport answered to a different set of rules than
/// everything else on the same mount: it forwarded the client's
/// `Authorization` even where `forward_auth = false` said to strip it, and
/// never supplied the replacement credential the operator configured — so a
/// deployment holding its own provider key worked over HTTP and was refused
/// over websocket, or leaked the client's key to an upstream configured never
/// to see it.
pub fn handshake_headers(
    from: &HeaderMap,
    upstream: &kompressor_core::config::Upstream,
) -> Vec<(HeaderName, HeaderValue)> {
    const CARRIED: &[&str] = &[
        "authorization",
        "openai-organization",
        "openai-project",
        "openai-beta",
        "chatgpt-account-id",
        "originator",
        "user-agent",
        "session_id",
    ];
    const CREDENTIAL: [&str; 3] = ["authorization", "x-api-key", "api-key"];
    let mut out: Vec<(HeaderName, HeaderValue)> = CARRIED
        .iter()
        .filter(|name| upstream.forward_auth || !CREDENTIAL.contains(name))
        .filter_map(|name| {
            let value = from.get(*name)?;
            let name = HeaderName::from_bytes(name.as_bytes()).ok()?;
            Some((name, value.clone()))
        })
        .collect();
    // Configured headers last, and they replace rather than accompany: an
    // upstream that supplies its own credential must not also receive the
    // client's.
    for (name, value) in &upstream.headers {
        let (Ok(name), Ok(value)) = (name.parse::<HeaderName>(), HeaderValue::from_str(value))
        else {
            continue;
        };
        out.retain(|(existing, _)| existing != name);
        out.push((name, value));
    }
    out
}

/// Accept the upgrade and pump frames until either side hangs up.
#[allow(clippy::too_many_arguments)]
pub async fn relay(
    state: ProxyState,
    identity: Identity,
    mode: ProxyMode,
    provider: String,
    upgrade: WebSocketUpgrade,
    url: String,
    headers: Vec<(HeaderName, HeaderValue)>,
    request_id: String,
) -> Response {
    let mut response = upgrade.on_upgrade(move |client| async move {
        let context = Session {
            state,
            identity,
            mode,
            provider,
        };
        let started = Instant::now();
        let time_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_millis() as i64)
            .unwrap_or(0);

        let failure = pump(client, &url, headers, &request_id, &context)
            .await
            .err()
            .map(|error| {
                // A relay failure is the session ending, not a request failing:
                // the handshake has already been answered, so there is no status
                // code left to send.
                tracing::warn!(%error, request_id = %request_id, "websocket relay ended");
                error.to_string()
            });

        // One row for the connection, because the connection is what has an
        // identity here: every frame on it shares `request_id`, and the HTTP
        // path's unit — a request — does not exist on this transport.
        //
        // Without it a relayed session is invisible to every per-request
        // report. `kompressor stats summary`, the HTML report and the Harbor
        // bench all read `proxy_requests`, so traffic over this transport
        // contributed compression events and nothing else: no duration, no
        // tenant attribution, and nowhere for a relay failure to land except a
        // log line nobody is tailing. That is the silent failure this module's
        // own doc exists to argue against, reached through the stats layer.
        context
            .state
            .core
            .stats()
            .send(Event::ProxyRequest(ProxyRequestEvent {
                time_ms,
                tenant: context.identity.tenant.as_str().to_string(),
                request_id: request_id.clone(),
                api_family: ApiFamily::OpenAiResponses.as_str().to_string(),
                model: None,
                upstream: url.clone(),
                // The handshake succeeded; 101 is what the client was told, and
                // it keeps relayed sessions separable from forwarded 200s.
                status: Some(101),
                stream: true,
                // **Null, not zero.** The relay never reads a `response.completed`
                // frame, so it does not know what the provider billed. Zero
                // would read as "this cost nothing" and quietly deflate every
                // cost report; null reads as "not measured from here", which is
                // the truth. Same reasoning as `opaque` in the skip taxonomy.
                prompt_tokens: None,
                completion_tokens: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                cache_write_1h_tokens: None,
                shadow_prompt_tokens: None,
                conversation: None,
                // The retrieve tool is deliberately not offered on this
                // transport, so these are genuinely zero rather than unknown.
                retrieve_rounds: 0,
                retrieve_tokens_injected: 0,
                upstream_ms: None,
                total_ms: Some(started.elapsed().as_secs_f64() * 1000.0),
                error: failure,
                strip_usage: false,
            }));
    });
    response.headers_mut().insert(
        HeaderName::from_static(UPSTREAM_HEADER),
        HeaderValue::from_static("relayed"),
    );
    response
}

/// One frame from the client, on its way upstream: compressed, or declined.
///
/// Two things can change it. Fragments the model asked back are appended, and
/// whatever is left is compressed. Either alone is reason enough to rewrite the
/// frame; with neither, the original bytes go out untouched.
///
/// Declining always means the same thing here as everywhere else in this crate:
/// forward what the client sent. Every early return below is a decline.
///
/// # Why this shape and not the HTTP one
///
/// The request is a whole JSON document in a single text frame — measured on a
/// live 16-payload codex sweep, where all 18 client frames were one complete
/// `response.create` each, with no incremental assembly to reassemble. That is
/// what makes this tractable at all, and it is why the walker can be handed the
/// frame directly: apart from the `type` field, a `response.create` *is* a
/// Responses body, so [`ApiFamily::OpenAiResponses`] walks it unchanged.
///
/// It is also worth compressing. In that capture, 95.5% of request bytes were
/// `input`, and inside it 85.7% of the whole request was `custom_tool_call_output`
/// — the payload class the engine is built for. The Claude Code case is the
/// mirror image, where 83% was tool *schemas* no walker can touch.
///
/// The retrieve tool is deliberately not injected. Over HTTP the proxy holds a
/// whole response, answers a `optscale_kompressor_retrieve` call and runs a second round
/// invisibly; over a websocket the turn is a live event stream, so answering one
/// means participating in the protocol. Injecting a tool this transport cannot
/// service would hand the model a call nobody answers — the defect already met
/// once on the HTTP path, deliberately not reintroduced here.
async fn rewrite_frame(
    session: &Session,
    text: &str,
    request_id: &str,
    piggyback: &Mutex<Piggyback>,
) -> Option<String> {
    let Ok(mut value) = serde_json::from_str::<Value>(text) else {
        tracing::debug!("websocket frame: not JSON");
        return None;
    };
    // The one frame shape this understands. Every other event on this socket —
    // cancellations, pings, whatever a future client adds — is relayed
    // untouched rather than guessed at.
    let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
    if kind != "response.create" {
        tracing::debug!(kind, "websocket frame: not a request, relaying untouched");
        return None;
    }

    // Three steps, and the order of all three is load-bearing.
    //
    // What the turn is about is read from the client's own text, before the
    // engine touches it, and weighed only against handles from *earlier*
    // frames. Read afterwards it would be weighed against handles minted on
    // this very frame, and a compressed log still carries the samples the
    // crusher kept — which have every word in common with the fragment behind
    // them, so the relay would restore what it had just elided, on the frame
    // that elided it.
    //
    // The append comes last for the opposite reason. A fragment put in before
    // the engine runs is content the engine weighs up, and on a frame with
    // enough else in it, elides straight back out under a fresh handle: the
    // span minted around the wrapper text is not the one `keep_inline`
    // protects. Appending afterwards cannot be undone and costs nothing — the
    // client does not know about the item and never sends it back, so it
    // crosses the wire exactly once.
    note_what_the_turn_asks_for(session, &value, piggyback);
    let compressed = compress_value(
        &session.state,
        &session.identity,
        session.mode,
        &mut value,
        request_id,
        &session.provider,
        piggyback,
    )
    .await;
    let restored = restore_fragments(session, &mut value, request_id, piggyback);

    if restored == 0 && !compressed {
        return None;
    }
    serde_json::to_string(&value).ok()
}

/// Queue anything this turn reads as being about a fragment already elided.
///
/// The second of the two signals. Naming the handle is the first and the exact
/// one; this covers the model that describes what it wants instead — the same
/// two-words-in-common test the HTTP side uses to decide what to keep inline.
///
/// Must run against the client's own text, before the engine rewrites it.
fn note_what_the_turn_asks_for(session: &Session, value: &Value, piggyback: &Mutex<Piggyback>) {
    if session
        .state
        .core
        .config_for(Some(PROVIDER))
        .proxy
        .retrieve_tool
        == RetrieveTool::Never
    {
        return;
    }
    let mut held = piggyback
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let outstanding = held.outstanding();
    if outstanding.is_empty() {
        return;
    }
    let query = newest_turn_text(value);
    if query.is_empty() {
        return;
    }
    for id in outstanding {
        // `peek`, not `retrieve`: reading a fragment to decide whether the
        // model wants it is not the model asking, and must not look like use to
        // the retention clock or like a retrieval to the statistics.
        let Some(fragment) = session.state.core.peek(&session.identity, &id) else {
            continue;
        };
        if crate::handler::query_is_about(&query, &fragment) {
            held.also_wants(id);
        }
    }
}

/// Append the fragments this connection owes the model.
///
/// Returns how many were appended. Nothing is appended twice, and a fragment
/// the store can no longer produce is dropped rather than announced: the model
/// asked for text, and a note saying the text is gone is worse than silence.
fn restore_fragments(
    session: &Session,
    value: &mut Value,
    request_id: &str,
    piggyback: &Mutex<Piggyback>,
) -> usize {
    let config = session.state.core.config_for(Some(PROVIDER));
    // The operator's switch for the HTTP retrieve tool governs here too. There
    // is no second knob for a second transport: "do not give elided content
    // back to the model" is one decision, however it is carried out.
    if config.proxy.retrieve_tool == RetrieveTool::Never {
        return 0;
    }

    let due = piggyback
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take_pending();
    if due.is_empty() {
        return 0;
    }

    let model = value
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string);
    let budget = config.proxy.retrieve_max_tokens_per_request;
    let mut body = String::new();
    let mut restored = 0usize;
    let mut refused = 0usize;
    for id in due {
        // Priced before it is fetched, so a fragment the budget cannot afford
        // never files a retrieval event for content the model does not receive.
        // `peek` is the read that does not count as the model asking.
        let Some(preview) = session.state.core.peek(&session.identity, &id) else {
            continue;
        };
        // Priced with the unauthored role, as `retrieval_events.returned_tokens`
        // is. The estimator scales authored prose by 0.775, and a fragment is
        // elided tool output — a command printed it, nobody wrote it — so the
        // prose discount is wrong twice over: wrong about the content, and
        // wrong against the number the budget is checked by afterwards. Priced
        // as "developer" the budget admitted 20,844 tokens against a limit of
        // 16,384 on a measured sweep, which is 16,384 / 0.775 almost exactly.
        let cost = estimate_tokens(&preview, "");
        let affordable = piggyback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .may_spend(cost, budget);
        if !affordable {
            refused += 1;
            // Filed per handle, and outside the lock the answer was read under.
            // The warning below says how many were withheld and the operator
            // cannot act on a count alone: tuning the budget means knowing
            // which fragments it keeps turning away, and only a row carries
            // the handle.
            session.state.core.record_refusal(
                &session.identity,
                &id.to_string(),
                &RequestInfo {
                    request_id,
                    api_family: Some(ApiFamily::OpenAiResponses.as_str()),
                    model: model.as_deref(),
                    provider: Some(&session.provider),
                },
                Surface::Proxy,
            );
            continue;
        }
        let got = session.state.core.retrieve(
            &session.identity,
            &id.to_string(),
            None,
            &RequestInfo {
                request_id,
                api_family: Some(ApiFamily::OpenAiResponses.as_str()),
                model: model.as_deref(),
                provider: Some(&session.provider),
            },
            Surface::Proxy,
        );
        let Some(content) = got.content else {
            tracing::debug!(handle = %id, expired = got.expired, "nothing to restore");
            continue;
        };
        piggyback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .spend(estimate_tokens(&content, ""));
        body.push_str(&format!("\n[ref:{id}]\n{content}\n"));
        restored += 1;
    }
    if refused > 0 {
        // Said out loud. A fragment withheld looks exactly like one the model
        // never asked for, and the difference is the whole reason the budget
        // exists — an operator tuning it has to be able to see it bind.
        tracing::warn!(
            request_id = %request_id,
            refused,
            budget,
            spent = piggyback
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .spent(),
            "withheld fragments: this connection has spent its retrieval budget"
        );
    }
    if restored == 0 {
        return 0;
    }

    let Some(input) = value.get_mut("input").and_then(Value::as_array_mut) else {
        tracing::debug!("websocket frame: no input array to restore into");
        return 0;
    };
    input.push(serde_json::json!({
        "type": "message",
        "role": "developer",
        "content": [{
            "type": "input_text",
            "text": format!(
                "<kompressor_restored>\nContent below was elided from earlier \
                 context and replaced with a handle. You referred to the \
                 handle, so the original is restored here verbatim — this is \
                 the text itself, not a summary of it.\n{body}\
                 </kompressor_restored>"
            ),
        }],
    }));
    tracing::info!(
        request_id = %request_id,
        restored,
        "restored elided fragments into a websocket request frame"
    );
    restored
}

/// The newest turn in a `response.create` — its last input item.
///
/// The last item, not all of them, and not the raw frame. Two reasons, both
/// measured on a real session. The frame itself is no use: its first one is
/// 67 KB of which 46 KB is the `additional_tools` tree, so only the keys that
/// carry conversation are descended into. And the whole of `input` is no use
/// either: the second frame of that session holds 10,754 characters of text,
/// nearly all standing developer instructions, against 196 for the turn alone.
/// Weighed against that much prose almost any fragment shares two words with
/// the "query", and everything elided looks wanted. The newest item is the new
/// thing being said, which is the same thing the HTTP side reads.
fn newest_turn_text(value: &Value) -> String {
    fn collect(value: &Value, into: &mut String) {
        match value {
            Value::String(text) => {
                into.push_str(text);
                into.push(' ');
            }
            Value::Array(items) => items.iter().for_each(|item| collect(item, into)),
            Value::Object(fields) => {
                for (key, field) in fields {
                    if matches!(key.as_str(), "text" | "content" | "output" | "input") {
                        collect(field, into);
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = String::new();
    if let Some(newest) = value
        .get("input")
        .and_then(Value::as_array)
        .and_then(|items| items.last())
    {
        collect(newest, &mut out);
    }
    out
}

/// Feed one upstream frame to the connection's state, if it carries model text.
///
/// The model's own output is the only place a request for an elided fragment
/// can appear on this transport, and it arrives as deltas: `delta` on both
/// `response.output_text.delta` and `response.custom_tool_call_input.delta`.
fn observe_upstream(text: &str, piggyback: &Mutex<Piggyback>) {
    // A cheap gate before parsing. 471 frames crossed in the session this was
    // written against, and all but a handful carry no model text at all.
    if !text.contains(".delta\"") {
        return;
    }
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return;
    };
    if !value
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.ends_with(".delta"))
    {
        return;
    }
    let Some(delta) = value.get("delta").and_then(Value::as_str) else {
        return;
    };
    piggyback
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .saw_output(delta);
}

async fn compress_value(
    state: &ProxyState,
    identity: &Identity,
    mode: ProxyMode,
    value: &mut Value,
    request_id: &str,
    provider: &str,
    piggyback: &Mutex<Piggyback>,
) -> bool {
    let config = state.core.config_for(Some(PROVIDER));
    let proxy = &config.proxy;
    // Every decline says why. The failure mode of this whole path is silence —
    // a frame that is not compressed looks exactly like one that had nothing to
    // compress — so the reason is logged rather than inferred.
    if mode.is_off() || !proxy.enabled {
        tracing::debug!(mode = mode.as_str(), "websocket frame: compression is off");
        return false;
    }

    let family = ApiFamily::OpenAiResponses;
    // Walked and dropped before the engine runs: the walker is a boxed trait
    // object that is not `Send`, and holding one across the compression await
    // would make this future unspawnable.
    let Some(mut walk) = walker_for(family).walk(value) else {
        tracing::debug!("websocket frame: the walker did not recognise the shape");
        return false;
    };
    let model = walk.model.clone();

    let before = crate::handler::compressible_tokens(&walk);
    if before < proxy.min_tokens {
        tracing::debug!(
            tokens = before,
            min_tokens = proxy.min_tokens,
            "websocket frame: below proxy.min_tokens"
        );
        return false;
    }

    let options = state.core.options_for_provider(
        identity,
        Some(PROVIDER),
        Some(&EngineOptionsPatch {
            mode: mode.engine_mode(),
            enable_stale_read: (!proxy.stale_read).then_some(false),
            ..Default::default()
        }),
    );
    let messages = std::mem::take(&mut walk.messages);
    // Taken before the await, not inside the call: a lock guard born in an
    // argument list lives to the end of the statement, and that statement
    // suspends. Holding it across the await makes the whole relay future
    // non-`Send` and unspawnable.
    let served = piggyback
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .served();
    let Some((messages, saved)) = crate::handler::compress_with_deadline(
        state,
        identity,
        messages,
        walk.frozen_message_count,
        // Warm, because this path tracks no prefix at all: it compresses one
        // frame and keeps no cross-turn state. The two mistakes do not cost the
        // same — guessing cold rewrites a live prefix and the provider re-reads
        // everything after the first changed byte at full price, while guessing
        // warm only leaves a saving unclaimed.
        PrefixCache::Warm,
        &options,
        request_id,
        family,
        model.as_deref(),
        provider,
        // What this connection has already handed back. Eliding it again would
        // undo the restore on the very next frame, and the model would be left
        // chasing the same handle round the loop.
        served.clone(),
        // Not a cost decision here, so it is not priced like one. On the HTTP
        // path a refused promotion costs a retrieve round and the model asks
        // again; on this one there is no tool to ask with, so re-eliding a
        // fragment already handed back destroys it. `u32::MAX` demands clears
        // any break the engine can weigh.
        served.into_iter().map(|id| (id, u32::MAX)).collect(),
        // Irrelevant here, since the demands above clear any bar — but the
        // measured rate is the honest value to pass, not a placeholder that
        // would start lying the day the exemption above is reconsidered.
        state.prefixes.cache_hit_per_mille(),
        // Unpriced for the same reason: the demands above clear any bar.
        0,
        None,
        // No conversation on this transport, so nothing to pin against. The
        // `u32::MAX` demands above already make every promotion here permanent.
        None,
        proxy.compress_timeout,
    )
    .await
    else {
        return false;
    };
    walk.messages = messages;

    let rewritten = kompressor_wire::apply_walk(value, &walk);
    let after = crate::handler::compressible_tokens(&walk);
    // Same guard the HTTP path keeps: the engine checks per slot, and the total
    // is checked here, because a frame that grew is one we should never have
    // touched.
    if rewritten == 0 || after >= before {
        tracing::debug!(
            rewritten,
            before,
            after,
            "websocket frame: nothing was removed"
        );
        return false;
    }

    // Remember the handles just put in front of the model. This is the whole
    // basis for answering one back later: a handle the relay did not create is
    // not a question it can answer.
    let elided: Vec<RefId> = walk
        .messages
        .iter()
        .flat_map(|message| refs_in(&message.content.joined_text()))
        .collect();
    let handles = elided.len();
    piggyback
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .note_elided(elided);

    tracing::info!(
        request_id = %request_id,
        tokens_saved = saved,
        handles,
        "compressed a websocket request frame"
    );
    true
}

/// Where to record relayed frames, when `KOMPRESSOR_WS_CAPTURE` names a file.
///
/// A debugging aid and not a feature: it answers the one question that decides
/// whether this transport can be compressed at all — whether a request arrives
/// as a single JSON message or is built up incrementally — and that cannot be
/// answered by reading anyone's protocol documentation.
///
/// Off unless the variable is set, and it is a path rather than a boolean
/// because **the file holds the conversation**: prompts, tool output, the
/// model's replies. It is the same content the proxy already forwards, but
/// written to disk it outlives the request, so the operator picks where.
fn capture_path() -> Option<std::path::PathBuf> {
    std::env::var_os("KOMPRESSOR_WS_CAPTURE").map(std::path::PathBuf::from)
}

/// Append one frame to the capture file, if capturing.
///
/// Best-effort: a capture that cannot be written must never break the session
/// it is observing, so every error here is dropped.
fn capture(path: &Option<std::path::PathBuf>, direction: &str, kind: &str, bytes: &[u8]) {
    let Some(path) = path else { return };
    let record = serde_json::json!({
        "at_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        "dir": direction,
        "kind": kind,
        "bytes": bytes.len(),
        // Lossy on purpose: a binary frame is exactly the case worth knowing
        // about, and it must not be able to abort the capture.
        "text": String::from_utf8_lossy(bytes),
    });
    // Through the shared writer, which holds a lock. Both pump directions run
    // concurrently by construction, so appending from each of them without one
    // interleaves a client frame with an upstream frame mid-line and the file
    // stops being JSON -- the same hazard the HTTP capture hit, found there by
    // a test and fixed in both places at once.
    crate::bodycapture::append_line(path, &record.to_string());
}

/// What the relay needs to compress a frame, carried for the session's life.
struct Session {
    state: ProxyState,
    identity: Identity,
    mode: ProxyMode,
    provider: String,
}

/// Dial the upstream and move frames both ways until one side closes.
async fn pump(
    client: WebSocket,
    url: &str,
    headers: Vec<(HeaderName, HeaderValue)>,
    request_id: &str,
    session: &Session,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut request =
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(url)?;
    for (name, value) in headers {
        request.headers_mut().insert(name, value);
    }

    let (upstream, _response) = tokio_tungstenite::connect_async(request).await?;
    tracing::info!(request_id = %request_id, %url, "websocket relay open");

    let (mut client_tx, mut client_rx) = client.split();
    let (mut upstream_tx, mut upstream_rx) = upstream.split();
    let recording = capture_path();
    let recording_up = recording.clone();

    // One per connection, shared by both directions: the frames going upstream
    // are where fragments are restored, and the frames coming back are the only
    // place a request for one can appear.
    let piggyback = Arc::new(Mutex::new(Piggyback::default()));
    let piggyback_up = Arc::clone(&piggyback);

    // Both directions run until either finishes. `select!` rather than joining
    // them: when one side closes, the other has nobody left to talk to, and
    // holding it open leaks a task per abandoned session.
    let to_upstream = async {
        while let Some(message) = client_rx.next().await {
            let message = match message {
                Ok(message) => message,
                Err(error) => {
                    tracing::debug!(%error, "client frame error");
                    break;
                }
            };
            // What arrived, before this relay touches it. Labelled for what it
            // is: the old name said `client->upstream`, which is what the frame
            // was *for* rather than what it was, and left a capture with no
            // record of the bytes actually sent -- the wrong side entirely for
            // asking which byte of a prefix moved.
            match &message {
                Message::Text(text) => {
                    capture(&recording_up, "client->proxy", "text", text.as_bytes())
                }
                Message::Binary(data) => capture(&recording_up, "client->proxy", "binary", data),
                _ => {}
            }
            // The only place this relay is not a relay. A frame that does not
            // compress, or is not a shape we know, goes out exactly as it came.
            let message = match &message {
                Message::Text(text) => {
                    match rewrite_frame(session, text.as_str(), request_id, &piggyback_up).await {
                        Some(rewritten) => Message::Text(rewritten.into()),
                        None => message,
                    }
                }
                _ => message,
            };
            // And what leaves, after any rewrite. Recorded whether or not the
            // frame changed: a diff needs both sides of every turn, and "the
            // record is missing because nothing happened" is indistinguishable
            // from "the record is missing because the capture dropped it".
            match &message {
                Message::Text(text) => {
                    capture(&recording_up, "proxy->upstream", "text", text.as_bytes())
                }
                Message::Binary(data) => capture(&recording_up, "proxy->upstream", "binary", data),
                _ => {}
            }
            if upstream_tx.send(to_tungstenite(message)).await.is_err() {
                break;
            }
        }
        let _ = upstream_tx.close().await;
    };

    let to_client = async {
        while let Some(message) = upstream_rx.next().await {
            let message = match message {
                Ok(message) => message,
                Err(error) => {
                    tracing::debug!(%error, "upstream frame error");
                    break;
                }
            };
            // `Frame` is a raw frame tungstenite only produces for callers that
            // asked for them; there is nothing meaningful to relay, so it is
            // dropped rather than guessed at.
            match &message {
                Upstream::Text(text) => {
                    capture(&recording, "upstream->client", "text", text.as_bytes());
                    observe_upstream(text.as_str(), &piggyback);
                }
                Upstream::Binary(data) => capture(&recording, "upstream->client", "binary", data),
                _ => {}
            }
            let Some(message) = from_tungstenite(message) else {
                continue;
            };
            if client_tx.send(message).await.is_err() {
                break;
            }
        }
        let _ = client_tx.close().await;
    };

    tokio::select! {
        _ = to_upstream => {},
        _ = to_client => {},
    }
    tracing::info!(request_id = %request_id, "websocket relay closed");
    Ok(())
}

/// axum's frame to tungstenite's.
fn to_tungstenite(message: Message) -> Upstream {
    match message {
        Message::Text(text) => Upstream::Text(text.as_str().into()),
        Message::Binary(data) => Upstream::Binary(data),
        Message::Ping(data) => Upstream::Ping(data),
        Message::Pong(data) => Upstream::Pong(data),
        Message::Close(frame) => Upstream::Close(frame.map(|frame| {
            tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: CloseCode::from(frame.code),
                reason: frame.reason.as_str().into(),
            }
        })),
    }
}

/// tungstenite's frame to axum's. `None` for one that cannot be relayed.
fn from_tungstenite(message: Upstream) -> Option<Message> {
    Some(match message {
        Upstream::Text(text) => Message::Text(text.as_str().into()),
        Upstream::Binary(data) => Message::Binary(data),
        Upstream::Ping(data) => Message::Ping(data),
        Upstream::Pong(data) => Message::Pong(data),
        Upstream::Close(frame) => Message::Close(frame.map(|frame| CloseFrame {
            code: frame.code.into(),
            reason: frame.reason.as_str().into(),
        })),
        Upstream::Frame(_) => return None,
    })
}

/// The refusal when an upgrade arrives that cannot be relayed.
///
/// Says which transport failed, because the alternative is the client reading a
/// generic error and retrying the handshake six more times.
pub fn refused(request_id: &str, why: &str) -> Response {
    let body = serde_json::json!({
        "error": {
            "type": "kompressor_websocket_refused",
            "message": why,
            "request_id": request_id,
        }
    });
    let mut response = Response::new(axum::body::Body::from(body.to_string()));
    *response.status_mut() = StatusCode::BAD_GATEWAY;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_turn_is_the_query_not_the_whole_frame() {
        // The first frame of a codex session is ~67 KB, nearly all of it the
        // `additional_tools` item: a namespace tree whose `exec` member carries
        // a 27 KB description. Reading that as the turn's text would make every
        // query match every fragment, and restore the lot.
        let frame = serde_json::json!({
            "type": "response.create",
            "input": [
                {"type": "additional_tools", "role": "developer", "tools": [{
                    "type": "namespace", "name": "functions", "description": "",
                    "tools": [{"type": "custom", "name": "exec",
                               "description": "Run JavaScript code to orchestrate tool calls"}],
                }]},
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "what does reconcile_07 do"}]},
            ],
        });
        let text = newest_turn_text(&frame);
        assert!(
            text.contains("reconcile_07"),
            "the turn is the text: {text:?}"
        );
        assert!(
            !text.contains("JavaScript"),
            "a tool description is not something the model asked: {text:?}"
        );
    }

    #[test]
    fn only_a_delta_frame_feeds_the_connection_state() {
        let piggyback = Mutex::new(Piggyback::default());
        let id = kompressor_engine::refs::ref_for("a fragment");
        piggyback.lock().unwrap().note_elided(std::iter::once(id));

        // The handle appears in a frame that is not a delta — a completed item
        // being echoed, say. The model has not said anything new.
        observe_upstream(
            &serde_json::json!({"type": "response.output_item.done",
                                "delta": format!("ref:{id}")})
            .to_string(),
            &piggyback,
        );
        assert!(piggyback.lock().unwrap().take_pending().is_empty());

        observe_upstream(
            &serde_json::json!({"type": "response.output_text.delta",
                                "delta": format!("I need ref:{id}")})
            .to_string(),
            &piggyback,
        );
        assert_eq!(piggyback.lock().unwrap().take_pending(), vec![id]);
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn an_upgrade_is_recognised_however_the_client_spells_it() {
        assert!(is_upgrade(&headers(&[
            ("connection", "Upgrade"),
            ("upgrade", "websocket")
        ])));
        // The case this proxy actually meets: `Connection` is a list, and a
        // client that also wants keep-alive sends both tokens.
        assert!(is_upgrade(&headers(&[
            ("connection", "keep-alive, Upgrade"),
            ("upgrade", "WebSocket")
        ])));
    }

    #[test]
    fn an_ordinary_request_is_not_mistaken_for_one() {
        assert!(!is_upgrade(&headers(&[("connection", "keep-alive")])));
        // Half a handshake is not a handshake. Relaying on `upgrade` alone
        // would hijack any request carrying the header for another protocol.
        assert!(!is_upgrade(&headers(&[("upgrade", "websocket")])));
        assert!(!is_upgrade(&headers(&[("connection", "upgrade")])));
        assert!(!is_upgrade(&HeaderMap::new()));
    }

    #[test]
    fn only_the_scheme_changes() {
        // The path and query come from routing, which under a subscription has
        // already sent this to a different host under a different prefix.
        assert_eq!(
            upstream_url("https://chatgpt.com/backend-api/codex/responses?stream=1"),
            "wss://chatgpt.com/backend-api/codex/responses?stream=1"
        );
        assert_eq!(
            upstream_url("http://127.0.0.1:9/v1/responses"),
            "ws://127.0.0.1:9/v1/responses"
        );
    }

    /// The handshake obeys the same credential policy as the forward path.
    ///
    /// It used to read only the client's headers, so `forward_auth = false`
    /// was ignored and the configured replacement credential never sent: the
    /// same mount authenticated one way over HTTP and another over websocket.
    #[test]
    fn the_handshake_applies_the_upstreams_credential_policy() {
        let client = headers(&[
            ("authorization", "Bearer the-clients-key"),
            ("chatgpt-account-id", "acct"),
        ]);
        let upstream = kompressor_core::config::Upstream {
            forward_auth: false,
            headers: [(
                "Authorization".to_string(),
                "Bearer the-operators-key".to_string(),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        let carried = handshake_headers(&client, &upstream);
        let auth: Vec<_> = carried
            .iter()
            .filter(|(name, _)| name.as_str() == "authorization")
            .map(|(_, value)| value.to_str().unwrap().to_string())
            .collect();
        assert_eq!(
            auth,
            vec!["Bearer the-operators-key".to_string()],
            "exactly the configured credential, and not alongside the client's"
        );
        // Everything that is not a credential still travels.
        assert!(
            carried
                .iter()
                .any(|(name, _)| name.as_str() == "chatgpt-account-id")
        );
    }

    /// And with no replacement configured, `forward_auth = false` strips.
    #[test]
    fn the_handshake_strips_the_credential_when_told_to() {
        let carried = handshake_headers(
            &headers(&[("authorization", "Bearer the-clients-key")]),
            &kompressor_core::config::Upstream {
                forward_auth: false,
                ..Default::default()
            },
        );
        assert!(
            !carried
                .iter()
                .any(|(name, _)| name.as_str() == "authorization"),
            "forward_auth = false must strip it here as it does over HTTP"
        );
    }

    #[test]
    fn the_credential_travels_and_the_handshake_does_not() {
        let carried = handshake_headers(
            &headers(&[
                ("authorization", "Bearer token"),
                ("chatgpt-account-id", "acct"),
                ("host", "127.0.0.1:8817"),
                ("connection", "Upgrade"),
                ("upgrade", "websocket"),
                ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ]),
            &kompressor_core::config::Upstream {
                forward_auth: true,
                ..Default::default()
            },
        );
        let names: Vec<String> = carried.iter().map(|(n, _)| n.to_string()).collect();

        // Without this the upstream refuses the handshake outright.
        assert!(names.contains(&"authorization".to_string()));
        assert!(names.contains(&"chatgpt-account-id".to_string()));
        // These describe our listener, not the upstream, and tungstenite
        // generates its own key.
        for owned in ["host", "connection", "upgrade", "sec-websocket-key"] {
            assert!(
                !names.contains(&owned.to_string()),
                "{owned} belongs to the handshake and must not be forwarded"
            );
        }
    }
}
