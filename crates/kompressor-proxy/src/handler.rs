//! The request pipeline: §12.3, in order.
//!
//! Every step can decline, and declining always means the same thing — forward
//! what the client sent. That is the property the whole crate is organised
//! around, so the pipeline is written as a sequence that produces *what to
//! send*, rather than as a chain that can fail into an error response. The only
//! statuses this proxy originates are a 404 for a path with no upstream, a 413
//! for a body over the cap, and a 502 when the upstream could not be reached at
//! all: a provider that answered is always relayed, status and body, exactly as
//! it answered.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{FromRequestParts, State};
use axum::response::Response;
use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use serde_json::{Value, json};
use tracing::Instrument;

use kompressor_core::auth::{Credentials, Identity};
use kompressor_core::config::{
    CacheTtl, FrozenPrefix, ProxyConfig, RetrieveExecution, RetrieveTool,
};
use kompressor_core::{AppCore, RequestInfo};
use kompressor_engine::estimate_tokens;
use kompressor_engine::messages::{Message, PrefixCache};
use kompressor_engine::options::EngineOptionsPatch;
use kompressor_engine::refs::{RefId, refs_in};
use kompressor_stats::events::{Event, ProxyRequestEvent, Surface};
use kompressor_wire::{ApiFamily, Walk, detect_family, walker_for};

use crate::bodycapture;
use crate::bodycodec;
use crate::failopen::Stage;
use crate::mode::ProxyMode;
use crate::prefixcache;
use crate::retrieve::{Budget, Retriever};
use crate::stream::{ClientChunk, StreamRelay};
use crate::upstream::Outbound;
use crate::websocket;
use crate::{ProxyState, headers, nonstream, route, tool};

/// Handle one proxied request.
///
/// A thin wrapper so that every log line inside the pipeline — including the
/// ones the engine and the store emit — carries the request id the client can
/// correlate against, without the span being entered across an await.
pub(crate) async fn handle(
    State(state): State<ProxyState>,
    request: axum::extract::Request,
) -> Response {
    let request_id = request_id(request.headers());
    let span = tracing::info_span!("proxy", request_id = %request_id);
    run(state, request, request_id).instrument(span).await
}

async fn run(state: ProxyState, request: axum::extract::Request, request_id: String) -> Response {
    let started = Instant::now();
    let (parts, body) = request.into_parts();
    let config = state.core.config();
    let proxy = &config.proxy;

    let Some(upstream) = route::match_upstream(proxy, parts.uri.path()) else {
        return error_response(
            StatusCode::NOT_FOUND,
            "no upstream is configured for this path",
            &request_id,
        );
    };
    // Classified before routing, not just before header policy: under a
    // subscription the same path belongs to a different host.
    let auth_mode = crate::authmode::classify(&parts.headers);
    let route = route::route_for(
        upstream,
        parts.uri.path(),
        parts.uri.query(),
        auth_mode.is_subscription(),
    );

    // Before the body is read, because there is not one: an upgrade is a GET
    // with a handshake attached. Left to fall through, it reaches the forward
    // path as a plain GET, the switch never happens, and the upstream answers
    // 405 on a path that takes POSTs — which is what a wrapped codex met seven
    // times per session before falling back to HTTPS.
    // Gated, and on by default. The gate is `is_upgrade` and nothing else, so
    // this branch is the *only* thing the flag changes: an ordinary request
    // matches neither required header, falls past it, and takes the same
    // forward path it always did. Both transports are served at once.
    // See `ProxyConfig::websocket_relay` for what relaying costs.
    if proxy.websocket_relay && websocket::is_upgrade(&parts.headers) {
        let url = websocket::upstream_url(&route.url);
        let carried = websocket::handshake_headers(&parts.headers, upstream);
        // Resolved here rather than inside the relay: both read the *handshake*
        // headers, and once the socket is open there are no headers left to
        // read. The tenant and the mode belong to the session, not to a frame.
        let identity = resolve_identity(&state, &parts.headers);
        let mode = resolve_mode(
            &state,
            &parts.headers,
            &identity,
            detect_family(&route.path).map(ApiFamily::provider),
        );
        let provider = upstream.name.clone();
        let mut parts = parts;
        return match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
            Ok(upgrade) => {
                // Requests on this socket are compressed; what is missing is
                // the provider's own token counts, which never arrive because
                // the relay sees no `response.completed`. Recorded at info
                // because a session whose savings cannot be measured should be
                // visible in the log rather than inferred from a silent gap in
                // the cost report.
                tracing::info!(
                    request_id = %request_id,
                    upstream = %route.origin(),
                    "relaying a websocket; requests are compressed, usage is not reported"
                );
                websocket::relay(
                    state.clone(),
                    identity,
                    mode,
                    provider,
                    upgrade,
                    url,
                    carried,
                    request_id,
                )
                .await
            }
            // The headers said websocket and the handshake did not parse.
            // Falling through to the GET path would reproduce the 405.
            Err(error) => {
                tracing::warn!(%error, request_id = %request_id, "malformed websocket upgrade");
                websocket::refused(&request_id, "the websocket handshake could not be read")
            }
        };
    }

    let body = match axum::body::to_bytes(body, proxy.max_body_bytes).await {
        Ok(body) => body,
        Err(error) => {
            // the body cannot be forwarded because it was never fully read;
            // this is the one request-shaped failure the proxy must report
            tracing::warn!(%error, "request body exceeded proxy.max_body_bytes");
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body exceeds the configured maximum",
                &request_id,
            );
        }
    };

    // Decoded before anything looks at it. A client may compress the request
    // body, and codex always does — `content-encoding: zstd` under a
    // `content-type: application/json`, so the type says nothing. Reading the
    // raw bytes there yields a body that will not parse, and the request is
    // forwarded whole while every health check stays green.
    //
    // `wire` keeps what arrived. A request this proxy does not rewrite is
    // forwarded exactly as it came, so a decode that was not needed costs the
    // upstream nothing and a re-encode never has to be byte-identical.
    let encoding = bodycodec::BodyEncoding::from_headers(&parts.headers);
    let wire = body.clone();
    let body = match encoding.decode(&body, proxy.max_body_bytes) {
        Some(decoded) => decoded,
        None => {
            // Unreadable at its stated coding. Nothing here can be parsed, so
            // this takes the same path a body that is not JSON takes.
            state
                .failopen
                .record(Stage::Parse, &"the body could not be decoded");
            body
        }
    };

    // Cheap: `Bytes` is refcounted, and this is what tells a rewritten body
    // from one the engine declined.
    let decoded = body.clone();

    let family = if upstream.api_family.eq_ignore_ascii_case("passthrough") {
        None
    } else {
        detect_family(&route.path)
    };
    let identity = resolve_identity(&state, &parts.headers);
    let mode = resolve_mode(
        &state,
        &parts.headers,
        &identity,
        family.map(ApiFamily::provider),
    );
    tracing::debug!(
        tenant = %identity.tenant.as_str(),
        upstream = %upstream.name,
        mode = mode.as_str(),
        "routing"
    );
    let prepared = prepare(
        &state,
        &identity,
        mode,
        family,
        &parts.method,
        &parts.headers,
        body,
        &request_id,
        upstream.name.as_str(),
    )
    .await;

    // The client is back in this conversation, so whatever was keeping its
    // cache warm has done its job. Cancelled before this request is sent, so a
    // ping cannot race the request it was waiting for.
    if let Some(conversation) = prepared.prefix {
        state.keepwarm.cancel(conversation);
    }

    // Both bodies, before either is re-encoded, when a capture is asked for.
    // Which byte changed is the one question the aggregates cannot answer, and
    // whether it changed on the way in or on the way out is what says who
    // changed it. Off unless `KOMPRESSOR_BODY_CAPTURE` names a file.
    let capturing = bodycapture::path();
    if capturing.is_some() {
        bodycapture::record(&capturing, &request_id, "client->proxy", &decoded);
        bodycapture::record(&capturing, &request_id, "proxy->upstream", &prepared.body);
    }

    let upstream_headers =
        headers::upstream_headers(&parts.headers, upstream, &request_id, auth_mode);
    let keep_warm = keep_warm_for(
        &state,
        family,
        &prepared,
        &route.url,
        &upstream_headers,
        &request_id,
        &identity,
        route.origin(),
    );
    // What actually goes out. Unchanged means unchanged: the bytes that
    // arrived, in the coding they arrived in. Only a body this proxy rewrote
    // is re-encoded, and if that fails the original ships rather than a body
    // whose `content-encoding` lies about it.
    let outbound_body = if prepared.body == decoded {
        // Unchanged: the bytes that arrived, in the coding they arrived in.
        wire
    } else if !encoding.is_encoded() {
        prepared.body.clone()
    } else {
        match encoding.encode(&prepared.body) {
            Some(encoded) => encoded,
            None => {
                state
                    .failopen
                    .record(Stage::Parse, &"the rewritten body could not be re-encoded");
                wire
            }
        }
    };
    let outbound = {
        let mut outbound = Outbound::new(
            state.client.clone(),
            parts.method.clone(),
            route.url.clone(),
            upstream_headers,
            outbound_body,
            encoding,
        );
        if let Some(json) = prepared.json.clone() {
            outbound = outbound.with_json(json);
        }
        if !prepared.stream {
            outbound = outbound.with_timeout(proxy.upstream_read_timeout);
        }
        outbound
    };

    // Server-side execution is the point of the whole design: a client that
    // cannot answer a tool it never declared would otherwise be handed the
    // call, and that is the failure this mode exists to prevent.
    if config.log.payloads {
        tracing::debug!(
            injected = prepared.injected,
            stream = prepared.stream,
            saved_tokens = prepared.saved_tokens,
            execution = ?proxy.retrieve_execution,
            "prepared"
        );
    }
    let retriever = (prepared.injected && proxy.retrieve_execution == RetrieveExecution::Server)
        .then(|| {
            Retriever::new(
                state.core.clone(),
                identity.clone(),
                request_id.clone(),
                family
                    .map(ApiFamily::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                prepared.model.clone(),
                state.prefixes.clone(),
                // `prepare` guarantees this is the conversation fingerprint
                // whenever the request had one, including on the paths where
                // compression itself declined.
                prepared.prefix,
            )
        });
    let budget = Budget::new(
        proxy.retrieve_max_rounds,
        proxy.retrieve_max_tokens_per_request,
    );

    let mut recorder = Recorder::new(
        &state,
        &identity,
        &request_id,
        family,
        &prepared,
        route.origin(),
        // Taken before `outbound` is moved into whichever path sends it.
        outbound.waited(),
    );
    recorder.keep_warm = keep_warm;

    if prepared.stream {
        stream_response(
            state.clone(),
            outbound,
            family,
            retriever,
            budget,
            prepared,
            recorder,
            request_id,
            mode,
            started,
        )
        .await
    } else {
        buffered_response(
            state.clone(),
            outbound,
            family,
            retriever,
            budget,
            prepared,
            recorder,
            request_id,
            mode,
            started,
        )
        .await
    }
}

/// The keep-warm ping this request would leave behind, if its conversation is
/// one to keep warm. Whether it is armed is decided when the response ends,
/// on how the model stopped. See [`crate::keepwarm`].
#[allow(clippy::too_many_arguments)]
fn keep_warm_for(
    state: &ProxyState,
    family: Option<ApiFamily>,
    prepared: &Prepared,
    url: &str,
    upstream_headers: &HeaderMap,
    request_id: &str,
    identity: &Identity,
    upstream: &str,
) -> Option<KeepWarmPlan> {
    if family != Some(ApiFamily::AnthropicMessages) || prepared.one_hour {
        return None;
    }
    let proxy = &state.core.config_for(Some("anthropic")).proxy;
    if !proxy.cache_keep_warm {
        return None;
    }
    let conversation = prepared.prefix?;
    let parsed;
    let sent = match prepared.json.as_ref() {
        Some(value) => value,
        None => {
            parsed = serde_json::from_slice::<Value>(&prepared.body).ok()?;
            &parsed
        }
    };
    let ping = crate::keepwarm::Ping::for_request(
        sent,
        url,
        upstream_headers,
        request_id,
        identity.tenant.as_str(),
        prepared.model.clone(),
        upstream,
        conversation,
    )?;
    Some(KeepWarmPlan {
        state: state.clone(),
        ping,
        schedule: crate::keepwarm::Schedule {
            interval: proxy.cache_keep_warm_interval,
            max_pings: proxy.cache_keep_warm_max_pings,
        },
    })
}

/// A ping ready to arm, held by the [`Recorder`] until the response ends.
pub(crate) struct KeepWarmPlan {
    state: ProxyState,
    ping: crate::keepwarm::Ping,
    schedule: crate::keepwarm::Schedule,
}

/// What the compression stage decided to send.
struct Prepared {
    /// The bytes that go upstream.
    body: Bytes,
    /// The parsed outgoing body, kept only when a retrieve round may need to
    /// extend it.
    json: Option<Value>,
    /// Estimated prompt tokens the compression removed.
    saved_tokens: u32,
    /// The retrieve tool was added, so the model may call it.
    injected: bool,
    /// The client asked for a streamed response.
    stream: bool,
    /// We added `include_usage`; the client did not ask for it.
    strip_usage: bool,
    model: Option<String>,
    /// The frozen prefix this request carried, so the cache-read count the
    /// provider reports can be filed against it for next turn. `None` when
    /// nothing was frozen — there is no prefix to have an opinion about.
    prefix: Option<u64>,
    /// The request asked for one-hour cache entries, which outlast any pause a
    /// keep-warm ping would bridge.
    one_hour: bool,
}

impl Prepared {
    /// The request as it arrived, untouched.
    fn passthrough(body: Bytes, stream: bool, model: Option<String>) -> Prepared {
        Prepared {
            prefix: None,
            one_hour: false,
            body,
            json: None,
            saved_tokens: 0,
            injected: false,
            stream,
            strip_usage: false,
            model,
        }
    }
}

/// Steps 2 to 6: decide what body to forward.
/// Say that a rewrite was thrown away, so the row it already wrote stops
/// claiming a saving.
///
/// The optimization event is emitted by the engine call; the decision to keep
/// the body happens afterwards, here. Rather than reorder that — the event is
/// shared by every surface and only the proxy can discard — the row is amended.
/// Events are processed in order by one writer thread, so the row is always
/// there by the time this arrives.
/// Forward the request as it arrived — but with the retrieve tool present if
/// this conversation is meant to have it.
///
/// `tools` sits in front of `messages` in the prompt, so an entry that appears
/// on one turn and not the next moves bytes ahead of every cached message and
/// the provider re-caches the lot. Injection used to happen only at the end of
/// `prepare_inner`, after seven different `return Prepared::passthrough`
/// sites — so a turn that declined, or whose rewrite was thrown away, sent the
/// client's original bytes with no tool in them and the array flipped.
///
/// Measured: with `retrieve_tool = "always"` the tool did not appear until
/// turn seven, because turns three through six were `no_compressible_content`
/// and three discards. `Always` did not mean always.
///
/// Anything unexpected here forwards the untouched body rather than failing
/// the request: a stable prefix is worth a cache re-warm, not a 500.
/// Whether to offer `optscale_kompressor_retrieve` on this request.
///
/// `Auto` asks whether this *conversation* may ever carry a handle, not whether
/// this request already does. The difference was measured at the byte level on
/// 2026-10-07 and it is expensive. A tools block is part of the prompt the
/// provider caches, so offering the tool for the first time mid-conversation
/// changes that prompt and the cache is lost from the tools position on. The
/// capture is unambiguous: at a turn where `cache_read` fell 11,648 -> 10,624,
/// the conversation was byte-identical (28,169 of 28,171 shared), the
/// `instructions` and `model` segments were identical, the agent's own tools
/// block was 10,622 bytes on both turns, and the one thing that changed was the
/// outgoing tools block going from 10,622 bytes without `optscale_kompressor_retrieve`
/// to 13,207 with it.
///
/// So the tool is offered from the first request of a recoverable conversation.
/// It costs its own definition — about 2,585 bytes — on turns that have nothing
/// to retrieve, against a prefix that then never moves. `carries_handle` stays
/// in the condition for the case the mode cannot predict: a client echoing back
/// a handle minted somewhere else.
fn offers_retrieve_tool(
    proxy: &ProxyConfig,
    engine_mode: kompressor_engine::Mode,
    carries_handle: bool,
) -> bool {
    match proxy.retrieve_tool {
        RetrieveTool::Never => false,
        RetrieveTool::Always => true,
        RetrieveTool::Auto => engine_mode == kompressor_engine::Mode::Recoverable || carries_handle,
    }
}

fn passthrough_with_stable_tools(
    proxy: &ProxyConfig,
    engine_mode: kompressor_engine::Mode,
    family: ApiFamily,
    body: Bytes,
    stream: bool,
    model: Option<String>,
) -> Prepared {
    let wants_tool = offers_retrieve_tool(
        proxy,
        engine_mode,
        tool::carries_handle(&String::from_utf8_lossy(&body)),
    );
    if !wants_tool {
        return Prepared::passthrough(body, stream, model);
    }
    let Ok(mut value) = serde_json::from_slice::<Value>(&body) else {
        return Prepared::passthrough(body, stream, model);
    };
    if !matches!(tool::inject(&mut value, family), tool::Injection::Injected) {
        return Prepared::passthrough(body, stream, model);
    }
    let Ok(out) = serde_json::to_vec(&value) else {
        return Prepared::passthrough(body, stream, model);
    };
    Prepared {
        body: Bytes::from(out),
        json: Some(value),
        saved_tokens: 0,
        injected: true,
        stream,
        strip_usage: false,
        model,
        // Stamped by `prepare`, which computes it for every request however
        // this one turned out.
        prefix: None,
        one_hour: false,
    }
}

/// Whether a discard would cost more than the rewrite it throws away.
///
/// Discarding forwards the client's ORIGINAL bytes, so the provider caches
/// those. A later turn that compresses the same messages then sends different
/// bytes and the prefix misses from there on — the flip is raw-to-compressed,
/// and it happens exactly because a discard cached raw bytes for messages a
/// later turn will rewrite.
///
/// So the guard stays in force until there is a cached prefix to lose, and is
/// suppressed after. Before anything is cached a discard is free. After, a
/// body that came out slightly larger costs a few hundred tokens once, while a
/// missed prefix costs the re-read of everything following the first changed
/// byte.
fn discard_would_break_the_prefix(state: &ProxyState, fingerprint: Option<u64>) -> bool {
    fingerprint.is_some_and(|key| state.prefixes.has_been_cached(key))
}

fn record_discard(state: &ProxyState, identity: &Identity, request_id: &str) {
    state.core.stats().send(Event::Discarded {
        tenant: identity.tenant.as_str().to_string(),
        request_id: request_id.to_string(),
    });
}

#[allow(clippy::too_many_arguments)]
/// The conversation this request belongs to, for the prefix-cache tracker —
/// and, while the body is open, the watermark that says how far its cache had
/// reached before this proxy wrote anything.
///
/// Walks the body a second time rather than reaching into `prepare_inner`:
/// that function has ten ways to decline and the identity has to survive all
/// of them, so computing it alongside is simpler than threading it through.
/// The walk is the cheap half of the pipeline and this only runs where the
/// engine would have run anyway.
///
/// The watermark has to be filed from here for the same reason, and learning
/// that cost a debugging round: the very first turn of a conversation is
/// usually the one that declines — everything frozen, nothing to compress —
/// so a watermark recorded inside `prepare_inner` was never recorded on the
/// turn that defines it. Turn two then filed its own, much higher marker count
/// as the watermark and the floor never fell. Measured: `marker=3 frozen=3`
/// where it should have read `frozen=1`.
/// The text of a conversation's opening messages, for the identity chain.
///
/// Everything, in order, capped inside
/// [`prefixcache::identity_chain_in_scope`] — not a chosen few. Choosing was
/// the bug: an agent harness opens every conversation
/// with the same preamble, so the first message, and then the first message
/// plus the first `user` message, both named the CLIENT rather than the
/// conversation. Measured on a 20-task run, 637 requests fell into five keys,
/// and the same five keys came back on a later run with different tasks.
///
/// The whole opening list does discriminate — two tasks differ somewhere in
/// it — and it stays stable because a later turn APPENDS, so the opening it
/// was first seen with is still its opening. Resolving that into an identity
/// is [`PrefixTracker::identify`]'s job. The chain is also scoped to the
/// authenticated tenant, configured upstream, API family and request model so
/// identical text across those boundaries cannot share tracker state.
fn conversation_opening(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .map(|message| message.content.joined_text())
        .collect()
}

fn prefix_identity(
    state: &ProxyState,
    identity: &Identity,
    family: Option<ApiFamily>,
    body: &Bytes,
    provider: &str,
) -> Option<u64> {
    // Identified with compression off too. The control arm of a comparison has
    // to be observed the way the treated arm is: without a conversation its
    // rows filed no conversation, its cache-read drops went untracked — an
    // off run reported zero cache loss over 992 requests — and `cache_ttl =
    // "adaptive"` could never switch there. Reading the body is all this does;
    // what is forwarded is decided elsewhere.
    let family = family?;
    let value: Value = serde_json::from_slice(body).ok()?;
    let walk = walker_for(family).walk(&value)?;
    let opening = conversation_opening(&walk.messages);
    let refs: Vec<&str> = opening.iter().map(String::as_str).collect();
    let scope = [
        identity.tenant.as_str(),
        provider,
        family.as_str(),
        walk.model.as_deref().unwrap_or(""),
    ];
    let fingerprint = state
        .prefixes
        .identify(&prefixcache::identity_chain_in_scope(&refs, &scope))?;
    // Recorded whatever the setting says, so that turning `frozen_prefix` on
    // mid-session finds a watermark describing what the client established
    // rather than whatever this turn happens to mark.
    state.prefixes.floor(fingerprint, walk.frozen_message_count);
    Some(fingerprint)
}

/// Decide what to send, then stamp the conversation's cache identity on it.
///
/// Stamped here rather than inside, because the identity has to survive every
/// way `prepare_inner` can decline. The case this feature exists for — a whole
/// request frozen, so nothing compresses — returns `Prepared::passthrough`,
/// and passthrough carries no fingerprint: the prefix was never recorded, the
/// provider's answer about it was never learned, and the state stayed `Warm`
/// for good. Measured: `tracked()` was 0 after three turns that should have
/// filed one entry.
#[allow(clippy::too_many_arguments)]
async fn prepare(
    state: &ProxyState,
    identity: &Identity,
    mode: ProxyMode,
    family: Option<ApiFamily>,
    method: &Method,
    request_headers: &HeaderMap,
    body: Bytes,
    request_id: &str,
    provider: &str,
) -> Prepared {
    let fingerprint = prefix_identity(state, identity, family, &body, provider);
    // Decided once, here: under `adaptive` the question itself moves the
    // conversation's state. Anthropic only; the TTL is its field.
    let long_ttl = family == Some(ApiFamily::AnthropicMessages)
        && match state
            .core
            .config_for(family.map(ApiFamily::provider))
            .proxy
            .cache_ttl
        {
            CacheTtl::Default => false,
            CacheTtl::OneHour => true,
            CacheTtl::Adaptive => fingerprint.is_some_and(|key| state.prefixes.long_ttl(key)),
        };
    let mut prepared = prepare_inner(
        state,
        identity,
        mode,
        family,
        method,
        request_headers,
        body,
        request_id,
        provider,
        fingerprint,
        long_ttl,
    )
    .await;
    if prepared.prefix.is_none() {
        prepared.prefix = fingerprint;
    }
    // After every way `prepare_inner` can decline, for the reason the identity
    // is stamped here: under the default marker floor Claude Code freezes its
    // whole conversation, so nearly every request is a passthrough, and a TTL
    // applied only to compressed bodies would almost never reach the provider.
    if long_ttl {
        extend_cache_ttl(&mut prepared);
        prepared.one_hour = true;
    }
    prepared
}

/// Ask for one-hour cache entries on whatever `prepared` will send.
///
/// A passthrough body is parsed for it and re-serialized. That changes JSON
/// bytes, never the prompt: the provider keys its cache on the rendered prompt,
/// and `serde_json` here keeps key order and numbers exact. A body that does
/// not parse, or carries no marker, goes out as it was.
fn extend_cache_ttl(prepared: &mut Prepared) {
    let mut parsed = None;
    let value = match prepared.json.as_mut() {
        Some(value) => value,
        None => match serde_json::from_slice::<Value>(&prepared.body) {
            Ok(value) => parsed.insert(value),
            Err(_) => return,
        },
    };
    if crate::cachettl::extend_cache_ttl(value) > 0
        && let Ok(out) = serde_json::to_vec(value)
    {
        prepared.body = Bytes::from(out);
    }
}

#[allow(clippy::too_many_arguments)]
async fn prepare_inner(
    state: &ProxyState,
    identity: &Identity,
    mode: ProxyMode,
    family: Option<ApiFamily>,
    method: &Method,
    request_headers: &HeaderMap,
    body: Bytes,
    request_id: &str,
    provider: &str,
    conversation: Option<u64>,
    // Whether this request will ask for one-hour cache entries, decided before
    // compression so a promotion is priced at the write it will cost.
    long_ttl: bool,
) -> Prepared {
    // This provider's settings: `proxy.providers` applied over the defaults.
    let config = state.core.config_for(family.map(ApiFamily::provider));
    let proxy = &config.proxy;
    let Some(family) = family else {
        let stream = unparsed_asks_to_stream(&body, request_headers);
        return Prepared::passthrough(body, stream, None);
    };
    if mode.is_off() || !proxy.enabled || method != Method::POST || !is_json(request_headers) {
        let stream = unparsed_asks_to_stream(&body, request_headers);
        // The model still goes on the row, so the control arm can be priced
        // per model like the treated one; the body is forwarded untouched.
        let model = mode
            .is_off()
            .then(|| serde_json::from_slice::<Value>(&body).ok())
            .flatten()
            .and_then(|value| {
                value
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        return Prepared::passthrough(body, stream, model);
    }

    let Ok(mut value) = serde_json::from_slice::<Value>(&body) else {
        state.failopen.record(Stage::Parse, &"body is not JSON");
        let stream = unparsed_asks_to_stream(&body, request_headers);
        return Prepared::passthrough(body, stream, None);
    };
    let stream = value
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| streams_by_header(request_headers));

    // The walker is used and dropped before the engine runs: it is a boxed
    // trait object that is not `Send`, and holding one across the compression
    // await would make this whole future unspawnable.
    let walked = walker_for(family).walk(&value);
    let Some(mut walk) = walked else {
        state
            .failopen
            .record(Stage::Walk, &"unrecognised request shape");
        return Prepared::passthrough(body, stream, None);
    };
    let model = walk.model.clone();

    // The same estimator the engine uses. Gating here on a cheap `len/4` while
    // measuring the saving with this one makes the skip decision and the
    // reported saving answers to two different questions.
    // Resolved before the token gate, not after. The gate's early return still
    // has to decide whether to offer the retrieve tool, and that decision is the
    // engine's effective mode rather than the proxy's patch -- offering it on
    // some turns of a conversation and not others is what moves the prefix.
    let options = state.core.options_for_provider(
        identity,
        Some(family.provider()),
        Some(&EngineOptionsPatch {
            mode: mode.engine_mode(),
            // The proxy's own knob can withhold the read lifecycle but never
            // grant it: `proxy.stale_read = false` is an operator saying "not
            // on this surface", while leaving it true means "whatever the
            // engine is configured to do" rather than overriding it.
            //
            // `first_seen` withholds it too, because the two cannot both be
            // had. Compressing history only pays while the provider's cache
            // keeps hitting, and that rests on the prefix-monotonic invariant
            // `crossturn` states: a message is matched only against strictly
            // earlier ones, so appending a turn never rewrites an earlier one.
            // Stale-read is the single transform that breaks it — a later
            // write to the same path replaces the read before it — and a
            // rewrite inside the cached prefix costs a re-write of everything
            // from that byte on.
            //
            // Measured, turn five to turn six of a Harbor run: the read went
            // out and was cached, the edit arrived, and 4,324 cached tokens
            // were written again at 1.25x. The trade does not come back: at
            // Opus rates, re-writing a 20,000-token suffix to remove a
            // 2,000-token read needs ~125 further turns to break even, against
            // sessions that run 20 to 70.
            enable_stale_read: (!proxy.stale_read
                || proxy.frozen_prefix == FrozenPrefix::FirstSeen)
                .then_some(false),
            ..Default::default()
        }),
    );

    let before = compressible_tokens(&walk);
    if before < proxy.min_tokens {
        tracing::debug!(
            tokens = before,
            "below proxy.min_tokens; forwarding as it arrived"
        );
        return passthrough_with_stable_tools(proxy, options.mode, family, body, stream, model);
    }

    // Is the freeze protecting a cache that still exists? Fingerprinted over
    // the frozen messages as they arrived, before the engine touches them —
    // that is what the provider cached on, and it has to be the same bytes
    // next turn for the lookup to mean anything.
    // The identity `prefix_identity` already resolved, passed in rather than
    // derived a second time. Two derivations of "which conversation is this"
    // is a bug waiting for the day they disagree, and that day came: this one
    // read `take(frozen_count.min(1))` while the other read `take(1)`, so they
    // agreed only while something was frozen AND the head was one message.
    // When the identity became a learned prefix the two diverged on every
    // request, the floor was filed under one key and looked up under another,
    // and three first_seen tests went from compressing to compressing nothing.
    let fingerprint = conversation;
    let prefix_cache = fingerprint
        .map(|key| state.prefixes.state(key))
        .unwrap_or(PrefixCache::Warm);
    if prefix_cache == PrefixCache::Cold {
        tracing::info!(
            request_id,
            frozen = walk.frozen_message_count,
            "the provider reported no cache read for this prefix last turn; \
             lifting the freeze"
        );
    }

    // How much of this conversation the provider has actually cached, which is
    // not what the client's marker says. Claude Code marks its *final*
    // message, so the marker-derived floor covers the messages that arrived
    // this turn — bytes that are about to be cached, not bytes that are. See
    // `PrefixTracker::floor`.
    let frozen = match proxy.frozen_prefix {
        FrozenPrefix::Marker => walk.frozen_message_count,
        FrozenPrefix::FirstSeen => fingerprint
            .map(|key| state.prefixes.floor(key, walk.frozen_message_count))
            // Nothing frozen means no prefix to have an opinion about, and the
            // marker count is already zero.
            .unwrap_or(walk.frozen_message_count),
    };
    if frozen < walk.frozen_message_count {
        tracing::debug!(
            request_id,
            marker = walk.frozen_message_count,
            cached = frozen,
            "the client's marker freezes further than the provider has cached; \
             compressing the difference"
        );
    }

    // `cache_control` protects in four places, and lifting three of them
    // changes nothing while the fourth holds. `SlotOrigin::protected` is the
    // fourth: `apply_walk` skips a protected origin, so the engine's rewrite
    // never reaches the body it came from. Measured — the engine reported
    // `before=10404 after=556` on a cold prefix and the request went out
    // byte-identical.
    //
    // Cleared here rather than in the walker because the walker has no way to
    // know: it runs before anything has asked the tracker, and its trait takes
    // the body alone. The reason lives on the slot, so the origin is matched
    // back to it by index.
    if prefix_cache == PrefixCache::Cold {
        let liftable: Vec<bool> = walk
            .origins
            .iter()
            .map(
                |origin| match walk.messages.get(origin.message).map(|m| &m.content) {
                    Some(kompressor_engine::messages::MessageContent::Blocks(slots)) => {
                        slots.get(origin.slot).is_some_and(|slot| {
                            slot.protection == kompressor_engine::messages::Protection::WhileCached
                        })
                    }
                    _ => false,
                },
            )
            .collect();
        for (origin, lift) in walk.origins.iter_mut().zip(liftable) {
            if lift {
                origin.protected = false;
            }
        }
    }

    // Both routes to keeping a fragment inline: fetched twice already, or
    // asked about now. The second is the only recovery path a relayed
    // websocket has, because that transport runs no retrieve loop.
    // Counts beside the ids: the engine prices a promotion's prefix rewrite
    // against how often the fragment would otherwise be fetched again, and this
    // is the only place that number is known.
    let fetches = state.prefixes.demand(conversation);
    let mut keep_inline: Vec<RefId> = fetches.iter().map(|(id, _)| *id).collect();
    for id in wanted_fragments(state, identity, conversation, &walk.messages) {
        if !keep_inline.contains(&id) {
            keep_inline.push(id);
        }
    }
    let keep_inline = affordable_inline(
        &walk.messages,
        keep_inline,
        before,
        proxy.keep_inline_max_percent,
        |id| {
            state
                .core
                .peek(identity, id)
                .map(|fragment| estimate_tokens(&fragment, ""))
                .unwrap_or(0)
        },
    );

    let messages = std::mem::take(&mut walk.messages);

    let compressed = compress_with_deadline(
        state,
        identity,
        messages,
        frozen,
        prefix_cache,
        &options,
        request_id,
        family,
        model.as_deref(),
        provider,
        // What this conversation has already fetched back, plus anything this
        // turn appears to be asking about. Empty for almost every request, and
        // the engine short-circuits on empty.
        keep_inline,
        fetches,
        // Measured from what this provider has actually reported, not assumed.
        state.prefixes.cache_hit_per_mille(),
        outside_tokens(&value),
        // At the TTL this request will ask for: a rewrite under one-hour
        // entries is written at 2x, not 1.25x.
        Some(
            state
                .prefixes
                .promotion_pricing(crate::prefixcache::CachePrices::for_model(
                    family,
                    model.as_deref(),
                    long_ttl,
                )),
        ),
        conversation,
        proxy.compress_timeout,
    )
    .await;
    let Some((messages, saved)) = compressed else {
        return passthrough_with_stable_tools(proxy, options.mode, family, body, stream, model);
    };
    walk.messages = messages;

    let rewritten = kompressor_wire::apply_walk(&mut value, &walk);
    let after = compressible_tokens(&walk);
    if rewritten > 0 && after >= before && !discard_would_break_the_prefix(state, fingerprint) {
        // the engine guards this per slot; the total is guarded here because a
        // body that grew is a body we should never have touched
        state
            .failopen
            .record(Stage::Discarded, &"compression did not shrink the prompt");
        record_discard(state, identity, request_id);
        return passthrough_with_stable_tools(proxy, options.mode, family, body, stream, model);
    }

    let carries_handle = tool::carries_handle(&String::from_utf8_lossy(&body))
        || walk
            .origins
            .iter()
            .filter_map(|origin| walk.current_text(origin))
            .any(tool::carries_handle);
    let wants_tool = offers_retrieve_tool(proxy, options.mode, carries_handle);
    let mut injected = false;
    if wants_tool {
        match tool::inject(&mut value, family) {
            tool::Injection::Injected => injected = true,
            tool::Injection::ClientOwnsTool => {
                tracing::debug!("the client declares the tool itself; execution is its own")
            }
            tool::Injection::NotNeeded => {}
            tool::Injection::Unsupported => state
                .failopen
                .record(Stage::Inject, &"a tools field we do not understand"),
        }
    }

    let strip_usage = stream && proxy.force_usage && force_usage(&mut value, family);

    if rewritten == 0 && !injected && !strip_usage {
        // nothing was changed, so the provider sees the client's own bytes
        return Prepared::passthrough(body, stream, model);
    }

    match serde_json::to_vec(&value) {
        Ok(out) => {
            let out = Bytes::from(out);
            if out.len() > body.len()
                && rewritten > 0
                && !discard_would_break_the_prefix(state, fingerprint)
            {
                state
                    .failopen
                    .record(Stage::Discarded, &"the rewritten body is larger");
                record_discard(state, identity, request_id);
                return passthrough_with_stable_tools(
                    proxy,
                    options.mode,
                    family,
                    body,
                    stream,
                    model,
                );
            }
            tracing::info!(
                saved_tokens = saved,
                rewritten_slots = rewritten,
                injected_tool = injected,
                "compressed"
            );
            Prepared {
                body: out,
                json: Some(value),
                saved_tokens: saved,
                injected,
                stream,
                strip_usage,
                model,
                prefix: fingerprint,
                one_hour: false,
            }
        }
        Err(error) => {
            state.failopen.record(Stage::Discarded, &error);
            Prepared::passthrough(body, stream, model)
        }
    }
}

/// Run the engine off the runtime, with a deadline.
///
/// A timeout, a panic and an error are one case: the original body is what gets
/// forwarded. The blocking task is left to finish on its own — it cannot be
/// cancelled, and abandoning it is cheaper than making every request wait for a
/// pathological payload.
/// The prompt a request carries outside its messages — system prompt,
/// instructions, tool schemas — in estimated tokens.
///
/// Every retrieve round re-sends it, so the engine prices a round with it. On
/// Claude Code it is ~13,600 tokens before the first message, and leaving it out
/// made a 23,000-token prompt read as 9,347: a fragment fetched in three
/// consecutive requests was refused twice for a rewrite it would have repaid.
fn outside_tokens(body: &Value) -> u32 {
    ["system", "instructions", "tools"]
        .iter()
        .filter_map(|key| body.get(*key))
        .filter(|value| !value.is_null())
        .map(|value| estimate_tokens(&value.to_string(), "system"))
        .fold(0, u32::saturating_add)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn compress_with_deadline(
    state: &ProxyState,
    identity: &Identity,
    messages: Vec<Message>,
    frozen_message_count: usize,
    // Whether the provider is still serving that prefix from cache. Decided by
    // the caller from what the provider reported last time; see `prefixcache`.
    prefix_cache: PrefixCache,
    options: &kompressor_engine::EngineOptions,
    request_id: &str,
    family: ApiFamily,
    model: Option<&str>,
    // The upstream mount's name, which is what a report means by "provider":
    // the thing the operator named in their own configuration, and stable
    // across the hosts one mount can send to.
    provider: &str,
    // Handles this conversation has already fetched back; the engine keeps
    // their content inline instead of eliding it again.
    keep_inline: Vec<RefId>,
    fetches: Vec<(RefId, u32)>,
    cache_hit_per_mille: u32,
    // What the provider bills outside the messages; see `outside_tokens`.
    outside_tokens: u32,
    // The provider's prices for a promotion; `None` prices it from the rate.
    cache_pricing: Option<kompressor_engine::messages::CachePricing>,
    // Which conversation to pin a promotion against. `None` for a transport
    // with no conversation of its own, which then keeps nothing pinned.
    conversation: Option<u64>,
    timeout: Duration,
) -> Option<(Vec<Message>, u32)> {
    let core = state.core.clone();
    let prefixes = state.prefixes.clone();
    let identity = identity.clone();
    let options = options.clone();
    let request_id = request_id.to_string();
    let model = model.map(str::to_string);
    let provider = provider.to_string();

    // Computation only. A `spawn_blocking` task cannot be cancelled, so when
    // the deadline below wins this one still runs to completion — and until
    // publication moved out of it, that abandoned task retained fragments
    // nothing could fetch, filed savings for an output never forwarded, and
    // promoted handles that rewrote later prompts. Everything it produces now
    // lives in the `Computed` it returns, which the timeout arm simply drops.
    let work = move || {
        let mut messages = messages;
        let computed = core.compress_only(
            &mut messages,
            &options,
            frozen_message_count,
            "",
            prefix_cache,
            &keep_inline,
            &fetches,
            cache_hit_per_mille,
            outside_tokens,
            cache_pricing,
        );
        (
            core, identity, options, request_id, model, provider, messages, computed,
        )
    };

    match tokio::time::timeout(timeout, tokio::task::spawn_blocking(work)).await {
        Ok(Ok((core, identity, options, request_id, model, provider, mut messages, computed))) => {
            // Published only now that the race is decided, so nothing a
            // timeout discarded can reach the store, the stats or the
            // promotion state.
            let outcome = core.publish(
                &identity,
                &mut messages,
                computed,
                &RequestInfo {
                    request_id: &request_id,
                    api_family: Some(family.as_str()),
                    model: model.as_deref(),
                    provider: Some(provider.as_str()),
                },
                &options,
                Surface::Proxy,
            );
            // Pinned before anything else can re-decide it. The rewrite this
            // promotion paid for is spent; weighing it again next turn,
            // against a bar that will have moved, is how a promoted fragment
            // gets re-elided and loses the prefix a second time.
            prefixes.record_inlined(conversation, &outcome.stats.kept_inline_refs);
            let saved = outcome
                .stats
                .tokens_in
                .saturating_sub(outcome.stats.tokens_out);
            Some((messages, saved))
        }
        // a panicked engine is a fail-open, not a 500: the invariant is that
        // compression subtracts tokens, never availability
        Ok(Err(error)) => {
            state.failopen.record(Stage::Compress, &error);
            None
        }
        Err(elapsed) => {
            state.failopen.record(Stage::Compress, &elapsed);
            None
        }
    }
}

/// Σ estimate_tokens over the slots the engine is allowed to touch.
pub(crate) fn compressible_tokens(walk: &Walk) -> u32 {
    walk.origins
        .iter()
        .filter(|origin| !origin.protected)
        .map(|origin| {
            let role = walk
                .messages
                .get(origin.message)
                .map(|message| message.role.as_str())
                .unwrap_or("");
            let text = walk.current_text(origin).unwrap_or(&origin.original);
            estimate_tokens(text, role)
        })
        .fold(0u32, |total, tokens| total.saturating_add(tokens))
}

/// Ask for usage on a streamed OpenAI completion, reporting whether we had to.
///
/// Only OpenAI Chat has this knob: Responses reports usage in its
/// `response.completed` event and Anthropic in `message_delta`, both
/// unconditionally.
fn force_usage(value: &mut Value, family: ApiFamily) -> bool {
    if family != ApiFamily::OpenAiChat {
        return false;
    }
    let already = value
        .pointer("/stream_options/include_usage")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if already {
        return false;
    }
    let Some(object) = value.as_object_mut() else {
        return false;
    };
    match object.get_mut("stream_options") {
        Some(Value::Object(options)) => {
            options.insert("include_usage".to_string(), json!(true));
        }
        _ => {
            object.insert("stream_options".to_string(), json!({"include_usage": true}));
        }
    }
    true
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .ends_with("json")
        })
}

/// Some SDKs signal streaming with `Accept` rather than in the body.
fn streams_by_header(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/event-stream"))
}

/// Whether a body we deliberately did not parse is asking for a stream.
///
/// A request in `off` mode, in a family we do not walk, or with a body that is
/// not JSON is never parsed — and sending a streamed completion down the
/// buffered path would hold the whole answer back until the provider finished
/// writing it. A client watching tokens appear would see nothing at all for the
/// length of the answer, which is a far worse regression than not compressing.
///
/// The scan is deliberately loose. A false positive costs nothing: the relay
/// buffers a response that turns out not to be an event stream. A false
/// negative is the stall.
fn unparsed_asks_to_stream(body: &[u8], headers: &HeaderMap) -> bool {
    if streams_by_header(headers) {
        return true;
    }
    let Some(at) = body.windows(8).position(|window| window == br#""stream""#) else {
        return false;
    };
    let mut rest = body[at + 8..]
        .iter()
        .copied()
        .skip_while(u8::is_ascii_whitespace);
    if rest.next() != Some(b':') {
        return false;
    }
    let tail: Vec<u8> = rest.skip_while(u8::is_ascii_whitespace).take(4).collect();
    tail == b"true"
}

/// The inbound request id, or a new one.
///
/// Accepting the client's is what lets a trace started in the client's logs be
/// followed through the compression event, the retrieval events and the proxy
/// event without a join on timestamps.
fn request_id(headers: &HeaderMap) -> String {
    headers
        .get(headers::REQUEST_ID)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .map(str::to_string)
        .unwrap_or_else(new_request_id)
}

/// A sortable, collision-free-enough identifier without pulling in a uuid
/// dependency: milliseconds since the epoch, then a process-wide counter.
fn new_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("kp-{millis:012x}-{seq:08x}")
}

/// Resolve the tenant, falling back to the anonymous one.
///
/// A proxy that answered 401 to its own credential check would turn a working
/// provider call into our error — the one thing §12.8 forbids. An unresolvable
/// credential costs attribution, not the request.
///
/// **It also costs isolation, and that is the part worth stating.** Every
/// `AuthError` — missing, unknown, disabled — resolves here to
/// [`Identity::anonymous`], which is a real tenant with a fragment store and a
/// live retrieve path. Two callers with absent, mistyped or revoked keys share
/// it, and handles are content digests rather than access tokens, so one can
/// resolve a handle the other minted.
///
/// Failing open on *attribution* and failing open on *isolation* are different
/// decisions, and §12.8 only requires the first: forwarding the request
/// uncompressed would satisfy the rule and keep the namespaces apart. The
/// current behaviour is the simpler one and is kept deliberately — a
/// deployment running without `auth.mode` is the common case and every caller
/// in it is anonymous by design — but a deployment that *does* configure keys
/// is relying on this path never being reached. Narrowing it means refusing to
/// compress rather than refusing the request; see §12.8 before changing it.
pub(crate) fn resolve_identity(state: &ProxyState, headers: &HeaderMap) -> Identity {
    let bearer = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let credentials = Credentials {
        bearer,
        api_key: header("x-api-key"),
        kompressor_key: header("x-kompressor-key"),
        secret: header("x-kompressor-secret"),
        requested_tenant: header("x-kompressor-tenant"),
    };
    match state.core.auth().resolve(&credentials) {
        Ok(identity) => identity,
        Err(error) => {
            state.failopen.record(Stage::Auth, &error);
            Identity::anonymous()
        }
    }
}

/// How many markers one request may promote by relevance.
///
/// A bound, not a tuning knob. The scan reads a fragment per candidate, and a
/// request that has elided a hundred things should not turn into a hundred
/// store reads and a hundred expansions because one word matched.
const WANTED_PER_REQUEST: usize = 4;

/// Trim the inline set to what a request can afford to carry.
///
/// The engine's own guard asks whether one message is worth keeping whole. It
/// prices a single turn and commits to every remaining one, which is a question
/// it cannot answer locally: measured across four runs of one terminal-bench
/// task, keeping two messages inline looked like a 2.4x forfeit-to-protected
/// ratio when it was decided and cost 80.7x over the 34 turns it then applied
/// to -- and the per-message guard still had roughly 13x headroom, so nothing
/// in that path would ever have stopped it. This bounds the *set* instead: the
/// forfeit for a turn cannot exceed `keep_inline_max_percent` of what the
/// engine looked at, however many individually-reasonable decisions produced
/// it.
///
/// **Admission is in promotion order and never revoked**, which is the whole
/// reason this trims the tail rather than, say, dropping the largest. The set
/// grows and the context it is measured against grows, so a fragment admitted
/// once stays admitted; evicting one would put a handle back where inline text
/// used to be, rewrite the prefix, and cost the provider cache everything after
/// it -- paying the §12.10 price to save a fraction of it.
fn affordable_inline(
    messages: &[Message],
    candidates: Vec<RefId>,
    compressible: u32,
    max_percent: u32,
    // The fragment's own size, passed in rather than looked up here, so the
    // decision can be tested without a store behind it.
    protected_for: impl Fn(&RefId) -> u32,
) -> Vec<RefId> {
    if candidates.is_empty() {
        return candidates;
    }
    // `max_percent == 0` turns the *ceiling* off, not the break check below it.
    // Those answer different questions and an operator disabling one must not
    // silently disable the other -- which an early return here did, and a test
    // written to isolate the break check found it.
    let ceiling = if max_percent == 0 {
        u32::MAX
    } else {
        (compressible as u64 * max_percent as u64 / 100) as u32
    };
    // Priced by the message that would be shipped whole, not by the fragment
    // inside it. The fragment is what is *protected*; the message is what is
    // *given up*, and they are not close -- in the run this was built for, two
    // messages of 3,958 tokens were kept to protect 834 tokens of fragment, so
    // pricing the fragment would have set a ceiling nearly five times looser
    // than it reads. Charging for the protection rather than the cost is the
    // same mistake the per-message guard makes about time, in a second
    // dimension.
    // Two filters, and they answer different questions. This one is per
    // candidate and one-off: is the prefix rewrite this promotion causes worth
    // the fragment it protects? The ceiling below is about the set as a whole
    // and recurs every turn.
    let mut unaffordable = 0usize;
    let priced: Vec<(RefId, u32)> = candidates
        .into_iter()
        .filter_map(|id| {
            let (forfeited, break_cost) = forfeit_for(messages, &id);
            // A fragment already shipping inline has no marker to replace, so
            // there is no break to pay and nothing to refuse -- re-deciding it
            // every turn is what would churn the prefix.
            if break_cost == 0 {
                return Some((id, forfeited));
            }
            let protected = protected_for(&id);
            let payback = protected.saturating_mul(KEEP_INLINE_BREAK_PAYBACK_FETCHES);
            if break_cost > payback {
                tracing::warn!(
                    handle = %id,
                    break_cost,
                    protected,
                    payback,
                    "not promoting: the prefix rewrite costs more than the \
                     fragment can pay back, so the model may ask for it again"
                );
                unaffordable += 1;
                return None;
            }
            Some((id, forfeited))
        })
        .collect();
    let (kept, refused, spent) = inline_set_under_ceiling(&priced, ceiling);
    let refused = refused + unaffordable;
    if refused > 0 {
        tracing::warn!(
            refused,
            kept = kept.len(),
            spent,
            ceiling,
            compressible,
            "the inline set is at its ceiling; fragments beyond it will be \
             elided again and may cost a retrieve round"
        );
    }
    kept
}

/// How many future fetches a promotion is allowed to bet on.
///
/// Promoting a fragment turns a marker the provider has already cached back
/// into full text, which rewrites the prompt at that message and costs a
/// re-read of everything after it -- once, at the fresh rate. The engine's own
/// guard cannot see this: it prices the compression forfeited on each later
/// turn and never the one-off break.
///
/// Measured on 2026-10-07 with both bodies captured. A fragment of **175
/// tokens** was promoted into a message sitting **17% into the prompt**, so 83%
/// of the prefix followed it: `cache_read` fell 18,816 -> 12,672 and **6,144
/// tokens** were re-read. Handles in the outgoing conversation went 11 -> 7 on
/// that turn, which is the promotion itself. Breaking even needed the model to
/// fetch that fragment **35 more times**.
///
/// This number is a **bet, not a measurement**, and it is the honest shape of
/// the problem: once a fragment is promoted nothing ever reveals whether it
/// would have been wanted again, so the saving side is unobservable by
/// construction. Eight says we will spend a break worth up to eight further
/// fetches and no more. It refuses the case above by a wide margin and leaves
/// alone the promotions that cost little -- a message near the end of the
/// prompt has almost nothing after it, so its break is small whatever the
/// fragment is worth.
const KEEP_INLINE_BREAK_PAYBACK_FETCHES: u32 = 8;

/// What keeping `id` inline costs: the message that would ship whole, and the
/// prompt that sits behind it.
///
/// Zero when no message in this request carries the handle, which is the right
/// answer rather than a missing one -- a fragment that is not here forfeits
/// nothing by being on the list, and dropping it would churn the set for no
/// saving.
///
/// The second figure is the cache break: every token after the carrying message
/// is re-read at full price on the turn the promotion lands, because the bytes
/// at that position changed. Only the *first* message carrying the handle
/// counts, since that is where the prefix stops matching.
fn forfeit_for(messages: &[Message], id: &RefId) -> (u32, u32) {
    let needle = id.to_string();
    let texts: Vec<String> = messages
        .iter()
        .map(|message| message.content.joined_text())
        .collect();
    let Some(at) = texts.iter().position(|text| text.contains(&needle)) else {
        return (0, 0);
    };
    // Unauthored, as the retrieval statistics are: the estimator's prose
    // discount would make these quietly smaller than they read.
    let forfeited = estimate_tokens(&texts[at], "");
    let break_cost = texts[at + 1..]
        .iter()
        .map(|text| estimate_tokens(text, ""))
        .sum();
    (forfeited, break_cost)
}

/// Take candidates in order while they fit, and say how many did not.
///
/// Split out from [`affordable_inline`] so the admission rule can be tested
/// without a store behind it. In order and never reordered: see that function
/// for why the tail is what gives way.
fn inline_set_under_ceiling(priced: &[(RefId, u32)], ceiling: u32) -> (Vec<RefId>, usize, u32) {
    let mut spent = 0u32;
    let mut kept = Vec::with_capacity(priced.len());
    let mut refused = 0usize;
    for (id, cost) in priced {
        if spent.saturating_add(*cost) > ceiling {
            refused += 1;
            continue;
        }
        spent = spent.saturating_add(*cost);
        kept.push(*id);
    }
    (kept, refused, spent)
}

/// Shortest word that may count as a match.
///
/// Three letters and under are `the`, `and`, `src`, `log` — present in almost
/// any fragment, and matching on them expands everything. Measured against the
/// alternative in headroom, where an unscoped proactive expansion turned a
/// 127-character message into 27,722 characters of someone else's tool output.
const SHORTEST_WORD: usize = 4;

/// How many distinct words must appear in both the query and the fragment.
const WORDS_IN_COMMON: usize = 2;

/// Does `query` look like it is asking about `fragment`?
///
/// Deliberately crude: distinct lowercase words of at least [`SHORTEST_WORD`]
/// characters, present in both. The cost of a false positive is one message
/// shipped whole; the cost of a false negative is the model not getting content
/// it asked about, which on a websocket it cannot then fetch. Erring towards
/// expansion is the right asymmetry here, but only just — hence two words
/// rather than one.
pub(crate) fn query_is_about(query: &str, fragment: &str) -> bool {
    let words = |text: &str| -> std::collections::HashSet<String> {
        text.split(|c: char| !c.is_alphanumeric())
            .filter(|word| word.chars().count() >= SHORTEST_WORD)
            .map(str::to_lowercase)
            .collect()
    };
    let asked = words(query);
    if asked.is_empty() {
        return false;
    }
    let held = words(fragment);
    asked.iter().filter(|word| held.contains(*word)).count() >= WORDS_IN_COMMON
}

/// Handles this request carries that the newest user message seems to be about.
///
/// # Why this exists
///
/// Over HTTP a model that meets a marker it needs calls `optscale_kompressor_retrieve`
/// and the proxy answers it inside the round trip. Over a relayed websocket
/// there is no such path: the turn is a live event stream, the tool is
/// deliberately not injected, and a marker the model cannot resolve is simply
/// content it has lost. This is the recovery path for transports that have no
/// other one — decide on the way *in* that a fragment is wanted, and ship it
/// whole instead of eliding it again.
///
/// # What bounds it
///
/// Only markers **present in this request** are considered. That is the lesson
/// from headroom's equivalent feature, where a tracker of every compression the
/// process had made expanded one agent's tool output into a teammate's prompt:
/// a 127-character message reached the model as 27,722 characters. A fragment
/// this conversation never received was never context it lost.
///
/// Promotion is sticky, by design. A query-driven expansion that came and went
/// with the wording of each turn would rewrite the prefix back and forth; once
/// a fragment is wanted it stays inline, exactly as a twice-fetched one does.
/// The engine's own cost guard still applies, so a tiny fragment cannot drag a
/// huge message inline.
fn wanted_fragments(
    state: &ProxyState,
    identity: &Identity,
    conversation: Option<u64>,
    messages: &[Message],
) -> Vec<RefId> {
    let Some(conversation) = conversation else {
        return Vec::new();
    };
    // The newest user text is the query: it is what the model is answering now.
    let Some(query) = messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .map(|message| message.content.joined_text())
    else {
        return Vec::new();
    };
    if query.is_empty() {
        return Vec::new();
    }

    let already = state.prefixes.retrieved(Some(conversation));
    let mut wanted = Vec::new();
    for message in messages {
        for id in refs_in(&message.content.joined_text()) {
            if wanted.len() >= WANTED_PER_REQUEST {
                break;
            }
            if already.contains(&id) || wanted.contains(&id) {
                continue;
            }
            // `peek`, not `retrieve`: reading a fragment to decide whether to
            // inline it is not the model asking for it, and must not look like
            // use to the store's retention clock or like a retrieval to the
            // statistics.
            let Some(fragment) = state.core.peek(identity, &id) else {
                continue;
            };
            if query_is_about(&query, &fragment) {
                wanted.push(id);
            }
        }
    }
    if !wanted.is_empty() {
        tracing::debug!(
            count = wanted.len(),
            "keeping fragments inline because this turn appears to ask about them"
        );
        state.prefixes.record_wanted(conversation, &wanted);
    }
    wanted
}

/// The request's mode: its own header, then the tenant's, then the provider's
/// (`proxy.providers`), then the deployment's.
///
/// `proxy.compress = false` — the deployment's, or the provider's — is checked
/// before any of them: it is the
/// deployment saying it does not compress, and a switch a client header can
/// turn back on is not one.
///
/// Resolving it here rather than short-circuiting further down is what makes
/// the run say so. The mode is what the response reports on
/// `x-kompressor-mode`, so a proxy forwarding everything unchanged while
/// announcing `recoverable` is a control arm that cannot be told from a
/// compressed one after the fact.
///
/// `proxy.enabled` is checked too, for an embedder that mounts
/// [`proxy_router`](crate::proxy_router) directly. It is not the same switch:
/// under `serve` a disabled proxy is never mounted and every proxied path 404s,
/// so it cannot be a baseline over the same network path.
pub(crate) fn resolve_mode(
    state: &ProxyState,
    headers: &HeaderMap,
    identity: &Identity,
    provider: Option<&str>,
) -> ProxyMode {
    let config = state.core.config_for(provider);
    let proxy = &config.proxy;
    if !proxy.compress || !proxy.enabled {
        return ProxyMode::Off;
    }
    if let Some(mode) = headers
        .get(headers::MODE)
        .and_then(|value| value.to_str().ok())
        .and_then(ProxyMode::parse)
    {
        return mode;
    }
    if let Some(mode) = identity.mode {
        return ProxyMode::On(mode);
    }
    ProxyMode::On(config.engine.mode)
}

#[allow(clippy::too_many_arguments)]
async fn buffered_response(
    state: ProxyState,
    outbound: Outbound,
    family: Option<ApiFamily>,
    retriever: Option<Retriever>,
    budget: Budget,
    prepared: Prepared,
    recorder: Recorder,
    request_id: String,
    mode: ProxyMode,
    started: Instant,
) -> Response {
    let relayed = match nonstream::run(
        outbound,
        family,
        retriever.as_ref(),
        budget,
        &state.failopen,
    )
    .await
    {
        Ok(relayed) => relayed,
        Err(error) => {
            recorder.finish(None, 0, 0, None, started, Some(error.to_string()));
            return error_response(
                StatusCode::BAD_GATEWAY,
                "the upstream could not be reached",
                &request_id,
            );
        }
    };

    let mut response_headers = headers::client_headers(&relayed.headers);
    headers::annotate(
        &mut response_headers,
        &request_id,
        mode.as_str(),
        prepared.saved_tokens,
        relayed.rounds,
    );
    let mut recorder = recorder;
    if family == Some(ApiFamily::AnthropicMessages) {
        recorder.stop_reason = serde_json::from_slice::<Value>(&relayed.body)
            .ok()
            .and_then(|body| body.get("stop_reason")?.as_str().map(str::to_string));
    }
    recorder.finish(
        Some(relayed.status.as_u16()),
        relayed.rounds,
        relayed.injected_tokens,
        relayed.usage.as_ref(),
        started,
        None,
    );

    let mut response = Response::new(Body::from(relayed.body));
    *response.status_mut() = relayed.status;
    *response.headers_mut() = response_headers;
    response
}

#[allow(clippy::too_many_arguments)]
async fn stream_response(
    state: ProxyState,
    outbound: Outbound,
    family: Option<ApiFamily>,
    retriever: Option<Retriever>,
    budget: Budget,
    prepared: Prepared,
    recorder: Recorder,
    request_id: String,
    mode: ProxyMode,
    started: Instant,
) -> Response {
    let upstream = match outbound.send().await {
        Ok(response) => response,
        Err(error) => {
            recorder.finish(None, 0, 0, None, started, Some(error.to_string()));
            return error_response(
                StatusCode::BAD_GATEWAY,
                "the upstream could not be reached",
                &request_id,
            );
        }
    };

    let status = upstream.status();
    let mut response_headers = headers::client_headers(upstream.headers());
    // Rounds are reported as zero on a streamed response and cannot be
    // anything else: the headers go out before the first frame arrives, and a
    // retrieve round happens after it. The count the operator needs is on the
    // request's stats event, which is written when the relay finishes.
    headers::annotate(
        &mut response_headers,
        &request_id,
        mode.as_str(),
        prepared.saved_tokens,
        0,
    );

    // An error, or a response that is not an event stream after all, is a whole
    // body: relaying it as one is both simpler and what the client expects.
    if !status.is_success() || !is_event_stream(upstream.headers(), prepared.stream) {
        let body = upstream.bytes().await.unwrap_or_default();
        recorder.finish(Some(status.as_u16()), 0, 0, None, started, None);
        let mut response = Response::new(Body::from(body));
        *response.status_mut() = status;
        *response.headers_mut() = response_headers;
        return response;
    }

    // A family we do not walk has no turn to intercept, so its stream skips the
    // machine entirely and is piped through. Buffering it to look for a tool
    // call we could not have injected would hold every token back until the
    // provider finished the answer.
    let Some(family) = family else {
        recorder.finish(Some(status.as_u16()), 0, 0, None, started, None);
        let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
        *response.status_mut() = status;
        *response.headers_mut() = response_headers;
        return response;
    };

    let config = state.core.config();
    let relay = StreamRelay {
        outbound,
        family,
        retriever,
        budget,
        buffer_limit: config.proxy.stream_buffer_bytes,
        buffer_timeout: config.proxy.stream_buffer_timeout,
        intercept_limit: config.proxy.stream_intercept_bytes,
        intercept_timeout: config.proxy.stream_intercept_timeout,
        keepalive: config.proxy.stream_keepalive,
        content_grace: config.proxy.stream_content_grace_bytes,
        strip_usage: prepared.strip_usage,
        failopen: state.failopen.clone(),
        log_payloads: config.log.payloads,
        capture: bodycapture::path(),
    };

    let (tx, rx) = tokio::sync::mpsc::channel::<ClientChunk>(32);
    let mut recorder = recorder;
    tokio::spawn(async move {
        let outcome = relay.run(upstream, tx).await;
        recorder.stop_reason = outcome.stop_reason.clone();
        // A stream the client walked out on still gets a row, and the row says
        // so. Without the note it is a healthy 200 carrying usage for tokens
        // that reached nobody, which is worse than no row at all: it is a row
        // that lies.
        let failure = if outcome.client_gone {
            Some("client disconnected before the stream completed".to_string())
        } else if outcome.upstream_error {
            outcome
                .error_detail
                .clone()
                .or_else(|| Some("upstream stream ended early".to_string()))
        } else {
            None
        };
        recorder.finish(
            Some(status.as_u16()),
            outcome.rounds,
            outcome.injected_tokens,
            outcome.usage.as_ref(),
            started,
            failure,
        );
    });

    let body = Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|chunk| (chunk, rx))
    }));
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;
    response
}

/// Whether the upstream's response is the event stream the client asked for.
///
/// `asked_for_stream` is what the request said, and it decides the case where
/// the upstream says nothing: **chatgpt.com returns the Responses SSE stream
/// with no `Content-Type` at all** (measured 2026-09-22 against
/// `/backend-api/codex/responses`). Requiring the header there sent every
/// subscription stream down the buffered path — the relay never ran, so a
/// `optscale_kompressor_retrieve` call was never intercepted and reached the client
/// instead, which rejected it and answered from what was left. A wrong answer,
/// caused by compression, from a missing header.
///
/// A type that *is* present is still believed over what we asked for: an
/// upstream that says `application/json` is answering something other than the
/// stream, and treating that as SSE would hand the client an empty body.
fn is_event_stream(headers: &HeaderMap, asked_for_stream: bool) -> bool {
    match headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    {
        Some(value) => value.contains("event-stream"),
        None => asked_for_stream,
    }
}

/// The proxy's own errors, in the shape every provider client already parses.
fn error_response(status: StatusCode, message: &str, request_id: &str) -> Response {
    let body = json!({"error": {"message": message, "type": "kompressor_proxy_error"}});
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    if let Ok(value) = http::HeaderValue::from_str(request_id) {
        response
            .headers_mut()
            .insert(http::HeaderName::from_static(headers::REQUEST_ID), value);
    }
    response
}

/// Holds the half-built proxy event until the request is over.
pub(crate) struct Recorder {
    core: Arc<AppCore>,
    event: ProxyRequestEvent,
    /// Where to file what the provider reports about this request's frozen
    /// prefix. This is the half that makes the next turn's decision possible:
    /// without it the tracker never learns anything and every prefix stays
    /// `Warm` forever, which is the state this feature exists to leave.
    prefixes: Arc<prefixcache::PrefixTracker>,
    prefix: Option<u64>,
    /// Provider wait, accumulated by the `Outbound` this request sends through.
    /// Shared rather than copied so the retrieve loop's extra rounds are in it.
    waited: Arc<AtomicU64>,
    /// The keep-warm ping to arm if the response stops on `tool_use`.
    pub(crate) keep_warm: Option<KeepWarmPlan>,
    /// How the model stopped, where the path that relayed it could tell.
    pub(crate) stop_reason: Option<String>,
}

impl Recorder {
    fn new(
        // The state rather than its pieces: `core` and `prefixes` both come
        // from here, and threading them separately pushed this past the
        // argument limit for no gain in clarity.
        state: &ProxyState,
        identity: &Identity,
        request_id: &str,
        family: Option<ApiFamily>,
        prepared: &Prepared,
        upstream: &str,
        // The provider wait, accumulated by the `Outbound` this request sends
        // through. Shared so the retrieve loop's extra rounds land in it.
        waited: Arc<AtomicU64>,
    ) -> Recorder {
        let event = ProxyRequestEvent {
            time_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
            tenant: identity.tenant.as_str().to_string(),
            request_id: request_id.to_string(),
            api_family: family
                .map(ApiFamily::as_str)
                .unwrap_or("passthrough")
                .to_string(),
            model: prepared.model.clone(),
            upstream: upstream.to_string(),
            status: None,
            stream: prepared.stream,
            // Recorded because a stripped usage frame is a request the client
            // cannot account for, and an agent's self-reported totals then
            // fall short of what this proxy measured.
            strip_usage: prepared.strip_usage,
            prompt_tokens: None,
            completion_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            cache_write_1h_tokens: None,
            shadow_prompt_tokens: None,
            // Hex rather than the raw u64: SQLite integers are signed, so
            // half the fingerprint space would be written as a negative
            // number and read back as one. A fixed-width string groups and
            // sorts cleanly and is the same value on every proxy.
            conversation: prepared.prefix.map(|prefix| format!("{prefix:016x}")),
            retrieve_rounds: 0,
            retrieve_tokens_injected: 0,
            upstream_ms: None,
            total_ms: None,
            error: None,
            keep_warm: false,
        };
        Recorder {
            core: state.core.clone(),
            event,
            waited,
            prefixes: state.prefixes.clone(),
            prefix: prepared.prefix,
            keep_warm: None,
            stop_reason: None,
        }
    }

    /// Arm the keep-warm ping if this request left the client owing the model
    /// a tool result, which is the one case where its next request is sure to
    /// come and to extend this conversation.
    fn arm_keep_warm(&mut self, started: Instant) {
        let Some(plan) = self.keep_warm.take() else {
            return;
        };
        let succeeded = self.event.error.is_none()
            && self
                .event
                .status
                .is_some_and(|status| (200..300).contains(&status));
        let prompt = self
            .event
            .prompt_tokens
            .unwrap_or(0)
            .saturating_add(self.event.cache_read_tokens.unwrap_or(0))
            .saturating_add(self.event.cache_write_tokens.unwrap_or(0));
        if !succeeded
            || self.stop_reason.as_deref() != Some("tool_use")
            || prompt < crate::keepwarm::MIN_PREFIX_TOKENS
        {
            return;
        }
        let keepwarm = plan.state.keepwarm.clone();
        keepwarm.arm(plan.state, plan.ping, plan.schedule, started);
    }

    fn finish(
        mut self,
        status: Option<u16>,
        rounds: u32,
        injected: u32,
        usage: Option<&Value>,
        started: Instant,
        error: Option<String>,
    ) {
        self.event.status = status;
        self.event.retrieve_rounds = rounds;
        self.event.retrieve_tokens_injected = injected;
        self.event.total_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
        // Left null when nothing was sent — a request refused before the
        // provider was reached waited zero, and writing a 0.0 would put it in
        // the same bucket as one that answered instantly.
        let waited = self.waited.load(Ordering::Relaxed);
        self.event.upstream_ms = (waited > 0).then(|| waited as f64 / 1000.0);
        self.event.error = error;
        if let Some(usage) = usage {
            self.event.prompt_tokens = billed(usage, &["prompt_tokens", "input_tokens"]);
            self.event.completion_tokens = billed(usage, &["completion_tokens", "output_tokens"]);
            self.event.cache_read_tokens =
                billed(usage, &["cache_read_input_tokens"]).or_else(|| {
                    usage
                        .pointer("/prompt_tokens_details/cached_tokens")
                        .or_else(|| usage.pointer("/input_tokens_details/cached_tokens"))
                        .and_then(Value::as_u64)
                        .map(|tokens| tokens as u32)
                });
            self.event.cache_write_tokens = billed(usage, &["cache_creation_input_tokens"]);
            // The TTL split, which prices the write: 1.25x input for five-minute
            // entries, 2x for one-hour ones.
            self.event.cache_write_1h_tokens = usage
                .pointer("/cache_creation/ephemeral_1h_input_tokens")
                .and_then(Value::as_u64)
                .map(|tokens| tokens as u32);
        }
        // Filed only when this request actually froze a prefix. `None` for
        // cache reads means the response carried no usage at all, which is not
        // evidence either way and is stored as such.
        if let Some(prefix) = self.prefix {
            // Anthropic reports `input_tokens` as the uncached remainder, with
            // reads and writes beside it; OpenAI's `prompt_tokens` already
            // includes its cached part. The tracker needs the whole prompt
            // either way, and the event keeps the provider's own figure.
            let anthropic = usage.is_some_and(|usage| {
                usage.get("cache_read_input_tokens").is_some()
                    || usage.get("cache_creation_input_tokens").is_some()
            });
            let written = self.event.cache_write_tokens.unwrap_or(0);
            let prompt = if anthropic {
                self.event.prompt_tokens.map(|uncached| {
                    uncached
                        .saturating_add(self.event.cache_read_tokens.unwrap_or(0))
                        .saturating_add(written)
                })
            } else {
                self.event.prompt_tokens
            };
            self.prefixes
                .record(prefix, self.event.cache_read_tokens, prompt, rounds);
            if anthropic
                && written > 0
                && self.event.cache_read_tokens.is_some()
                && prompt.is_some_and(|prompt| prompt > 0)
            {
                self.prefixes.record_cache_writes(written);
            }
        }
        self.arm_keep_warm(started);
        self.core.stats().send(Event::ProxyRequest(self.event));
    }
}

/// The first of several spellings a provider might use for the same number.
fn billed(usage: &Value, keys: &[&str]) -> Option<u32> {
    keys.iter()
        .find_map(|key| usage.get(*key).and_then(Value::as_u64))
        .map(|tokens| tokens as u32)
}

#[cfg(test)]
mod tests {
    use super::{RefId, inline_set_under_ceiling};
    use kompressor_engine::refs::ref_for;

    #[test]
    fn the_prompt_outside_the_messages_is_its_system_prompt_and_tools() {
        let messages_only = serde_json::json!({"messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(super::outside_tokens(&messages_only), 0);
        let full = serde_json::json!({
            "system": [{"type": "text", "text": "You are a coding agent. ".repeat(100)}],
            "tools": [{"name": "Bash", "description": "Run a command. ".repeat(50), "input_schema": {}}],
            "messages": [{"role": "user", "content": "hi"}]
        });
        let outside = super::outside_tokens(&full);
        assert!(outside > 500, "{outside}");
        let without_tools = serde_json::json!({"system": full["system"].clone(), "messages": []});
        assert!(super::outside_tokens(&without_tools) < outside);
    }

    fn priced(sizes: &[u32]) -> Vec<(RefId, u32)> {
        sizes
            .iter()
            .enumerate()
            .map(|(n, size)| (ref_for(&format!("fragment {n}")), *size))
            .collect()
    }

    fn message(role: &str, text: &str) -> super::Message {
        use kompressor_engine::messages::{Message, MessageContent};
        Message {
            role: role.to_string(),
            content: MessageContent::Text(text.to_string()),
            meta: Default::default(),
        }
    }

    #[test]
    fn a_promotion_that_rewrites_most_of_the_prompt_is_refused() {
        // The case measured on 2026-10-07, in miniature. A 175-token fragment
        // was promoted into a message near the front, 6,144 tokens of prefix
        // followed it, and the model would have had to fetch it 35 more times
        // to break even.
        let id = ref_for("the retry helper body");
        let carrier = format!("def retry(): ref:{id}");
        let messages = vec![
            message("user", "go"),
            message("tool", &carrier),
            // Everything after the carrier is what gets re-read.
            message("tool", &"x ".repeat(20_000)),
        ];
        let kept = super::affordable_inline(&messages, vec![id], 1_000_000, 0, |_| 175);
        assert!(
            kept.is_empty(),
            "a break this far ahead of what it protects must not be paid"
        );
    }

    #[test]
    fn a_promotion_near_the_end_of_the_prompt_is_cheap_and_allowed() {
        // Same fragment, same size, but nothing follows it: the prefix survives
        // and the promotion costs only the compression it forfeits.
        let id = ref_for("the retry helper body");
        let messages = vec![
            message("tool", &"x ".repeat(20_000)),
            message("tool", &format!("def retry(): ref:{id}")),
        ];
        let kept = super::affordable_inline(&messages, vec![id], 1_000_000, 0, |_| 175);
        assert_eq!(kept, vec![id], "nothing follows it, so there is no break");
    }

    #[test]
    fn a_fragment_already_inline_is_not_re_judged() {
        // No message carries the handle, so there is no marker to replace and
        // no break to pay. Dropping it here would churn the set every turn,
        // which is the prefix rewrite this whole rule exists to avoid.
        let id = ref_for("somewhere else entirely");
        let messages = vec![message("user", "go")];
        let kept = super::affordable_inline(&messages, vec![id], 1_000_000, 0, |_| 1);
        assert_eq!(kept, vec![id]);
    }

    #[test]
    fn the_set_stops_at_the_ceiling_and_says_what_it_dropped() {
        let set = priced(&[1_000, 1_000, 1_000]);
        let (kept, refused, spent) = inline_set_under_ceiling(&set, 2_000);
        assert_eq!((kept.len(), refused, spent), (2, 1, 2_000));
    }

    #[test]
    fn admission_is_in_order_so_it_survives_the_ceiling_rising() {
        // The property the provider cache depends on. The set only grows and
        // the context it is measured against only grows, so a fragment admitted
        // on one turn must still be admitted on the next -- otherwise a handle
        // reappears where inline text was, the prefix is rewritten, and
        // everything after it is billed at full price again.
        let early = priced(&[1_000, 1_000, 1_000]);
        let (first, _, _) = inline_set_under_ceiling(&early, 2_000);

        // A later turn: same set in the same order, plus one more, bigger
        // ceiling.
        let mut later = early.clone();
        later.push((ref_for("fragment 3"), 500));
        let (second, _, _) = inline_set_under_ceiling(&later, 3_500);

        assert!(
            first.iter().all(|id| second.contains(id)),
            "a fragment was evicted: {first:?} then {second:?}"
        );
        assert_eq!(second.len(), 4, "and the room freed up was used");
    }

    #[test]
    fn one_oversized_fragment_does_not_block_the_rest() {
        // Skipped rather than stopped: a single huge fragment at the head of
        // the queue would otherwise shut the whole mechanism down behind it,
        // and the small ones after it are the cheap ones to protect.
        let set = priced(&[10_000, 100, 100]);
        let (kept, refused, spent) = inline_set_under_ceiling(&set, 1_000);
        assert_eq!((kept.len(), refused, spent), (2, 1, 200));
    }

    #[test]
    fn a_fragment_the_store_lost_costs_nothing_and_stays() {
        // `peek` answers `None` for an expired or evicted fragment, which is
        // priced at zero. Dropping it here would be the wrong call twice over:
        // it is free to carry, and removing it from the set is the prefix
        // rewrite this whole ordering exists to avoid.
        let set = priced(&[0, 1_000]);
        let (kept, refused, _) = inline_set_under_ceiling(&set, 500);
        assert_eq!((kept.len(), refused), (1, 1));
    }

    #[test]
    fn a_ceiling_of_zero_keeps_nothing() {
        let set = priced(&[1, 1]);
        let (kept, refused, _) = inline_set_under_ceiling(&set, 0);
        assert!(kept.is_empty());
        assert_eq!(refused, 2);
    }

    /// The relevance rule that decides a proactive expansion.
    #[test]
    fn a_turn_is_about_a_fragment_only_on_several_substantial_words() {
        let fragment =
            "ERROR in src/payments/refund.rs: ledger reconciliation failed for order 4471";

        // Two substantial words in common: this turn is about that fragment.
        assert!(query_is_about("why did the refund ledger fail?", fragment));

        // One word is not enough. A single shared token is how an unscoped
        // expansion turns a short message into someone else's tool output.
        assert!(!query_is_about("tell me about the ledger", fragment));

        // Short words are excluded: `the`, `and`, `src`, `for` appear in
        // almost any fragment and would expand everything.
        assert!(!query_is_about("the and for src", fragment));

        // Unrelated text matches nothing.
        assert!(!query_is_about("what is the weather in Glasgow", fragment));

        // An empty query asks about nothing.
        assert!(!query_is_about("", fragment));

        // Case and punctuation do not matter.
        assert!(query_is_about("REFUND -- LEDGER?", fragment));
    }

    use super::*;

    #[test]
    fn a_shared_preamble_does_not_merge_two_conversations() {
        // The reason the identity is learned rather than derived. Two tasks
        // behind one constant preamble must not land on one key: under
        // `first_seen` they would share a watermark, and a floor that may fall
        // but never rise means the fresher conversation drags the older one's
        // floor to 0 and its next turn rewrites bytes the provider holds.
        let tracker = prefixcache::PrefixTracker::new(Duration::ZERO);
        let preamble = "you are a helpful terminal agent";
        let identify = |task: &str| {
            let opening = conversation_opening(&[
                Message::text("developer", preamble),
                Message::text("user", task),
            ]);
            let refs: Vec<&str> = opening.iter().map(String::as_str).collect();
            tracker.identify(&prefixcache::identity_chain(&refs))
        };
        let one = identify("fix the failing test in parser.rs");
        let two = identify("add a changelog entry");
        assert!(one.is_some() && two.is_some());
        assert_ne!(one, two, "the task must reach the identity");
    }

    #[test]
    fn identical_openings_do_not_share_prefix_state_across_tenants_or_models() {
        use kompressor_core::Config;
        use kompressor_store::memory::MemoryRefStore;
        use kompressor_store::types::Tenant;

        let core = kompressor_core::AppCore::new(
            Config::default(),
            Arc::new(MemoryRefStore::new()),
            kompressor_stats::writer::null_sink(),
        );
        let state = ProxyState::new(Arc::new(core), reqwest::Client::new());
        let make_identity = |tenant: &str| Identity {
            tenant: Tenant::new(tenant),
            mode: None,
            trusted: false,
        };
        let body = |model: &str| {
            Bytes::from(
                json!({
                    "model": model,
                    "messages": [{"role": "user", "content": "same opening"}]
                })
                .to_string(),
            )
        };
        let get = |identity: &Identity, provider: &str, model: &str| {
            prefix_identity(
                &state,
                identity,
                Some(ApiFamily::OpenAiChat),
                &body(model),
                provider,
            )
        };

        let tenant_a = make_identity("tenant-a");
        let tenant_b = make_identity("tenant-b");
        let first = get(&tenant_a, "openai", "gpt-5");
        assert_eq!(get(&tenant_a, "openai", "gpt-5"), first);
        assert_ne!(get(&tenant_b, "openai", "gpt-5"), first);
        assert_ne!(get(&tenant_a, "anthropic", "gpt-5"), first);
        assert_ne!(get(&tenant_a, "openai", "gpt-4"), first);
        assert_eq!(state.prefixes.tracked(), 4);
    }

    #[test]
    fn a_conversation_is_still_itself_twenty_turns_later() {
        // The property the tracker turns on. A later turn APPENDS, so the
        // opening it was first seen with is still its opening and must resolve
        // to the same identity — the failure this replaces moved the key every
        // turn, giving 23 keys across 24 requests of one conversation.
        let tracker = prefixcache::PrefixTracker::new(Duration::ZERO);
        let mut messages = vec![
            Message::text("developer", "preamble"),
            Message::text("user", "the opening ask"),
        ];
        let identify = |messages: &[Message]| {
            let opening = conversation_opening(messages);
            let refs: Vec<&str> = opening.iter().map(String::as_str).collect();
            tracker.identify(&prefixcache::identity_chain(&refs))
        };
        let turn_one = identify(&messages);
        assert!(turn_one.is_some());
        for turn in 0..20 {
            messages.push(Message::text("assistant", format!("reply {turn}")));
            messages.push(Message::text("user", format!("follow-up {turn}")));
            assert_eq!(
                identify(&messages),
                turn_one,
                "turn {turn} changed identity"
            );
        }
    }

    #[test]
    fn identical_openings_that_part_are_two_conversations() {
        // Three attempts of one Harbor task open byte-identically. Filed as
        // one conversation, the later attempts inherited the first one's
        // cache-lifetime flag and wrote one-hour entries from their first
        // request.
        let tracker = prefixcache::PrefixTracker::new(Duration::ZERO);
        let identify = |texts: &[String]| {
            let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
            tracker.identify(&prefixcache::identity_chain(&refs))
        };
        let attempt = |name: &str, turns: usize| {
            let mut texts = vec!["preamble".to_string(), "the task".to_string()];
            for turn in 0..turns {
                texts.push(format!("{name} call {turn}"));
                texts.push(format!("{name} result {turn}"));
            }
            texts
        };

        let first = identify(&attempt("a", 0));
        assert_eq!(identify(&attempt("a", 4)), first);
        // Its opening alone cannot be told apart, and nothing has parted yet.
        assert_eq!(identify(&attempt("b", 0)), first);

        let second = identify(&attempt("b", 1));
        assert_ne!(second, first, "a departure is another conversation");
        for turns in 2..6 {
            assert_eq!(identify(&attempt("b", turns)), second, "turn {turns}");
            assert_eq!(identify(&attempt("a", turns + 4)), first, "turn {turns}");
        }
        let third = identify(&attempt("c", 1));
        assert!(third != first && third != second);
    }

    #[test]
    fn a_rewritten_tail_is_still_the_same_conversation() {
        // The one rewrite measured in 958 requests reached two messages back.
        let tracker = prefixcache::PrefixTracker::new(Duration::ZERO);
        let identify = |texts: &[&str]| tracker.identify(&prefixcache::identity_chain(texts));
        let id = identify(&["preamble", "task"]);
        assert_eq!(
            identify(&["preamble", "task", "call", "result", "reminder"]),
            id
        );
        assert_eq!(
            identify(&["preamble", "task", "call", "retried", "reminder"]),
            id
        );
        assert_eq!(
            identify(&["preamble", "task", "call", "retried", "reminder", "next"]),
            id
        );
        // A replay of an earlier turn is not a departure either.
        assert_eq!(identify(&["preamble", "task", "call"]), id);
    }

    #[test]
    fn a_conversation_that_opens_inside_anothers_preamble_is_told_apart() {
        // Deepest match wins. A conversation first seen at two messages must
        // not swallow one that opens with the same two and goes elsewhere.
        let tracker = prefixcache::PrefixTracker::new(Duration::ZERO);
        let identify = |texts: &[&str]| tracker.identify(&prefixcache::identity_chain(texts));
        let shallow = identify(&["preamble", "task one"]);
        let deeper = identify(&["preamble", "task two", "and more"]);
        assert_ne!(shallow, deeper);
        // Each is still recognised as itself once both are known.
        assert_eq!(identify(&["preamble", "task one", "later turn"]), shallow);
        assert_eq!(
            identify(&["preamble", "task two", "and more", "later"]),
            deeper
        );
    }

    #[test]
    fn an_empty_conversation_has_no_identity() {
        let tracker = prefixcache::PrefixTracker::new(Duration::ZERO);
        assert!(tracker.identify(&[]).is_none());
        assert!(conversation_opening(&[]).is_empty());
    }

    #[test]
    fn a_streamed_request_is_recognised_without_being_parsed() {
        let headers = HeaderMap::new();
        assert!(unparsed_asks_to_stream(
            br#"{"model":"gpt-5","stream":true,"messages":[]}"#,
            &headers
        ));
        assert!(unparsed_asks_to_stream(br#"{"stream" : true }"#, &headers));
        assert!(!unparsed_asks_to_stream(
            br#"{"stream":false,"messages":[]}"#,
            &headers
        ));
        assert!(!unparsed_asks_to_stream(br#"{"messages":[]}"#, &headers));
        assert!(!unparsed_asks_to_stream(
            br#"{"note":"the word stream appears here"}"#,
            &headers
        ));

        let mut accepts = HeaderMap::new();
        accepts.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("text/event-stream"),
        );
        assert!(
            unparsed_asks_to_stream(br#"{}"#, &accepts),
            "some SDKs say it in the Accept header and nowhere else"
        );
    }

    #[test]
    fn a_stream_with_no_content_type_is_still_the_stream_we_asked_for() {
        // chatgpt.com sends the Responses SSE stream with no Content-Type.
        // Requiring the header put every subscription stream on the buffered
        // path, where the relay never runs — so the model's retrieve call went
        // to the client, which rejected it, and the answer was wrong.
        let bare = HeaderMap::new();
        assert!(is_event_stream(&bare, true));
        // Nothing was asked for, so nothing is assumed.
        assert!(!is_event_stream(&bare, false));
    }

    #[test]
    fn a_content_type_that_is_present_is_believed_over_what_we_asked_for() {
        // An upstream answering JSON to a streamed request is answering
        // something else — an error, most likely. Relaying that as SSE would
        // find no frames in it and hand the client an empty body.
        let mut json = HeaderMap::new();
        json.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        assert!(!is_event_stream(&json, true));

        let mut sse = HeaderMap::new();
        sse.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        assert!(is_event_stream(&sse, true));
        // A provider that labels the stream is believed whatever we asked.
        assert!(is_event_stream(&sse, false));
    }
}
