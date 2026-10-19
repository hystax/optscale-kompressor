//! The streamed path: the relay state machine of §12.9.
//!
//! This is the case that gives the crate its shape. A server-side retrieve loop
//! that runs only on buffered responses is worse than no loop at all: every
//! `optscale_kompressor_retrieve` call on a streamed response reaches a client that never
//! declared the tool and comes back "unsupported call", while the elision it was
//! meant to undo stays elided. A compression mode whose recovery path works only
//! when the client happens not to be streaming is not a recovery path.
//!
//! The machine has three states:
//!
//! - **Buffering.** Read frames without forwarding any, until the stream says
//!   what kind of turn this is. Tool-call *names* arrive in the first frame of
//!   a call on all three families, so this window is normally a handful of
//!   frames; it costs time to first byte only on turns that actually retrieve.
//!   It is bounded by `proxy.stream_buffer_bytes`, and overflowing it means
//!   passthrough rather than unbounded memory.
//! - **Intercepting.** The turn is ours alone. Consume the rest of it, execute
//!   the retrievals, open the next upstream stream, and start over. The client
//!   has seen nothing of this round and never will — it asked for one
//!   completion. Keep-alive comments go out while this happens, or client SDK
//!   read timeouts fire on a round that takes longer than they expect a first
//!   byte to.
//! - **Passthrough.** Relay frames as bytes, rewriting nothing except a usage
//!   frame once a round has been hidden from the client's accounting.
//!
//! A turn that mixes our call with any other is replayed to the client
//! verbatim, from the buffer, exactly as it arrived: half a turn cannot be
//! satisfied, and a client that never saw the call it is expected to answer
//! cannot continue the conversation.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;

use kompressor_wire::ApiFamily;

use crate::failopen::{FailOpen, Stage};
use crate::retrieve::{Budget, Retriever};
use crate::sse::{SseDecoder, SseFrame, parse as parse_frame};
use crate::tool::RETRIEVE_TOOL_NAME;
use crate::turn::{self, ModelToolCall};
use crate::upstream::Outbound;

/// What a client-bound channel carries. The error type exists only because
/// `axum::body::Body::from_stream` wants a fallible stream; nothing produces
/// one.
pub(crate) type ClientChunk = Result<Bytes, std::io::Error>;

/// Sent to the client while a retrieve round is in flight.
///
/// A comment is a valid SSE frame that every client ignores, which is what
/// makes it a keep-alive rather than an event the client has to understand.
/// Sent while a retrieve round is in flight.
const KEEPALIVE: &[u8] = b": kompressor retrieving\n\n";

/// Sent while the relay is still deciding whose turn this is.
///
/// A distinct comment because it means something different: nothing is being
/// fetched, the frames the provider has sent so far are simply being held back
/// until the machine knows whether this turn is ours. Both are SSE comments, so
/// no client parses either.
const KEEPALIVE_BUFFERING: &[u8] = b": kompressor\n\n";

/// What the relay did, for the stats event the request records at the end.
#[derive(Debug, Default)]
pub(crate) struct StreamOutcome {
    pub(crate) rounds: u32,
    pub(crate) injected_tokens: u32,
    pub(crate) usage: Option<Value>,
    /// The client hung up before the stream finished.
    ///
    /// Recorded because the row is otherwise indistinguishable from a completed
    /// request: same status, same upstream, a plausible duration. An operator
    /// counting statuses would see a healthy 200 for a completion nobody ever
    /// received.
    ///
    /// Such a row usually carries **no usage**, and deliberately so. Usage
    /// arrives in the stream's final frame, and waiting for it means holding a
    /// connection open for a client that has gone — which is what hid this case
    /// in the first place. The provider bills either way; an abandoned row says
    /// the cost is unknown rather than guessing at it.
    pub(crate) client_gone: bool,
    pub(crate) upstream_error: bool,
}

/// The state machine's configured environment.
pub(crate) struct StreamRelay {
    pub(crate) outbound: Outbound,
    pub(crate) family: ApiFamily,
    /// `None` once no further round may be intercepted: client-side execution,
    /// or a budget that has already been refused once.
    pub(crate) retriever: Option<Retriever>,
    pub(crate) budget: Budget,
    pub(crate) buffer_limit: usize,
    /// How long the relay may hold frames back before giving up on deciding.
    ///
    /// The byte limit above bounds how *much* is held; this bounds how *long*.
    /// Both exist for the same reason and cost the same thing — see
    /// `should_stop_buffering`. `Duration::ZERO` disables it.
    pub(crate) buffer_timeout: Duration,
    /// How much may be held once the turn is known to be ours.
    ///
    /// `buffer_limit` bounds the *deciding* window and stops applying the
    /// moment the machine decides, which left interception with no bound at
    /// all: a turn that opened with a retrieve call and then kept generating
    /// held 2.5 MB across 9,392 frames for five full minutes, twice, until the
    /// client gave up and retried. Ten minutes of a seventeen-minute run.
    pub(crate) intercept_limit: usize,
    /// How long interception may run before the relay gives up on it.
    ///
    /// Deliberately shorter than any client's first-byte timeout. The two
    /// legitimate retrieve rounds measured on the same workload finished in
    /// fourteen seconds; a minute and a half is six times that and half of the
    /// five minutes the client waits.
    pub(crate) intercept_timeout: Duration,
    pub(crate) keepalive: Duration,
    /// How much content may be held while waiting to see whether a retrieve
    /// call follows it. `proxy.stream_content_grace_bytes`; 0 gives up at the
    /// first byte, which is what this did before.
    pub(crate) content_grace: usize,
    /// The client did not ask for usage; we did, to measure the saving.
    pub(crate) strip_usage: bool,
    pub(crate) failopen: Arc<FailOpen>,
    /// Log every frame, with what the machine made of it.
    ///
    /// `proxy.log.payloads`. Off by default because a frame carries model
    /// output verbatim — but when interception does not fire, the sequence of
    /// (frame kind → signal → state) is the whole of the evidence, and there
    /// is no way to reconstruct it after the fact.
    pub(crate) log_payloads: bool,
    /// Where to record the decision this relay makes about each frame, when
    /// `KOMPRESSOR_BODY_CAPTURE` names a file.
    ///
    /// The same file the request bodies go to, so a turn reads as one timeline:
    /// what we sent, then what came back and what the relay did about it.
    ///
    /// `log.payloads` already traces this at debug level and carries the same
    /// fields, but it is all-or-nothing -- it also logs retrieved fragments,
    /// which are tool output and can hold anything the command printed, so it
    /// is not something to leave on. This records the decision without the
    /// contents.
    pub(crate) capture: Option<std::path::PathBuf>,
}

/// What handing held frames to the client did.
enum HandOver {
    /// The client has them. `elided` says whether a call of ours was taken out.
    ///
    /// Scrubbing is armed by *removal*, not by hand-over. Arming it on every
    /// hand-over looked like the fix for a turn whose first signal is the
    /// client's call — `hand_over` sees only the frames it holds, so ours may
    /// not have arrived yet — but it armed the rewrite on turns where nothing
    /// of ours was ever present. On Responses that is harmless, because `scrub`
    /// there only removes items named ours; on Chat and Anthropic it is not:
    /// those arms rewrite `finish_reason`/`stop_reason` from the foreign-call
    /// flag alone, so a legitimate client tool call had its stop reason turned
    /// into `stop`/`end_turn` and the client stopped dispatching it. Reproduced
    /// with `proxy.compress = false`, where there is no call of ours at all.
    ///
    /// The ordering case is covered instead by arming when a passthrough frame
    /// of ours is dropped — removal, wherever it happens.
    Sent {
        elided: bool,
    },
    ClientGone,
}

/// A frame the relay is holding, and the call it belongs to.
struct Held {
    frame: SseFrame,
    call: Option<usize>,
}

/// How one upstream stream ended.
enum RoundEnd {
    /// The client has the whole answer.
    Finished {
        usage: Value,
    },
    /// The turn was ours; here is what it asked for.
    Intercepted {
        turn: StreamTurn,
        usage: Value,
    },
    /// The client hung up. Nothing left to do for it.
    ClientGone,
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Buffering,
    Intercepting,
    Passthrough,
}

impl StreamRelay {
    /// Drive the relay to completion, writing the client's stream into `tx`.
    pub(crate) async fn run(
        mut self,
        first: reqwest::Response,
        tx: Sender<ClientChunk>,
    ) -> StreamOutcome {
        let mut stream: BoxStream<'static, reqwest::Result<Bytes>> = Box::pin(first.bytes_stream());
        let mut outcome = StreamOutcome::default();
        // usage of the rounds the client never sees, folded into the usage
        // frame of the round it does
        let mut hidden = json!({});
        let mut total = json!({});
        let mut refusals = 0u32;

        loop {
            let intercept = self.retriever.is_some();
            let end = self
                .read_round(&mut stream, &tx, outcome.rounds, &hidden, intercept)
                .await;
            let (turn, usage) = match end {
                RoundEnd::ClientGone => {
                    outcome.client_gone = true;
                    break;
                }
                RoundEnd::Failed => {
                    outcome.upstream_error = true;
                    break;
                }
                RoundEnd::Finished { usage } => {
                    turn::add_usage(&mut total, &usage);
                    break;
                }
                RoundEnd::Intercepted { turn, usage } => (turn, usage),
            };
            turn::add_usage(&mut total, &usage);

            if self.budget.is_spent() {
                refusals += 1;
                if refusals > crate::nonstream::MAX_REFUSAL_ROUNDS {
                    self.recovery_error(&tx, "retrieve_loop_not_terminating")
                        .await;
                    outcome.upstream_error = true;
                    break;
                }
            }
            match self.next_round(&turn, &tx).await {
                Some(next) => {
                    turn::add_usage(&mut hidden, &usage);
                    outcome.rounds += 1;
                    outcome.injected_tokens = self.budget.tokens;
                    stream = next;
                }
                None => {
                    if tx.is_closed() {
                        outcome.client_gone = true;
                    } else {
                        self.recovery_error(&tx, "retrieve_round_failed").await;
                        outcome.upstream_error = true;
                    }
                    break;
                }
            }
        }

        let carried = total.as_object().is_some_and(|usage| !usage.is_empty());
        outcome.usage = carried.then_some(total);
        outcome
    }

    /// End a hidden recovery failure visibly, without inventing a successful answer.
    async fn recovery_error(&self, tx: &Sender<ClientChunk>, code: &str) {
        let message = "kompressor: could not complete the retrieval loop";
        let error = match self.family {
            ApiFamily::OpenAiChat => format!(
                "data: {}\n\n",
                json!({"error": {
                    "type": "kompressor_retrieve_loop", "code": code, "message": message
                }})
            ),
            ApiFamily::OpenAiResponses => format!(
                "event: error\ndata: {}\n\n",
                json!({
                    "type": "error", "code": code, "message": message
                })
            ),
            ApiFamily::AnthropicMessages => format!(
                "event: error\ndata: {}\n\n",
                json!({
                    "type": "error", "error": {"type": "api_error", "message": message, "code": code}
                })
            ),
        };
        let _ = tx.send(Ok(Bytes::from(error))).await;
    }

    /// Whether the relay has held the client waiting long enough to stop
    /// trying to decide.
    ///
    /// Buffering exists so a turn that turns out to be ours can be swallowed
    /// whole. It costs the client's patience: until the machine decides, the
    /// client sees nothing. The byte limit already bounds one side of that
    /// trade — an overflowing buffer relays rather than growing — and this
    /// bounds the other.
    ///
    /// It is needed because the heartbeat is not always enough. Keepalives are
    /// SSE *comments*, and a client is free to ignore comments when deciding
    /// whether a stream has gone idle; one real agent does exactly that, and
    /// gave up after five minutes having been sent thirty of them. Frames the
    /// client can actually see are the only thing guaranteed to reset its
    /// timer, so after this long the relay hands over what it has.
    ///
    /// **What it costs**: a round that flushes can no longer be intercepted, so
    /// a retrieve call arriving after the flush goes to the client, which did
    /// not declare the tool. That is the same trade the byte limit already
    /// makes, and it is the better one: losing one interception costs a
    /// retrieval, while losing the client costs the whole request, its tokens,
    /// and a retry.
    fn held_too_long(&self, since: std::time::Instant) -> bool {
        held_too_long(self.buffer_timeout, since.elapsed())
    }

    /// Read one upstream stream to its end, forwarding what the client should
    /// see.
    async fn read_round(
        &self,
        stream: &mut BoxStream<'static, reqwest::Result<Bytes>>,
        tx: &Sender<ClientChunk>,
        rounds: u32,
        hidden: &Value,
        intercept: bool,
    ) -> RoundEnd {
        let mut decoder = SseDecoder::new();
        let mut turn = StreamTurn::new(self.family);
        let mut buffered: Vec<Held> = Vec::new();
        let mut buffered_bytes = 0usize;
        // Content held so far in this round, bounded by `content_grace`.
        let mut content_bytes = 0usize;
        let mut usage = json!({});
        let mut state = State::Buffering;
        if self.log_payloads {
            tracing::debug!(intercept, rounds, "relay round begins");
        }
        // Set once our call has been taken out of what the client is being
        // sent, carrying whether the turn still has a call of the client's own.
        // The frames after the hand-over are relayed one at a time, and on the
        // Responses family the last of them repeats the entire output list —
        // which would put the call straight back in front of a client that has
        // just been spared it.
        let mut elided: Option<bool> = None;
        // Item ids of our own call, collected as its frames go by so the
        // argument deltas — which name no tool, only an `item_id` — can be
        // dropped with it.
        let mut our_items: Vec<String> = Vec::new();
        let held_since = std::time::Instant::now();
        let mut first_byte: Option<std::time::Instant> = None;

        loop {
            let intercepting = state == State::Intercepting;
            // Ping whenever the client can see nothing *because of us*. In
            // passthrough it is watching the provider's own stream, and a stall
            // there is the provider's to explain — inserting our own heartbeat
            // would hide it.
            let silent = state != State::Passthrough;
            // The bound that applies to whatever is being held right now:
            // the interception timeout once a retrieve call is being waited on,
            // the buffering timeout before that, and none in passthrough.
            // Zero means "no bound", as it does everywhere else here.
            let bound = match state {
                State::Passthrough => None,
                State::Intercepting => Some(self.intercept_timeout),
                State::Buffering => Some(self.buffer_timeout),
            }
            .filter(|bound| !bound.is_zero())
            .map(|bound| tokio::time::Instant::from_std(held_since + bound));
            let chunk = match next_chunk(stream, tx, self.keepalive, silent, intercepting, bound)
                .await
            {
                Pull::Chunk(chunk) => {
                    first_byte.get_or_insert_with(std::time::Instant::now);
                    chunk
                }
                Pull::Done => break,
                // The hold outlasted its bound with nothing arriving. Give the
                // client what is held and relay the rest, which is the same
                // answer every other bound in this module gives.
                Pull::HeldTooLong => {
                    tracing::warn!(
                        state = ?state,
                        held_ms = held_since.elapsed().as_millis() as u64,
                        "the upstream went quiet while the client was held; \
                         relaying the rest of this turn"
                    );
                    state = State::Passthrough;
                    let held = std::mem::take(&mut buffered);
                    match self
                        .hand_over(tx, &held, &turn, rounds, hidden, &mut our_items)
                        .await
                    {
                        HandOver::ClientGone => return RoundEnd::ClientGone,
                        HandOver::Sent { elided: true } => {
                            elided = Some(turn.has_foreign_call());
                        }
                        HandOver::Sent { elided: false } => {}
                    }
                    continue;
                }
                Pull::UpstreamError => {
                    let _ = tx
                        .send(Err(std::io::Error::other("upstream stream ended early")))
                        .await;
                    return RoundEnd::Failed;
                }
                // What the client was waiting on when it gave up. Without this
                // there is no way to tell "the provider sent nothing at all"
                // from "the provider was talking and we were holding it back" —
                // and only the second is something this proxy can fix.
                Pull::ClientGone => {
                    tracing::info!(
                        state = ?state,
                        held_ms = held_since.elapsed().as_millis() as u64,
                        first_byte_ms = first_byte.map(|t| t.duration_since(held_since).as_millis() as u64),
                        frames_held = buffered.len(),
                        bytes_held = buffered_bytes,
                        "client gave up while the relay was reading"
                    );
                    return RoundEnd::ClientGone;
                }
            };
            decoder.push(&chunk);

            // An event that never terminates is held by the decoder, not by
            // this loop, so it was outside both configured bounds: they count
            // complete frames and an unframed one is never counted at all.
            // `SseDecoder::pending` was written for exactly this accounting and
            // had no caller anywhere. Measured: `data: ` followed by a megabyte
            // with no blank line grew the decoder to a megabyte while
            // `buffered_bytes` stayed at zero.
            //
            // Over the limit, the relay gives up holding rather than keeps
            // growing — the same answer it gives a buffer that overflows on
            // complete frames. In passthrough there is nothing to give up, so
            // the bytes are simply forwarded as they frame.
            if state != State::Passthrough && decoder.pending() > self.buffer_limit {
                tracing::warn!(
                    pending = decoder.pending(),
                    buffer_limit = self.buffer_limit,
                    "an unterminated event passed the buffer limit; relaying \
                     the rest of this turn rather than holding it"
                );
                state = State::Passthrough;
                let held = std::mem::take(&mut buffered);
                match self
                    .hand_over(tx, &held, &turn, rounds, hidden, &mut our_items)
                    .await
                {
                    HandOver::ClientGone => return RoundEnd::ClientGone,
                    HandOver::Sent { elided: true } => {
                        elided = Some(turn.has_foreign_call());
                    }
                    HandOver::Sent { elided: false } => {}
                }
            }

            while let Some(frame) = decoder.next_frame() {
                if let Some(carried) = frame_usage(self.family, &frame) {
                    // The maximum, not the sum: these frames are two reports of
                    // one call, and summing them doubled the whole input side.
                    // Rounds still sum, in the caller.
                    turn::max_usage(&mut usage, &carried);
                }
                match state {
                    State::Passthrough => {
                        // Our call's own frames are dropped rather than
                        // rewritten: there is nothing in them for the client.
                        //
                        // Gated on having a retriever, not on `elided`. Without
                        // one this request never offered the tool, so a
                        // `optscale_kompressor_retrieve` call in the stream is the
                        // *client's* own and removing it would be the proxy
                        // eating a call it does not own. Dropping one of ours
                        // here is also what arms the rewrite, which covers the
                        // ordering `hand_over` cannot see.
                        if self.retriever.is_some() {
                            // Chat coalesces several calls into one frame, so
                            // ours is filtered out of it rather than the frame
                            // being dropped whole — the client's call and its
                            // text are in the same bytes.
                            if self.family == ApiFamily::OpenAiChat {
                                match filter_our_chat_calls(&frame, &mut our_items) {
                                    ChatFilter::Dropped => {
                                        elided = Some(turn.has_foreign_call());
                                        continue;
                                    }
                                    ChatFilter::Rewritten(bytes) => {
                                        elided = Some(turn.has_foreign_call());
                                        if !self.emit_rewritten(tx, bytes, rounds, hidden).await {
                                            return RoundEnd::ClientGone;
                                        }
                                        continue;
                                    }
                                    ChatFilter::Untouched => {}
                                }
                            } else if belongs_to_our_call(self.family, &frame, &mut our_items) {
                                elided = Some(turn.has_foreign_call());
                                continue;
                            }
                        }
                        let rewritten =
                            elided.and_then(|others| scrub(self.family, &frame, others));
                        let sent = match rewritten {
                            Some(bytes) => self.emit_rewritten(tx, bytes, rounds, hidden).await,
                            None => self.emit(tx, &frame, rounds, hidden).await,
                        };
                        if !sent {
                            return RoundEnd::ClientGone;
                        }
                        continue;
                    }
                    State::Buffering | State::Intercepting => {}
                }

                let (signal, call) = turn.observe(&frame);
                if self.log_payloads {
                    tracing::debug!(
                        state = ?state,
                        signal = ?signal,
                        intercept,
                        bytes = buffered_bytes,
                        frame = %String::from_utf8_lossy(frame.raw()).trim(),
                        "relay frame"
                    );
                }
                let frame_len = frame.raw().len();
                if let Some(path) = self.capture.as_ref() {
                    // Why the relay did what it did, frame by frame. The
                    // question this answers is the one the request bodies
                    // cannot: a retrieve call that reached the client did so
                    // because the turn had already gone passthrough, and only
                    // the running content total says why.
                    crate::bodycapture::append_line(
                        path,
                        &serde_json::json!({
                            "at_ms": std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|since| since.as_millis() as u64)
                                .unwrap_or(0),
                            "dir": "upstream->client",
                            "state": format!("{state:?}"),
                            "signal": format!("{signal:?}"),
                            "intercept": intercept,
                            "content_bytes": content_bytes,
                            "content_grace": self.content_grace,
                            "buffered_bytes": buffered_bytes,
                            "bytes": frame_len,
                        })
                        .to_string(),
                    );
                }
                buffered_bytes += frame_len;
                buffered.push(Held { frame, call });

                let leave = match signal {
                    Signal::RetrieveCall if state == State::Buffering => {
                        // execution may be the client's, in which case its own
                        // call is nothing for us to intercept
                        if intercept {
                            state = State::Intercepting;
                        }
                        !intercept
                    }
                    // a second call to our tool in the same turn is still our
                    // turn: the model may ask for several fragments at once
                    Signal::RetrieveCall => false,
                    // A call the client declared really is its own turn, and
                    // it must have it.
                    Signal::OtherCall => true,
                    // Content is different, and treating the two alike was a
                    // defect. A model that says one sentence before calling
                    // our tool has not made the turn the client's — but the
                    // old rule handed the buffer over on that sentence, went
                    // passthrough, and relayed the retrieve calls that
                    // followed straight to a client that could not answer
                    // them. So content is now *bounded* rather than fatal.
                    Signal::Content(text_len) => match state {
                        // Only a turn that could retrieve pays for the wait.
                        // Without an injected tool there is nothing to wait
                        // for, and holding a word of it would be latency
                        // bought for nothing.
                        State::Buffering if intercept => {
                            // The text, not the frame that carried it.
                            content_bytes += text_len;
                            content_bytes > self.content_grace
                                || buffered_bytes > self.buffer_limit
                                || self.held_too_long(held_since)
                        }
                        State::Buffering => true,
                        // The turn is already known to be ours. A model
                        // narrating between its own retrieve calls does not
                        // hand it back; the interception bounds still apply.
                        State::Intercepting => {
                            self.intercepted_too_long(buffered_bytes, held_since, buffered.len())
                        }
                        State::Passthrough => false,
                    },
                    // Both bounds now apply in either held state. They used to
                    // be gated on `Buffering`, which meant interception — the
                    // state that holds *everything* — was bounded by nothing.
                    Signal::None => match state {
                        State::Buffering => {
                            buffered_bytes > self.buffer_limit || self.held_too_long(held_since)
                        }
                        State::Intercepting => {
                            self.intercepted_too_long(buffered_bytes, held_since, buffered.len())
                        }
                        State::Passthrough => false,
                    },
                };
                if leave {
                    if let Some(path) = self.capture.as_ref() {
                        // The give-up itself, stated rather than implied. Once
                        // the relay is in passthrough it stops observing, so
                        // the frame trace simply ends -- and an absence is a
                        // poor diagnostic for the one event the trace exists to
                        // explain.
                        crate::bodycapture::append_line(
                            path,
                            &serde_json::json!({
                                "at_ms": std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|since| since.as_millis() as u64)
                                    .unwrap_or(0),
                                "dir": "upstream->client",
                                "state": "Passthrough",
                                // Which signal ended it, and for a content
                                // give-up which bound. Without this the record
                                // printed the content totals beside every
                                // give-up and invited the reading that content
                                // caused it: on the first live capture 28 of 31
                                // turns ended on `OtherCall` -- the model using
                                // a tool the client declared, which is correct
                                // and uninteresting -- with content at 0 to 299
                                // against a grace of 8,192, and the trace made
                                // that look alarming for a minute.
                                "reason": match signal {
                                    Signal::OtherCall => "the model called a tool the \
                                        client declared, so the turn is its own",
                                    Signal::Content(_) if content_bytes > self.content_grace =>
                                        "narration passed the content grace",
                                    Signal::Content(_) if buffered_bytes > self.buffer_limit =>
                                        "the deciding window passed the buffer limit",
                                    Signal::Content(_) => "held too long",
                                    _ => "the turn ended without a retrieve call",
                                },
                                "signal": format!("{signal:?}"),
                                "content_bytes": content_bytes,
                                "content_grace": self.content_grace,
                                "buffered_bytes": buffered_bytes,
                                "buffer_limit": self.buffer_limit,
                            })
                            .to_string(),
                        );
                    }
                    state = State::Passthrough;
                    let held = std::mem::take(&mut buffered);
                    match self
                        .hand_over(tx, &held, &turn, rounds, hidden, &mut our_items)
                        .await
                    {
                        HandOver::ClientGone => return RoundEnd::ClientGone,
                        // Armed by removal: `hand_over` took one of ours out
                        // of the buffer.
                        HandOver::Sent { elided: true } => {
                            elided = Some(turn.has_foreign_call());
                        }
                        HandOver::Sent { elided: false } => {}
                    }
                }
            }
        }

        if let Some(frame) = decoder.flush() {
            match state {
                State::Passthrough => {
                    if !self.emit(tx, &frame, rounds, hidden).await {
                        return RoundEnd::ClientGone;
                    }
                }
                _ => {
                    let (_, call) = turn.observe(&frame);
                    buffered.push(Held { frame, call });
                }
            }
        }

        if state == State::Intercepting && turn.has_calls() {
            return RoundEnd::Intercepted { turn, usage };
        }
        // a stream that ended while we were still deciding carried nothing the
        // machine had an opinion about; the client gets all of it
        if matches!(
            self.hand_over(tx, &buffered, &turn, rounds, hidden, &mut our_items)
                .await,
            HandOver::ClientGone
        ) {
            return RoundEnd::ClientGone;
        }
        RoundEnd::Finished { usage }
    }

    /// Execute the turn's retrievals and open the next upstream stream.
    ///
    /// `None` means the round could not be run and the caller must hand the
    /// buffered turn back to the client.
    async fn next_round(
        &mut self,
        turn: &StreamTurn,
        tx: &Sender<ClientChunk>,
    ) -> Option<BoxStream<'static, reqwest::Result<Bytes>>> {
        let retriever = self.retriever.as_ref()?;
        let calls = turn.calls();
        let spent = self.budget.is_spent();
        let mut results = Vec::with_capacity(calls.len());
        let mut tokens = 0u32;
        // Reserved per answer, as on the buffered path: the round is charged
        // after it is built, so without this one call could carry the request
        // past the configured ceiling before anything checked.
        let mut left = self.budget.remaining();
        for call in &calls {
            let (payload, cost) = if spent {
                retriever.refuse(call)
            } else {
                let (payload, cost) = retriever.answer(call);
                if cost > left {
                    tracing::warn!(
                        stage = "retrieve_round",
                        tokens = cost,
                        remaining = left,
                        "refusing a retrieval that would pass the per-request token budget"
                    );
                    retriever.refuse(call)
                } else {
                    (payload, cost)
                }
            };
            left = left.saturating_sub(cost);
            tokens = tokens.saturating_add(cost);
            results.push((call.clone(), payload));
        }

        let synthetic = turn.as_response();
        let mut next = self.outbound.json().cloned()?;
        if !turn::append_round(self.family, &mut next, &synthetic, &results)
            || !self.outbound.set_json(next)
        {
            self.failopen
                .record(Stage::RetrieveRound, &"could not build the next round");
            return None;
        }
        self.budget.spend(tokens);

        let response = match send_with_keepalive(&self.outbound, tx, self.keepalive).await {
            Some(Ok(response)) => response,
            Some(Err(error)) => {
                self.failopen.record(Stage::RetrieveRound, &error);
                return None;
            }
            None => return None,
        };
        if !response.status().is_success() {
            self.failopen
                .record(Stage::RetrieveRound, &response.status());
            return None;
        }
        Some(Box::pin(response.bytes_stream()))
    }

    /// Whether interception has run past what a client will wait for.
    ///
    /// Same trade as the buffering bounds, made for the same reason. Holding is
    /// what interception *is*, so there is no version of this that costs
    /// nothing: past the bound the round cannot be intercepted any more and the
    /// retrieval does not happen. What it buys is that the failure is ours and
    /// takes ninety seconds, instead of the client's and taking five minutes,
    /// a full retry, and a second five minutes.
    fn intercepted_too_long(&self, bytes: usize, since: std::time::Instant, frames: usize) -> bool {
        let (over_bytes, over_time) = intercepted_too_long(
            self.intercept_limit,
            bytes,
            self.intercept_timeout,
            since.elapsed(),
        );
        if over_bytes || over_time {
            tracing::warn!(
                held_ms = since.elapsed().as_millis() as u64,
                frames,
                bytes,
                over_bytes,
                over_time,
                "giving up on intercepting this turn; the retrieve call will be dropped"
            );
        }
        over_bytes || over_time
    }

    /// Give the client frames the relay was holding, without our own call.
    ///
    /// The old behaviour was to replay the buffer byte for byte, on the
    /// reasoning that half a turn cannot be satisfied. That is right about the
    /// *client's* calls and wrong about ours: a `optscale_kompressor_retrieve` call the
    /// client never declared comes back "unsupported call", which is a wasted
    /// round trip and a model told its own tool does not exist. It happened on
    /// a measured run, on a turn that carried one call we could answer and one
    /// we then handed over anyway.
    ///
    /// So our call's frames are dropped, and the two frames that would still
    /// mention it — the Responses terminal frame, which repeats the whole
    /// output list, and whatever says the turn ended in a tool call — are
    /// rewritten. Returns false if the client has gone.
    async fn hand_over(
        &self,
        tx: &Sender<ClientChunk>,
        held: &[Held],
        turn: &StreamTurn,
        rounds: u32,
        hidden: &Value,
        ours_seen: &mut Vec<String>,
    ) -> HandOver {
        // Chat first, and before the `ours` question is even asked. `Held.call`
        // attributes a whole frame to ONE call, which a coalesced Chat delta
        // carrying both sides cannot be: with foreign-call precedence such a
        // frame is attributed to the client's call, so the `!ours` path below
        // would forward it verbatim — ours included. Filtering is per entry, so
        // it is the only thing that can split a frame like that.
        if self.family == ApiFamily::OpenAiChat {
            let mut removed_ours = false;
            for h in held {
                match filter_our_chat_calls(&h.frame, ours_seen) {
                    ChatFilter::Dropped => {
                        removed_ours = true;
                    }
                    ChatFilter::Rewritten(bytes) => {
                        removed_ours = true;
                        if !self.emit_rewritten(tx, bytes, rounds, hidden).await {
                            return HandOver::ClientGone;
                        }
                    }
                    ChatFilter::Untouched => {
                        if !self.emit(tx, &h.frame, rounds, hidden).await {
                            return HandOver::ClientGone;
                        }
                    }
                }
            }
            return HandOver::Sent {
                elided: removed_ours,
            };
        }
        let ours = held
            .iter()
            .any(|h| h.call.is_some_and(|slot| turn.is_ours(slot)));
        if !ours {
            for h in held {
                if !self.emit(tx, &h.frame, rounds, hidden).await {
                    return HandOver::ClientGone;
                }
            }
            return HandOver::Sent { elided: false };
        }
        // With another call still in the turn the client has something to
        // answer and the turn's ending is already correct; with none, a turn
        // that still says "I called a tool" leaves the client waiting for a
        // result it will never be asked for.
        let others_remain = turn.has_foreign_call();
        for h in held {
            if h.call.is_some_and(|slot| turn.is_ours(slot)) {
                // Remember the item this frame belonged to before dropping it.
                // The buffer is the only place these ids appear: an argument
                // delta names its item by id and nothing else, so a delta that
                // arrives after hand-over is unrecognisable unless the opening
                // frame was recorded here. Without this the client received the
                // arguments of a call whose opening frame it never saw.
                note_our_item(self.family, &h.frame, ours_seen);
                continue;
            }
            match scrub(self.family, &h.frame, others_remain) {
                Some(rewritten) => {
                    if !self.emit_rewritten(tx, rewritten, rounds, hidden).await {
                        return HandOver::ClientGone;
                    }
                }
                None => {
                    if !self.emit(tx, &h.frame, rounds, hidden).await {
                        return HandOver::ClientGone;
                    }
                }
            }
        }
        HandOver::Sent { elided: true }
    }

    /// Write one frame to the client, rewriting only what §12.9 allows.
    /// Send a frame this relay rewrote, with the usage treatment still applied.
    ///
    /// Rewritten bytes used to go straight to `send`, which skips everything
    /// `emit` does: folding a hidden retrieval round's usage into the frame the
    /// client sees, and dropping a usage-only chunk the client never asked for.
    /// A terminal frame is exactly the frame that needs scrubbing *and* carries
    /// usage, so the two met: after a hidden round of 20 output tokens, a
    /// `response.completed` reporting 30 reached the client as 30 rather than
    /// the combined 50, and the proxy's own accounting disagreed with what the
    /// client was told.
    async fn emit_rewritten(
        &self,
        tx: &Sender<ClientChunk>,
        bytes: Bytes,
        rounds: u32,
        hidden: &Value,
    ) -> bool {
        let reparsed = parse_frame(bytes);
        self.emit(tx, &reparsed, rounds, hidden).await
    }

    async fn emit(
        &self,
        tx: &Sender<ClientChunk>,
        frame: &SseFrame,
        rounds: u32,
        hidden: &Value,
    ) -> bool {
        // the common case: nothing to inspect, so nothing is parsed
        if !self.strip_usage && rounds == 0 {
            return send(tx, frame.raw().clone()).await;
        }
        if self.strip_usage && is_usage_only_chunk(self.family, frame) {
            // we asked for this chunk to measure the saving; the client did
            // not, and a client that did not ask for usage is one whose parser
            // may not expect a choice-less chunk
            return true;
        }
        if rounds > 0
            && let Some(rewritten) = rewrite_usage(self.family, frame, hidden)
        {
            return send(tx, rewritten).await;
        }
        send(tx, frame.raw().clone()).await
    }
}

/// Take our own call out of a frame that names the whole turn.
///
/// Dropping a call's own frames is not enough on its own: two kinds of frame
/// describe the turn rather than one part of it, and both would still mention a
/// call the client is no longer being shown.
///
/// `None` means the frame needed no change and should be relayed as it is.
/// What filtering a Chat frame produced.
enum ChatFilter {
    /// Nothing of ours in it.
    Untouched,
    /// Ours removed, something left worth sending.
    Rewritten(Bytes),
    /// Ours removed and nothing else was in the frame.
    Dropped,
}

/// Take our `tool_calls` entries out of a Chat delta, leaving the client's.
///
/// Chat coalesces: one delta may carry several calls, and some of them may be
/// the client's. Dropping the whole frame — the way Responses does, where each
/// item has its own frames — would take the client's call or its text with it.
/// So entries are removed individually and the frame is re-encoded.
///
/// Our entries are matched by name on the frame that names them, and by
/// `index` afterwards: an argument continuation carries only the index, so the
/// index has to be remembered the way Responses remembers an item id and
/// Anthropic a block index.
fn filter_our_chat_calls(frame: &SseFrame, ours: &mut Vec<String>) -> ChatFilter {
    let Some(mut value) = frame.json() else {
        return ChatFilter::Untouched;
    };
    let Some(choices) = value.get_mut("choices").and_then(Value::as_array_mut) else {
        return ChatFilter::Untouched;
    };
    let mut removed = false;
    let mut anything_left = false;
    for choice in choices.iter_mut() {
        let Some(delta) = choice.get_mut("delta") else {
            continue;
        };
        if let Some(calls) = delta.get_mut("tool_calls").and_then(Value::as_array_mut) {
            calls.retain(|call| {
                let key = call
                    .get("index")
                    .map(|index| index.to_string())
                    .or_else(|| call.get("id").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_else(|| "0".to_string());
                let named_ours = call
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    == Some(RETRIEVE_TOOL_NAME);
                if named_ours && !ours.contains(&key) {
                    ours.push(key.clone());
                }
                if named_ours || ours.contains(&key) {
                    removed = true;
                    return false;
                }
                true
            });
            if !calls.is_empty() {
                anything_left = true;
            }
        }
        // Text, a refusal or a role the client is waiting for keeps the frame
        // alive even with every call taken out of it.
        if let Some(delta) = choice.get("delta").and_then(Value::as_object)
            && delta
                .iter()
                .any(|(key, v)| key != "tool_calls" && !v.is_null())
        {
            anything_left = true;
        }
        if choice
            .get("finish_reason")
            .is_some_and(|reason| !reason.is_null())
        {
            anything_left = true;
        }
    }
    if !removed {
        return ChatFilter::Untouched;
    }
    if anything_left {
        ChatFilter::Rewritten(reframe(frame, &value))
    } else {
        ChatFilter::Dropped
    }
}

/// Record the item id or block index a frame of ours belongs to, so its later parts can be
/// recognised.
///
/// Responses names the tool only on `output_item.added`/`done`; the argument
/// deltas in between carry an `item_id` and nothing else. A frame dropped
/// before hand-over therefore has to leave its id behind, or a delta arriving
/// after hand-over is unrecognisable and reaches a client that never saw the
/// call it belongs to.
fn note_our_item(family: ApiFamily, frame: &SseFrame, ours: &mut Vec<String>) {
    if family == ApiFamily::AnthropicMessages {
        // The opening block may be buffered while its deltas arrive after
        // hand-over. Preserve its index before dropping that opening frame.
        belongs_to_our_call(family, frame, ours);
        return;
    }
    if family != ApiFamily::OpenAiResponses {
        return;
    }
    let Some(value) = frame.json() else {
        return;
    };
    if let Some(id) = value
        .get("item")
        .and_then(|item| item.get("id"))
        .and_then(Value::as_str)
        && !ours.iter().any(|known| known == id)
    {
        ours.push(id.to_string());
    }
}

/// Is this passthrough frame part of *our* retrieve call?
///
/// Once the relay has handed a turn over it stops buffering, so our own call's
/// frames flow straight to a client that never declared the tool. `scrub` does
/// not help: it only rewrites the terminal frame, which is one of several that
/// name the call. The frames to drop are the item's own `added` and `done`, and
/// the argument deltas, which reference the item by id rather than by name —
/// hence the running set.
///
/// Responses and Anthropic. Chat reports calls alongside text in the same
/// delta frames, so `filter_our_chat_calls` filters their entries instead.
fn belongs_to_our_call(family: ApiFamily, frame: &SseFrame, ours: &mut Vec<String>) -> bool {
    let Some(value) = frame.json() else {
        return false;
    };
    // Anthropic blocks a whole turn when this is missed: a client told the turn
    // stopped for `tool_use` waits for a `tool_result` it was never given a call
    // to produce. Our block is named on `content_block_start`; its
    // `input_json_delta` and `content_block_stop` carry only the block `index`,
    // so the index is remembered the way Responses remembers an item id.
    if family == ApiFamily::AnthropicMessages {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let index = value.get("index").and_then(Value::as_u64);
        return match kind {
            "content_block_start" => {
                let named_ours = value
                    .get("content_block")
                    .and_then(|block| block.get("name"))
                    .and_then(Value::as_str)
                    == Some(RETRIEVE_TOOL_NAME);
                if named_ours && let Some(index) = index {
                    let key = index.to_string();
                    if !ours.contains(&key) {
                        ours.push(key);
                    }
                }
                named_ours
            }
            "content_block_delta" | "content_block_stop" => {
                index.is_some_and(|index| ours.iter().any(|known| *known == index.to_string()))
            }
            _ => false,
        };
    }
    if family != ApiFamily::OpenAiResponses {
        return false;
    }
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let item = value.get("item");
    let named_ours = item
        .and_then(|item| item.get("name"))
        .and_then(Value::as_str)
        == Some(RETRIEVE_TOOL_NAME);

    match kind {
        "response.output_item.added" | "response.output_item.done" => {
            if named_ours {
                if let Some(id) = item.and_then(|item| item.get("id")).and_then(Value::as_str)
                    && !ours.iter().any(|known| known == id)
                {
                    ours.push(id.to_string());
                }
                return true;
            }
            false
        }
        "response.function_call_arguments.delta" | "response.function_call_arguments.done" => value
            .get("item_id")
            .and_then(Value::as_str)
            .is_some_and(|id| ours.iter().any(|known| known == id)),
        _ => false,
    }
}

fn scrub(family: ApiFamily, frame: &SseFrame, others_remain: bool) -> Option<Bytes> {
    let value = frame.json()?;
    match family {
        // The terminal frame repeats the entire output list, so the call has to
        // come out of it as well as off the wire. `status` is left alone: the
        // response really did complete.
        ApiFamily::OpenAiResponses => {
            let kind = value.get("type").and_then(Value::as_str)?;
            if !matches!(kind, "response.completed" | "response.incomplete") {
                return None;
            }
            let mut value = value;
            let output = value
                .get_mut("response")?
                .get_mut("output")?
                .as_array_mut()?;
            let before = output.len();
            output.retain(|item| {
                item.get("name").and_then(Value::as_str) != Some(RETRIEVE_TOOL_NAME)
            });
            (output.len() != before).then(|| reframe(frame, &value))
        }
        // Chat says how the turn ended in the same frame as the last delta.
        ApiFamily::OpenAiChat => {
            if others_remain {
                return None;
            }
            let mut value = value;
            let choices = value.get_mut("choices")?.as_array_mut()?;
            let mut changed = false;
            for choice in choices {
                if choice.get("finish_reason").and_then(Value::as_str) == Some("tool_calls") {
                    choice["finish_reason"] = Value::String("stop".to_string());
                    changed = true;
                }
            }
            changed.then(|| reframe(frame, &value))
        }
        // Anthropic is the one that hangs if this is missed: a client told the
        // turn stopped for `tool_use` waits for a `tool_result` it was never
        // given a call to produce.
        ApiFamily::AnthropicMessages => {
            if others_remain || value.get("type").and_then(Value::as_str) != Some("message_delta") {
                return None;
            }
            let mut value = value;
            let delta = value.get_mut("delta")?;
            if delta.get("stop_reason").and_then(Value::as_str) != Some("tool_use") {
                return None;
            }
            delta["stop_reason"] = Value::String("end_turn".to_string());
            Some(reframe(frame, &value))
        }
    }
}

/// Rebuild a frame around new data, keeping the `event:` line the provider sent.
///
/// Anthropic dispatches on `event:` and would not recognise a frame that lost
/// it; the OpenAI families do not send one, and adding one is not this proxy's
/// business either.
fn reframe(frame: &SseFrame, value: &Value) -> Bytes {
    let data = serde_json::to_string(value).unwrap_or_else(|_| frame.data().to_string());
    match frame.event() {
        Some(event) => Bytes::from(format!("event: {event}\ndata: {data}\n\n")),
        None => Bytes::from(format!("data: {data}\n\n")),
    }
}

async fn send(tx: &Sender<ClientChunk>, bytes: Bytes) -> bool {
    tx.send(Ok(bytes)).await.is_ok()
}

/// Whether a hold of `elapsed` has run past `bound`.
///
/// Zero is off, and is the shipped default — see `proxy.stream_buffer_timeout`
/// for the run this cost.
fn held_too_long(bound: Duration, elapsed: Duration) -> bool {
    !bound.is_zero() && elapsed >= bound
}

/// Whether an interception has run past either of its bounds, and which.
///
/// A free function, like [`held_too_long`], so the rule can be tested without
/// standing up a relay: the bounds are the whole of the decision and the relay
/// contributes nothing to it but the numbers. Zero disables a bound.
fn intercepted_too_long(
    byte_bound: usize,
    bytes: usize,
    time_bound: Duration,
    elapsed: Duration,
) -> (bool, bool) {
    (
        byte_bound > 0 && bytes > byte_bound,
        held_too_long(time_bound, elapsed),
    )
}

/// What reading the upstream produced.
#[derive(Debug)]
enum Pull {
    Chunk(Bytes),
    /// The upstream stream ended, cleanly or not.
    Done,
    UpstreamError,
    /// The client hung up. Nothing further is worth reading.
    ClientGone,
    /// The hold outlasted its bound with nothing arriving.
    ///
    /// The deadline used to be checked only inside frame processing, which is
    /// reached after a chunk arrives *and* a complete frame decodes. While the
    /// client is held back this loop is parked waiting for upstream bytes, and
    /// its only other branches send a heartbeat and loop again — so an upstream
    /// that simply stopped sending was never measured against the bound. The
    /// hold then lasted until the separate upstream read timeout, 600 s by
    /// default, against a stated interception bound of 90 s.
    HeldTooLong,
}

/// The next chunk, keeping the client's connection warm while we wait.
///
/// Two things happen here besides reading.
///
/// **The client is pinged while it can see nothing.** `silent` is set whenever the
/// relay is holding frames back — while a retrieve round runs, and while the
/// machine is still deciding whose turn this is. That second case is the one
/// that matters in practice: buffering is the state every streamed round starts
/// in, reasoning deltas deliberately do not end it, and a provider that goes
/// quiet there leaves the client watching a silent socket it never asked for.
/// A real agent gave up on exactly that after five minutes — "stream
/// disconnected before completion: idle timeout waiting for SSE" — while this
/// proxy sat waiting out its own ten-minute upstream read timeout.
///
/// **A departed client is noticed without having to write to it — but only
/// while it is being held back.** Discovering the disconnect solely when a send
/// fails means a relay that is buffering never discovers it at all: it holds the
/// connection open against a stalled provider, bills for tokens nobody will
/// read, and records no row because its recorder never runs. Watching the
/// channel close covers that. Watching it in *passthrough* would be wrong for
/// the opposite reason — a finished client closes its connection, and treating
/// that as a give-up mislabels every ordinary completion.
async fn next_chunk(
    stream: &mut BoxStream<'static, reqwest::Result<Bytes>>,
    tx: &Sender<ClientChunk>,
    keepalive: Duration,
    silent: bool,
    intercepting: bool,
    // How much longer this hold may last, when one is in force. `None` in
    // passthrough, where the client already has what arrived and nothing is
    // being held from it.
    deadline: Option<tokio::time::Instant>,
) -> Pull {
    loop {
        if let Some(deadline) = deadline
            && tokio::time::Instant::now() >= deadline
        {
            return Pull::HeldTooLong;
        }
        tokio::select! {
            // Zero disables the bound, which is what a zero timeout means
            // everywhere else in this module.
            _ = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => return Pull::HeldTooLong,
            item = stream.next() => return match item {
                Some(Ok(chunk)) => Pull::Chunk(chunk),
                Some(Err(error)) => {
                    tracing::warn!(%error, "upstream stream ended early");
                    Pull::UpstreamError
                }
                None => Pull::Done,
            },
            // Only while the client is being held back. In passthrough it has
            // the answer as it arrives, and an SSE client closes its connection
            // the moment it reads the terminal frame — so watching for the close
            // there labels every ordinary completion an abandonment. Measured:
            // 36 of 37 "abandoned" requests on a benchmark run were that, each
            // ending after a few seconds rather than the five minutes a real
            // give-up takes, and each losing the usage frame that arrives last.
            //
            // Passthrough needs no watcher anyway: a departed client is found
            // the next time there is a frame to hand it, which is precisely when
            // the departure costs something.
            // The caller logs this, with what was being held at the time.
            _ = tx.closed(), if silent => return Pull::ClientGone,
            _ = tokio::time::sleep(keepalive), if silent => {
                let beat = if intercepting { KEEPALIVE } else { KEEPALIVE_BUFFERING };
                if !send(tx, Bytes::from_static(beat)).await {
                    return Pull::ClientGone;
                }
            }
        }
    }
}

/// Open the next round's stream, pinging the client while the provider thinks.
async fn send_with_keepalive(
    outbound: &Outbound,
    tx: &Sender<ClientChunk>,
    keepalive: Duration,
) -> Option<reqwest::Result<reqwest::Response>> {
    let mut sending = Box::pin(outbound.send());
    loop {
        tokio::select! {
            result = &mut sending => return Some(result),
            _ = tokio::time::sleep(keepalive) => {
                if !send(tx, Bytes::from_static(KEEPALIVE)).await {
                    return None;
                }
            }
        }
    }
}

/// What a frame tells the state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Signal {
    /// Nothing the machine reacts to.
    None,
    /// Output the client is waiting for.
    ///
    /// Reasoning and thinking deltas are deliberately *not* content: on
    /// Anthropic a tool-use turn with extended thinking always opens with a
    /// thinking block, so treating it as content would mean the retrieve loop
    /// never ran for exactly the models most likely to use it. The cost is time
    /// to first byte on a thinking turn, bounded by the buffer limit, which
    /// falls back to passthrough.
    ///
    /// Carries the length of the *text*, not of the frame that delivered it.
    /// The grace used to count `frame.raw().len()`, which is mostly JSON
    /// envelope: a live turn narrating 1,857 bytes before calling the tool sent
    /// those as a hundred-odd deltas and spent the whole 8 KiB on overhead. The
    /// relay went passthrough, the retrieve call reached a client that answered
    /// `"unsupported call: optscale_kompressor_retrieve"`, and the run reported zero
    /// retrievals while the model had plainly asked. Counting what the bound is
    /// named for makes 8 KiB mean 8 KiB of prose again.
    Content(usize),
    /// A call to our tool started.
    RetrieveCall,
    /// A call to something else started.
    OtherCall,
}

/// One partially streamed tool call.
#[derive(Clone, Debug, Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

/// Accumulates a streamed assistant turn well enough to replay it as a
/// request.
struct StreamTurn {
    family: ApiFamily,
    calls: Vec<PartialCall>,
    /// Wire key (index, item id) to position in `calls`.
    index: BTreeMap<String, usize>,
    /// Completed Responses output items, kept verbatim so a reasoning item is
    /// carried into the next round rather than made the model redo it.
    items: Vec<Value>,
    /// The call slot the frame currently being observed belongs to.
    ///
    /// Set by `entry`, read once by `observe`. A frame has to be attributable
    /// to a call for the relay to be able to take one call's frames out of a
    /// replay, and the wire keys that do the attributing are already computed
    /// here — recomputing them at replay time would be a second parser that
    /// could disagree with this one.
    touched: Option<usize>,
}

impl StreamTurn {
    fn new(family: ApiFamily) -> StreamTurn {
        StreamTurn {
            family,
            calls: Vec::new(),
            index: BTreeMap::new(),
            items: Vec::new(),
            touched: None,
        }
    }

    fn has_calls(&self) -> bool {
        !self.calls.is_empty()
    }

    fn slot(&mut self, key: String) -> usize {
        match self.index.get(&key) {
            Some(position) => *position,
            None => {
                self.calls.push(PartialCall::default());
                let position = self.calls.len() - 1;
                self.index.insert(key, position);
                position
            }
        }
    }

    fn entry(&mut self, key: String) -> &mut PartialCall {
        let position = self.slot(key);
        self.touched = Some(position);
        &mut self.calls[position]
    }

    /// Whether the frames of `slot` are a call to our own tool.
    fn is_ours(&self, slot: usize) -> bool {
        self.calls
            .get(slot)
            .is_some_and(|call| call.name == RETRIEVE_TOOL_NAME)
    }

    /// Whether any call in the turn belongs to somebody else.
    ///
    /// What decides whether a turn still makes sense to a client once our own
    /// call has been taken out of it: with another call left the turn is still
    /// a tool-use turn and the client answers that one, and without it the turn
    /// has to stop looking like one at all.
    fn has_foreign_call(&self) -> bool {
        self.calls
            .iter()
            .any(|call| !call.name.is_empty() && call.name != RETRIEVE_TOOL_NAME)
    }

    /// Fold one frame into the turn and say what it means, and which call it
    /// was part of.
    fn observe(&mut self, frame: &SseFrame) -> (Signal, Option<usize>) {
        let Some(value) = frame.json() else {
            return (Signal::None, None);
        };
        self.observe_value(&value)
    }

    /// The same classification, over a parsed event rather than an SSE frame.
    ///
    /// Split out because the events are the transport's, not SSE's: the same
    /// `response.output_item.added` / `function_call_arguments.done` vocabulary
    /// arrives over a relayed websocket, where there are no `data:` lines to
    /// parse. A second classifier for that transport would be a second thing to
    /// disagree with this one about what a turn is.
    pub(crate) fn observe_value(&mut self, value: &Value) -> (Signal, Option<usize>) {
        self.touched = None;
        let signal = match self.family {
            ApiFamily::OpenAiChat => self.observe_chat(value),
            ApiFamily::OpenAiResponses => self.observe_responses(value),
            ApiFamily::AnthropicMessages => self.observe_anthropic(value),
        };
        (signal, self.touched.take())
    }

    fn observe_chat(&mut self, value: &Value) -> Signal {
        let Some(delta) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("delta"))
        else {
            return Signal::None;
        };
        for key in ["content", "refusal"] {
            if let Some(text) = delta
                .get(key)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                return Signal::Content(text.len());
            }
        }
        let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) else {
            return Signal::None;
        };
        // **Foreign-call precedence, not last-one-wins.** A Chat delta may carry
        // several `tool_calls`, and this used to overwrite the signal per entry
        // and report whichever came last. Both orders were then wrong in
        // opposite directions: `[ours, theirs]` classified as `OtherCall`, so
        // hand-over forwarded the whole frame — our undeclared call with it —
        // and `[theirs, ours]` classified as `RetrieveCall`, so the relay
        // intercepted and ran the client's call through `Retriever` instead of
        // letting the client execute it.
        //
        // A frame carrying any call the client owns is the client's turn to
        // finish, whatever else is in it. Ours is removed from the bytes
        // instead, by `filter_our_chat_calls`.
        let mut signal = Signal::None;
        for call in calls {
            let key = call
                .get("index")
                .map(|index| index.to_string())
                .or_else(|| call.get("id").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| "0".to_string());
            let entry = self.entry(key);
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                entry.id = id.to_string();
            }
            let function = call.get("function");
            if let Some(name) = function.and_then(|f| f.get("name")).and_then(Value::as_str) {
                entry.name = name.to_string();
                signal = match (signal, named(name)) {
                    (Signal::OtherCall, _) | (_, Signal::OtherCall) => Signal::OtherCall,
                    (Signal::RetrieveCall, _) | (_, Signal::RetrieveCall) => Signal::RetrieveCall,
                    _ => signal,
                };
            }
            if let Some(arguments) = function
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
            {
                entry.arguments.push_str(arguments);
            }
        }
        signal
    }

    fn observe_responses(&mut self, value: &Value) -> Signal {
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            return Signal::None;
        };
        match kind {
            "response.output_text.delta" | "response.refusal.delta" => Signal::Content(
                value
                    .get("delta")
                    .and_then(Value::as_str)
                    .map_or(0, str::len),
            ),
            "response.output_item.added" | "response.output_item.done" => {
                let Some(item) = value.get("item") else {
                    return Signal::None;
                };
                if kind == "response.output_item.done" {
                    self.items.push(item.clone());
                }
                if item.get("type").and_then(Value::as_str) != Some("function_call") {
                    return Signal::None;
                }
                let key = item_key(value, item);
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let entry = self.entry(key);
                if let Some(id) = item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str)
                {
                    entry.id = id.to_string();
                }
                if !name.is_empty() {
                    entry.name = name.clone();
                }
                if let Some(arguments) = item.get("arguments").and_then(Value::as_str)
                    && !arguments.is_empty()
                {
                    entry.arguments = arguments.to_string();
                }
                named(&name)
            }
            "response.function_call_arguments.delta" => {
                let key = item_key(value, &Value::Null);
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    self.entry(key).arguments.push_str(delta);
                }
                Signal::None
            }
            "response.function_call_arguments.done" => {
                let key = item_key(value, &Value::Null);
                if let Some(arguments) = value.get("arguments").and_then(Value::as_str) {
                    self.entry(key).arguments = arguments.to_string();
                }
                Signal::None
            }
            _ => Signal::None,
        }
    }

    fn observe_anthropic(&mut self, value: &Value) -> Signal {
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            return Signal::None;
        };
        let key = value
            .get("index")
            .map(|index| index.to_string())
            .unwrap_or_else(|| "0".to_string());
        match kind {
            "content_block_start" => {
                let block = value.get("content_block").unwrap_or(&Value::Null);
                match block.get("type").and_then(Value::as_str) {
                    // The block only opens here; its text arrives in the
                    // deltas that follow, so this contributes nothing to the
                    // grace.
                    Some("text") => Signal::Content(0),
                    Some("tool_use") => {
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let entry = self.entry(key);
                        entry.id = id;
                        entry.name = name.clone();
                        named(&name)
                    }
                    _ => Signal::None,
                }
            }
            "content_block_delta" => {
                let delta = value.get("delta").unwrap_or(&Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => Signal::Content(
                        delta
                            .get("text")
                            .and_then(Value::as_str)
                            .map_or(0, str::len),
                    ),
                    Some("input_json_delta") => {
                        if let Some(partial) = delta.get("partial_json").and_then(Value::as_str) {
                            self.entry(key).arguments.push_str(partial);
                        }
                        Signal::None
                    }
                    _ => Signal::None,
                }
            }
            // Closes a block this turn already knows about. Attributed but
            // never *opened*: a stop for an index nothing started is not a
            // call, and inventing a slot for it would put an empty call in the
            // turn the next round then has to replay.
            "content_block_stop" => {
                self.touched = self.index.get(&key).copied();
                Signal::None
            }
            _ => Signal::None,
        }
    }

    /// The calls, in the shape the retriever and the next request want.
    fn calls(&self) -> Vec<ModelToolCall> {
        self.calls
            .iter()
            .map(|call| ModelToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments: match self.family {
                    ApiFamily::AnthropicMessages => {
                        serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({}))
                    }
                    _ => Value::String(if call.arguments.is_empty() {
                        "{}".to_string()
                    } else {
                        call.arguments.clone()
                    }),
                },
            })
            .collect()
    }

    /// The turn as a complete response, so the next request is built by the
    /// same code the buffered path uses.
    fn as_response(&self) -> Value {
        match self.family {
            ApiFamily::OpenAiChat => {
                let calls: Vec<Value> = self
                    .calls
                    .iter()
                    .map(|call| {
                        json!({
                            "id": call.id,
                            "type": "function",
                            "function": {"name": call.name, "arguments": call.arguments},
                        })
                    })
                    .collect();
                json!({"choices": [{
                    "message": {"role": "assistant", "content": null, "tool_calls": calls},
                    "finish_reason": "tool_calls",
                }]})
            }
            ApiFamily::OpenAiResponses => {
                if !self.items.is_empty() {
                    return json!({"output": self.items});
                }
                let items: Vec<Value> = self
                    .calls
                    .iter()
                    .map(|call| {
                        json!({
                            "type": "function_call",
                            "call_id": call.id,
                            "name": call.name,
                            "arguments": call.arguments,
                        })
                    })
                    .collect();
                json!({"output": items})
            }
            ApiFamily::AnthropicMessages => {
                let blocks: Vec<Value> = self
                    .calls
                    .iter()
                    .map(|call| {
                        json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.name,
                            "input": serde_json::from_str::<Value>(&call.arguments)
                                .unwrap_or_else(|_| json!({})),
                        })
                    })
                    .collect();
                json!({"content": blocks})
            }
        }
    }
}

fn named(name: &str) -> Signal {
    if name == RETRIEVE_TOOL_NAME {
        Signal::RetrieveCall
    } else if name.is_empty() {
        Signal::None
    } else {
        Signal::OtherCall
    }
}

/// The key a Responses frame addresses its item by.
fn item_key(value: &Value, item: &Value) -> String {
    for source in [value, item] {
        for key in ["item_id", "id"] {
            if let Some(id) = source.get(key).and_then(Value::as_str) {
                return id.to_string();
            }
        }
    }
    value
        .get("output_index")
        .map(|index| index.to_string())
        .unwrap_or_else(|| "0".to_string())
}

/// Where a family's frame keeps its usage numbers, if this frame has any.
fn usage_pointer(family: ApiFamily, value: &Value) -> Option<&'static str> {
    let kind = value.get("type").and_then(Value::as_str);
    match family {
        ApiFamily::OpenAiChat => value
            .get("usage")
            .filter(|usage| usage.is_object())
            .map(|_| "/usage"),
        ApiFamily::OpenAiResponses => value
            .pointer("/response/usage")
            .filter(|usage| usage.is_object())
            .map(|_| "/response/usage"),
        ApiFamily::AnthropicMessages => match kind {
            Some("message_start") => value
                .pointer("/message/usage")
                .filter(|usage| usage.is_object())
                .map(|_| "/message/usage"),
            Some("message_delta") => value
                .get("usage")
                .filter(|usage| usage.is_object())
                .map(|_| "/usage"),
            _ => None,
        },
    }
}

/// The usage a frame reports, for the request's own accounting.
fn frame_usage(family: ApiFamily, frame: &SseFrame) -> Option<Value> {
    let value = frame.json()?;
    let pointer = usage_pointer(family, &value)?;
    value.pointer(pointer).cloned()
}

/// An OpenAI chunk that carries usage and nothing else.
///
/// This is the extra chunk `stream_options.include_usage` adds. A client that
/// did not ask for it may not expect a chunk with no choices in it.
fn is_usage_only_chunk(family: ApiFamily, frame: &SseFrame) -> bool {
    if family != ApiFamily::OpenAiChat {
        return false;
    }
    let Some(value) = frame.json() else {
        return false;
    };
    let choiceless = value
        .get("choices")
        .and_then(Value::as_array)
        .is_some_and(|choices| choices.is_empty());
    choiceless && value.get("usage").is_some_and(Value::is_object)
}

/// Fold the hidden rounds' usage into a usage frame, re-encoding only that
/// frame.
fn rewrite_usage(family: ApiFamily, frame: &SseFrame, hidden: &Value) -> Option<Bytes> {
    let mut value = frame.json()?;
    let pointer = usage_pointer(family, &value)?;
    let usage = value.pointer_mut(pointer)?;
    turn::add_usage(usage, hidden);
    let data = serde_json::to_string(&value).ok()?;
    let mut out = String::with_capacity(data.len() + 32);
    if let Some(event) = frame.event() {
        // Anthropic dispatches on the event name, so a re-encoded frame that
        // lost it would be an event the client silently ignores
        out.push_str("event: ");
        out.push_str(event);
        out.push('\n');
    }
    out.push_str("data: ");
    out.push_str(&data);
    out.push_str("\n\n");
    Some(Bytes::from(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(data: &str) -> SseFrame {
        SseFrame::synthetic(Bytes::from(format!("data: {data}\n\n")))
    }

    fn observe(family: ApiFamily, frames: &[&str]) -> (StreamTurn, Vec<Signal>) {
        let mut turn = StreamTurn::new(family);
        let signals = frames.iter().map(|f| turn.observe(&frame(f)).0).collect();
        (turn, signals)
    }

    #[test]
    fn a_chat_tool_call_is_recognised_from_its_first_frame_and_assembled_from_the_rest() {
        let (turn, signals) = observe(
            ApiFamily::OpenAiChat,
            &[
                r#"{"choices":[{"delta":{"role":"assistant","content":""}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"optscale_kompressor_retrieve","arguments":""}}]}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"ref\":"}}]}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"k_AbC123dEf456\"}"}}]}}]}"#,
            ],
        );
        assert_eq!(signals[0], Signal::None, "an empty delta is not content");
        assert_eq!(signals[1], Signal::RetrieveCall);
        let calls = turn.calls();
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].arguments, json!("{\"ref\":\"k_AbC123dEf456\"}"));
    }

    #[test]
    fn a_call_to_another_tool_is_named_in_its_first_frame_too() {
        let (_, signals) = observe(
            ApiFamily::OpenAiChat,
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":""}}]}}]}"#,
            ],
        );
        assert_eq!(signals[0], Signal::OtherCall);
    }

    /// The rule that decides whether one sentence throws away the turn.
    ///
    /// Extracted as a pure predicate because the relay around it needs a
    /// socket, and this is the whole of the decision that broke the retrieve
    /// loop against `gpt-5.5`.
    fn content_gives_up(intercept: bool, content_bytes: usize, grace: usize) -> bool {
        if !intercept {
            return true;
        }
        content_bytes > grace
    }

    #[test]
    fn a_sentence_before_a_retrieve_call_no_longer_throws_the_turn_away() {
        // The measured failure: gpt-5.5 says "I'll look at the omitted
        // section", then calls optscale_kompressor_retrieve thirteen times. Under the
        // old rule that sentence meant "not our turn", the relay went
        // passthrough, and all thirteen reached a client that answered
        // `unsupported call`. Nothing was fetched back.
        assert!(
            !content_gives_up(true, 34, 8192),
            "one sentence must not end the turn"
        );
        // Still bounded: a turn that keeps generating is let go rather than
        // held forever.
        assert!(content_gives_up(true, 8193, 8192));
    }

    #[test]
    fn a_turn_that_could_not_retrieve_pays_nothing_for_the_wait() {
        // No tool injected means no retrieve call can arrive, so there is
        // nothing to wait for and holding a word of it would be latency
        // bought for nothing.
        assert!(content_gives_up(false, 1, 8192));
        assert!(content_gives_up(false, 0, 8192));
    }

    #[test]
    fn a_zero_grace_restores_the_old_behaviour() {
        // The escape hatch, for a deployment that would rather stream than
        // retrieve. `0` gives up at the first byte, which is what this did
        // before the bound existed.
        assert!(content_gives_up(true, 1, 0));
    }

    #[test]
    fn text_is_content_on_every_family_and_thinking_deliberately_is_not() {
        let (_, chat) = observe(
            ApiFamily::OpenAiChat,
            &[r#"{"choices":[{"delta":{"content":"He"}}]}"#],
        );
        assert_eq!(chat[0], Signal::Content(2), "the two bytes of \"He\"");

        let (_, responses) = observe(
            ApiFamily::OpenAiResponses,
            &[r#"{"type":"response.output_text.delta","delta":"He"}"#],
        );
        assert_eq!(responses[0], Signal::Content(2));

        let (_, anthropic) = observe(
            ApiFamily::AnthropicMessages,
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            ],
        );
        assert_eq!(
            anthropic[..2],
            [Signal::None, Signal::None],
            "a thinking block must not end the buffering window, or a thinking \
             model's retrieve call could never be intercepted"
        );
        // A text block opening carries no text yet, so it contributes nothing
        // to the grace -- the deltas that follow do.
        assert_eq!(anthropic[2], Signal::Content(0));
    }

    #[test]
    fn an_anthropic_tool_use_turn_is_reassembled_into_a_request_shaped_turn() {
        let (turn, signals) = observe(
            ApiFamily::AnthropicMessages,
            &[
                r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":10}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"optscale_kompressor_retrieve","input":{}}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"ref\":\"k_AbC123dEf456\"}"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
            ],
        );
        assert_eq!(signals[1], Signal::RetrieveCall);
        let response = turn.as_response();
        assert_eq!(response["content"][0]["type"], "tool_use");
        assert_eq!(response["content"][0]["input"]["ref"], "k_AbC123dEf456");
        assert_eq!(turn.calls()[0].arguments["ref"], "k_AbC123dEf456");
    }

    #[test]
    fn a_responses_turn_keeps_its_completed_items_verbatim() {
        let (turn, signals) = observe(
            ApiFamily::OpenAiResponses,
            &[
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#,
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[]}}"#,
                r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_9","name":"optscale_kompressor_retrieve","arguments":""}}"#,
                r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"ref\":\"k_AbC123dEf456\"}"}"#,
                r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"ref\":\"k_AbC123dEf456\"}"}"#,
            ],
        );
        assert_eq!(signals[2], Signal::RetrieveCall);
        assert_eq!(turn.calls()[0].id, "call_9");
        let response = turn.as_response();
        assert_eq!(
            response["output"][0]["type"], "reasoning",
            "reasoning the model already did is carried into the next round"
        );
    }

    #[test]
    fn the_older_wire_shape_still_reports_its_input_side() {
        // `message_delta` used to carry `output_tokens` alone — every other
        // fixture in this crate is still that shape. A fold that let the later
        // frame stand would erase the input counters it omits.
        let start = r#"{"type":"message_start","message":{"usage":{"input_tokens":12,"cache_read_input_tokens":900}}}"#;
        let delta = r#"{"type":"message_delta","usage":{"output_tokens":5}}"#;

        let mut usage = json!({});
        for raw in [start, delta] {
            let carried = frame_usage(ApiFamily::AnthropicMessages, &frame(raw))
                .expect("both frames carry usage");
            turn::max_usage(&mut usage, &carried);
        }

        assert_eq!(usage["input_tokens"], 12);
        assert_eq!(usage["cache_read_input_tokens"], 900);
        assert_eq!(usage["output_tokens"], 5);
    }

    #[test]
    fn separate_rounds_still_sum() {
        // The distinction the fix turns on. Two frames in one round are two
        // reports of one call; two rounds are two calls, and a retrieve round
        // the client never sees is still a prompt the operator paid for.
        let round = json!({"input_tokens": 8, "cache_read_input_tokens": 41116});
        let mut total = json!({});
        turn::add_usage(&mut total, &round);
        turn::add_usage(&mut total, &round);
        assert_eq!(total["cache_read_input_tokens"], 82232);
        assert_eq!(total["input_tokens"], 16);
    }

    #[test]
    fn usage_is_read_from_the_frame_each_family_puts_it_in() {
        assert_eq!(
            frame_usage(
                ApiFamily::OpenAiChat,
                &frame(r#"{"choices":[],"usage":{"prompt_tokens":7}}"#)
            )
            .unwrap()["prompt_tokens"],
            7
        );
        assert_eq!(
            frame_usage(
                ApiFamily::AnthropicMessages,
                &frame(r#"{"type":"message_delta","usage":{"output_tokens":3}}"#)
            )
            .unwrap()["output_tokens"],
            3
        );
        assert_eq!(
            frame_usage(
                ApiFamily::OpenAiResponses,
                &frame(r#"{"type":"response.completed","response":{"usage":{"input_tokens":5}}}"#)
            )
            .unwrap()["input_tokens"],
            5
        );
        assert!(
            frame_usage(
                ApiFamily::OpenAiChat,
                &frame(r#"{"choices":[{"delta":{}}]}"#)
            )
            .is_none()
        );
    }

    #[test]
    fn only_the_usage_frame_is_re_encoded_and_it_keeps_its_event_name() {
        let hidden = json!({"output_tokens": 40});
        let rewritten = rewrite_usage(
            ApiFamily::AnthropicMessages,
            &SseFrame::synthetic(Bytes::from(
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":2}}\n\n",
            )),
            &hidden,
        )
        .expect("a usage frame");
        let text = String::from_utf8(rewritten.to_vec()).unwrap();
        assert!(text.starts_with("event: message_delta\n"));
        assert!(text.contains("\"output_tokens\":42"));

        assert!(
            rewrite_usage(
                ApiFamily::AnthropicMessages,
                &frame(r#"{"type":"content_block_delta"}"#),
                &hidden
            )
            .is_none(),
            "a frame with no usage is relayed as the bytes it arrived as"
        );
    }

    #[test]
    fn the_extra_usage_chunk_is_recognised_only_where_it_exists() {
        assert!(is_usage_only_chunk(
            ApiFamily::OpenAiChat,
            &frame(r#"{"choices":[],"usage":{"prompt_tokens":7}}"#)
        ));
        assert!(!is_usage_only_chunk(
            ApiFamily::OpenAiChat,
            &frame(r#"{"choices":[{"delta":{"content":"hi"}}],"usage":null}"#)
        ));
        assert!(!is_usage_only_chunk(
            ApiFamily::AnthropicMessages,
            &frame(r#"{"type":"message_delta","usage":{"output_tokens":3}}"#)
        ));
    }
}

#[cfg(test)]
mod pull_tests {
    use super::*;

    fn idle_stream() -> BoxStream<'static, reqwest::Result<Bytes>> {
        Box::pin(futures_util::stream::pending())
    }

    /// Drive `next_chunk` concurrently with reading the client's side, because
    /// the heartbeat only exists while the pull is being polled.
    async fn beat_while(ping: bool, intercepting: bool) -> Option<Bytes> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientChunk>(4);
        let mut stream = idle_stream();
        tokio::select! {
            _ = next_chunk(&mut stream, &tx, Duration::from_millis(10), ping, intercepting, None) => None,
            got = rx.recv() => got.and_then(Result::ok),
            _ = tokio::time::sleep(Duration::from_millis(400)) => None,
        }
    }

    /// The gap that let a real agent give up: while the relay is *deciding*
    /// whose turn a stream is it holds every frame back, and it used to do that
    /// in silence — only a retrieve round pinged.
    #[tokio::test]
    async fn a_buffering_relay_keeps_the_client_warm() {
        let beat = beat_while(true, false)
            .await
            .expect("a heartbeat arrives rather than silence");
        assert_eq!(
            beat.as_ref(),
            KEEPALIVE_BUFFERING,
            "and it says what is actually happening: nothing is being retrieved yet"
        );
    }

    #[tokio::test]
    async fn a_retrieve_round_still_says_it_is_retrieving() {
        let beat = beat_while(true, true).await.expect("a heartbeat");
        assert_eq!(beat.as_ref(), KEEPALIVE);
    }

    /// Passthrough is the provider's own stream. A stall there is the
    /// provider's to explain, and inserting our heartbeat would hide it.
    #[tokio::test]
    async fn passthrough_is_not_padded_with_heartbeats() {
        assert!(
            beat_while(false, false).await.is_none(),
            "with ping off nothing is sent, however long the provider is quiet"
        );
    }

    /// The reason the incident left no `proxy_requests` row. A relay that only
    /// learns of a disconnect when a send fails never learns while buffering —
    /// it holds a stalled provider open, pays for tokens nobody will read, and
    /// its recorder never runs.
    #[tokio::test]
    async fn a_client_that_leaves_while_held_back_is_noticed_at_once() {
        let (tx, rx) = tokio::sync::mpsc::channel::<ClientChunk>(4);
        drop(rx); // the client hung up while it was waiting on us
        let mut stream = idle_stream();
        let pull = tokio::time::timeout(
            Duration::from_secs(5),
            // A keepalive an hour away, so the only thing that can notice the
            // departure is the closed channel itself.
            next_chunk(
                &mut stream,
                &tx,
                Duration::from_secs(3600),
                true,
                false,
                None,
            ),
        )
        .await
        .expect("it must not wait on an upstream nobody is listening to");
        assert!(
            matches!(pull, Pull::ClientGone),
            "a closed channel is a departed client"
        );
    }

    /// The regression the first version of that caused. An SSE client closes its
    /// connection the moment it reads the terminal frame, so in passthrough a
    /// closed channel is what a *finished* request looks like. Watching for it
    /// there labelled 36 of 37 ordinary completions on a benchmark run as
    /// abandonments — each ending in a few seconds rather than the five minutes
    /// a real give-up takes — and dropped the usage frame, which arrives last.
    #[tokio::test]
    async fn a_client_that_leaves_in_passthrough_is_not_called_abandoned() {
        let (tx, rx) = tokio::sync::mpsc::channel::<ClientChunk>(4);
        drop(rx); // the client read the answer and closed, as clients do
        let mut stream: BoxStream<'static, reqwest::Result<Bytes>> =
            Box::pin(futures_util::stream::once(async {
                Ok(Bytes::from_static(b"data: [DONE]\n\n"))
            }));
        let pull = tokio::time::timeout(
            Duration::from_secs(5),
            next_chunk(
                &mut stream,
                &tx,
                Duration::from_millis(10),
                false,
                false,
                None,
            ),
        )
        .await
        .expect("it must still read the stream out");
        assert!(
            matches!(pull, Pull::Chunk(_)),
            "in passthrough the close is normal; the frame is delivered and the \
             departure is found on the next send, if there is one"
        );
    }

    #[tokio::test]
    async fn an_upstream_that_ends_is_not_mistaken_for_a_departed_client() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<ClientChunk>(4);
        let mut stream: BoxStream<'static, reqwest::Result<Bytes>> =
            Box::pin(futures_util::stream::empty());
        let pull = next_chunk(
            &mut stream,
            &tx,
            Duration::from_millis(10),
            true,
            false,
            None,
        )
        .await;
        assert!(matches!(pull, Pull::Done));
    }

    #[tokio::test]
    async fn a_chunk_comes_through_unchanged() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<ClientChunk>(4);
        let mut stream: BoxStream<'static, reqwest::Result<Bytes>> =
            Box::pin(futures_util::stream::once(async {
                Ok(Bytes::from_static(b"data: hi\n\n"))
            }));
        let pull = next_chunk(
            &mut stream,
            &tx,
            Duration::from_millis(10),
            true,
            false,
            None,
        )
        .await;
        match pull {
            Pull::Chunk(c) => assert_eq!(c.as_ref(), b"data: hi\n\n"),
            _ => panic!("the chunk must be delivered, not swallowed"),
        }
    }
}

#[cfg(test)]
mod hold_tests {
    use super::*;

    #[test]
    fn a_zero_bound_never_fires() {
        // The shipped default. Flushing on a timer hands whatever comes next to
        // the client, and when that was a `optscale_kompressor_retrieve` call the client
        // could not answer it — one benchmark run, 52 minutes, task failed.
        assert!(!held_too_long(Duration::ZERO, Duration::from_secs(3600)));
    }

    #[test]
    fn a_set_bound_fires_once_it_is_reached_and_not_before() {
        let bound = Duration::from_secs(45);
        assert!(!held_too_long(bound, Duration::from_secs(44)));
        assert!(held_too_long(bound, bound));
        assert!(held_too_long(bound, Duration::from_secs(46)));
    }
}

/// Taking our own call out of a turn the client is about to receive.
#[cfg(test)]
mod scrub_tests {
    use super::*;

    fn frame_of(raw: &str) -> SseFrame {
        let mut decoder = SseDecoder::new();
        decoder.push(raw.as_bytes());
        decoder.next_frame().expect("one frame")
    }

    /// The Responses terminal frame repeats the whole output list, so dropping
    /// the call's own frames is not enough: it would still arrive here.
    #[test]
    fn the_responses_terminal_frame_loses_our_call_from_its_output_list() {
        let raw = concat!(
            r#"data: {"type":"response.completed","response":{"status":"completed","output":["#,
            r#"{"type":"function_call","name":"optscale_kompressor_retrieve","call_id":"call_a"},"#,
            r#"{"type":"function_call","name":"read_file","call_id":"call_b"}]}}"#,
            "\n\n"
        );
        let out = scrub(ApiFamily::OpenAiResponses, &frame_of(raw), true).expect("rewritten");
        let text = String::from_utf8(out.to_vec()).unwrap();
        assert!(!text.contains("optscale_kompressor_retrieve"), "{text}");
        assert!(text.contains("read_file"), "{text}");
        assert!(
            text.contains(r#""status":"completed""#),
            "the turn did complete"
        );
    }

    #[test]
    fn a_terminal_frame_that_never_named_our_call_is_left_exactly_as_it_arrived() {
        let raw = concat!(
            r#"data: {"type":"response.completed","response":{"output":["#,
            r#"{"type":"function_call","name":"read_file","call_id":"call_b"}]}}"#,
            "\n\n"
        );
        assert!(scrub(ApiFamily::OpenAiResponses, &frame_of(raw), true).is_none());
    }

    /// With no other call left, a turn that still says it ended in a tool call
    /// leaves the client waiting for a result it will never be asked for. On
    /// Anthropic that is a hang, not a hiccup.
    #[test]
    fn anthropic_stops_claiming_tool_use_when_ours_was_the_only_call() {
        let raw = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n";
        let out = scrub(ApiFamily::AnthropicMessages, &frame_of(raw), false).expect("rewritten");
        let text = String::from_utf8(out.to_vec()).unwrap();
        assert!(text.contains("end_turn"), "{text}");
        assert!(!text.contains("tool_use"), "{text}");
        assert!(
            text.starts_with("event: message_delta\n"),
            "anthropic dispatches on the event line, so it has to survive: {text}"
        );
    }

    #[test]
    fn anthropic_keeps_tool_use_while_the_client_still_has_a_call_to_answer() {
        let raw = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n";
        assert!(scrub(ApiFamily::AnthropicMessages, &frame_of(raw), true).is_none());
    }

    #[test]
    fn chat_stops_claiming_tool_calls_when_ours_was_the_only_one() {
        let raw = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n";
        let out = scrub(ApiFamily::OpenAiChat, &frame_of(raw), false).expect("rewritten");
        let text = String::from_utf8(out.to_vec()).unwrap();
        assert!(text.contains(r#""finish_reason":"stop""#), "{text}");
        assert!(
            !text.starts_with("event:"),
            "the OpenAI families send no event line and this must not invent one: {text}"
        );
    }

    #[test]
    fn chat_keeps_tool_calls_while_another_call_remains() {
        let raw = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n";
        assert!(scrub(ApiFamily::OpenAiChat, &frame_of(raw), true).is_none());
    }

    #[test]
    fn a_frame_that_is_not_json_is_never_rewritten() {
        for family in [
            ApiFamily::OpenAiChat,
            ApiFamily::OpenAiResponses,
            ApiFamily::AnthropicMessages,
        ] {
            assert!(scrub(family, &frame_of("data: [DONE]\n\n"), false).is_none());
        }
    }
}

/// The bound that stops interception running past what a client will wait for.
#[cfg(test)]
mod intercept_bound_tests {
    use super::intercepted_too_long;
    use std::time::Duration;

    const MIB: usize = 1024 * 1024;

    #[test]
    fn a_hold_inside_both_bounds_keeps_intercepting() {
        let (bytes, time) = intercepted_too_long(MIB, 512, Duration::from_secs(90), Duration::ZERO);
        assert!(!bytes && !time);
    }

    /// The measured failure: 2.5 MB held against a 256 KiB buffering limit that
    /// stopped applying the moment the turn was recognised.
    #[test]
    fn passing_the_byte_bound_ends_the_interception() {
        let (bytes, _) =
            intercepted_too_long(MIB, 2_551_425, Duration::from_secs(90), Duration::ZERO);
        assert!(bytes);
    }

    /// The same failure seen on the clock. The client gave up at 300 s; the
    /// bound has to fire well before that to be worth having.
    #[test]
    fn passing_the_time_bound_ends_the_interception() {
        let (_, time) =
            intercepted_too_long(MIB, 0, Duration::from_secs(90), Duration::from_secs(300));
        assert!(time);
        let (_, still_waiting) =
            intercepted_too_long(MIB, 0, Duration::from_secs(90), Duration::from_secs(14));
        assert!(
            !still_waiting,
            "a legitimate retrieve round took fourteen seconds and must survive"
        );
    }

    #[test]
    fn a_zero_byte_bound_leaves_the_decision_to_the_clock() {
        let (bytes, time) =
            intercepted_too_long(0, usize::MAX, Duration::from_secs(90), Duration::ZERO);
        assert!(!bytes && !time);
    }

    #[test]
    fn both_bounds_off_means_the_old_unbounded_behaviour() {
        let (bytes, time) =
            intercepted_too_long(0, usize::MAX, Duration::ZERO, Duration::from_secs(86_400));
        assert!(!bytes && !time);
    }
}

#[cfg(test)]
mod upstream_error_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn a_truncated_upstream_body_is_not_a_clean_end() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/event-stream\r\n\r\n5\r\nhello\r\nA\r\nabc")
                .await
                .unwrap();
        });
        let response = reqwest::Client::new()
            .get(format!("http://{address}"))
            .send()
            .await
            .unwrap();
        let mut body: BoxStream<'static, reqwest::Result<Bytes>> =
            Box::pin(response.bytes_stream());
        let (tx, _rx) = tokio::sync::mpsc::channel(2);
        assert!(matches!(
            next_chunk(&mut body, &tx, Duration::from_secs(1), false, false, None).await,
            Pull::Chunk(_)
        ));
        let mut failed = false;
        for _ in 0..4 {
            match next_chunk(&mut body, &tx, Duration::from_secs(1), false, false, None).await {
                Pull::Chunk(_) => continue,
                Pull::UpstreamError => {
                    failed = true;
                    break;
                }
                Pull::HeldTooLong => unreachable!("no deadline was given"),
                Pull::Done | Pull::ClientGone => break,
            }
        }
        assert!(
            failed,
            "the truncated chunked body must produce a transport error"
        );
        server.await.unwrap();
    }
}

#[cfg(test)]
mod hold_deadline_tests {
    use super::*;

    /// A stalled upstream must not hold the client past the bound.
    ///
    /// The deadline used to be checked only inside frame processing, reached
    /// after a chunk arrives AND a complete frame decodes. While holding the
    /// client back the relay is parked in `next_chunk`, whose other branches
    /// are "the client left" and "send a heartbeat" — the last of which loops.
    /// An upstream that simply stopped sending was never measured against the
    /// bound, and the hold lasted until the separate upstream read timeout,
    /// 600 s by default, against a stated interception bound of 90 s.
    #[tokio::test]
    async fn a_quiet_upstream_ends_the_hold_at_its_deadline() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<ClientChunk>(64);
        let mut stalled: BoxStream<'static, reqwest::Result<Bytes>> =
            Box::pin(futures_util::stream::pending());

        let deadline = tokio::time::Instant::now() + Duration::from_millis(20);
        let pulled = tokio::time::timeout(
            Duration::from_millis(500),
            next_chunk(
                &mut stalled,
                &tx,
                Duration::from_millis(2),
                true,
                true,
                Some(deadline),
            ),
        )
        .await
        .expect("the deadline has to fire on its own, not be rescued by this timeout");

        assert!(
            matches!(pulled, Pull::HeldTooLong),
            "a quiet upstream past the bound must end the hold, got {pulled:?}"
        );
    }

    /// And with no hold in force there is no deadline to fire.
    #[tokio::test]
    async fn passthrough_has_no_deadline_and_waits() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<ClientChunk>(16);
        let mut stalled: BoxStream<'static, reqwest::Result<Bytes>> =
            Box::pin(futures_util::stream::pending());

        let waited = tokio::time::timeout(
            Duration::from_millis(60),
            next_chunk(
                &mut stalled,
                &tx,
                Duration::from_millis(5),
                false,
                false,
                None,
            ),
        )
        .await;

        assert!(
            waited.is_err(),
            "passthrough holds nothing back, so nothing is overdue"
        );
    }
}
