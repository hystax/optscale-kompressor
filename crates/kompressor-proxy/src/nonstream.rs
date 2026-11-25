//! The buffered path: forward, and run the retrieve loop over what comes back.
//!
//! The loop is an agent loop with three properties the client never sees:
//!
//! - **The base prompt never changes.** Each round appends the model's turn and
//!   our answers to it, so the provider's cached prefix still covers everything
//!   before the tail.
//! - **The intermediate rounds are hidden.** The client asked for one
//!   completion and gets one completion; it never asked for a tool it does not
//!   know about, and handing it one is how a client ends up reporting
//!   "unsupported call".
//! - **`usage` is the sum.** The client pays for every round, so it is told
//!   about every round. Reporting only the final round would make a
//!   three-round retrieval look like one cheap call, which is the accounting
//!   error that makes a retrieval loop look free when it is not.
//!
//! A failed recovery round returns an error; private tool calls never become
//! a successful client response.

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, StatusCode};
use serde_json::{Value, json};

use kompressor_wire::ApiFamily;

use crate::failopen::{FailOpen, Stage};
use crate::retrieve::{Budget, Retriever};
use crate::turn::{self, Turn};
use crate::upstream::Outbound;

/// What the buffered path produced.
pub(crate) struct Relayed {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Bytes,
    pub(crate) rounds: u32,
    pub(crate) injected_tokens: u32,
    /// The usage the client was shown, which is the summed usage.
    pub(crate) usage: Option<Value>,
    /// Each round's own usage, round 0 first, for the rounds whose response
    /// was read: what `retrieve_round_usage` is filed from.
    pub(crate) round_usage: Vec<Value>,
}

/// Forward the request and, where the model asks for it, fetch fragments back.
///
/// `retriever` is `None` when the loop must not run at all — client-side
/// execution, a family with no walker, or a request that carries no handle.
pub(crate) async fn run(
    outbound: Outbound,
    family: Option<ApiFamily>,
    retriever: Option<&Retriever>,
    budget: Budget,
    failopen: &FailOpen,
) -> Result<Relayed, reqwest::Error> {
    let mut per_round = Vec::new();
    let mut relayed = run_rounds(
        outbound,
        family,
        retriever,
        budget,
        failopen,
        &mut per_round,
    )
    .await?;
    relayed.round_usage = per_round;
    Ok(relayed)
}

async fn run_rounds(
    mut outbound: Outbound,
    family: Option<ApiFamily>,
    retriever: Option<&Retriever>,
    mut budget: Budget,
    failopen: &FailOpen,
    per_round: &mut Vec<Value>,
) -> Result<Relayed, reqwest::Error> {
    let mut rounds = 0u32;
    let mut injected = 0u32;
    // usage of the rounds the client will never see
    let mut earlier_usage = json!({});
    // Deliberately not kept: the response of a round that is still running.
    //
    // A previous version held the last completed round and relayed it when a
    // later one failed, so the client got an answer rather than an error. But
    // the loop only continues past a round when that round asked to retrieve,
    // so the held body *always* contained a `optscale_kompressor_retrieve` call — a tool
    // the client never declared. Handing one over is the failure this design
    // exists to prevent: codex answers `unsupported call: optscale_kompressor_retrieve`,
    // then runs to its output cap building a huge argument string and the next
    // request 400s. It is why `stream_buffer_timeout` ships at 0.
    //
    // So a mid-loop failure is reported as the failure it is. The client sees a
    // provider-shaped error and retries; it does not see a turn it cannot act
    // on.
    //
    // The budget being spent is not an exception to that. An earlier version
    // relayed the round after a refusal whatever it contained, and a model that
    // answered the refusal by calling `optscale_kompressor_retrieve` again put the tool
    // straight into the client's hands — the very failure described above,
    // reached by a different door. Measured against codex: the router answers
    // `unsupported call: optscale_kompressor_retrieve` and the turn is wasted.
    //
    // So a refusal is answered like any other call, and the loop continues. A
    // model that ignores the refusal indefinitely cannot be allowed to spin, so
    // the refusals are counted and the request fails visibly once they run out.
    let mut refusals = 0u32;

    loop {
        let response = match outbound.send().await {
            Ok(response) => response,
            Err(error) => {
                if rounds > 0 {
                    failopen.record(Stage::RetrieveRound, &error);
                }
                return Err(error);
            }
        };
        let status = response.status();
        let headers = response.headers().clone();
        let body = match response.bytes().await {
            Ok(body) => body,
            Err(error) => {
                if rounds > 0 {
                    failopen.record(Stage::RetrieveRound, &error);
                }
                return Err(error);
            }
        };

        let Some(family) = family else {
            return Ok(finish(status, headers, body, rounds, injected, None));
        };

        if !status.is_success() {
            if rounds > 0 {
                failopen.record(Stage::RetrieveRound, &status);
            }
            // The provider's own error, relayed as it arrived. Substituting an
            // earlier round would mean handing over that round's retrieve call.
            return Ok(finish(status, headers, body, rounds, injected, None));
        }

        let Ok(parsed) = serde_json::from_slice::<Value>(&body) else {
            // a success we cannot read is still a success; relay it
            return Ok(finish(status, headers, body, rounds, injected, None));
        };
        // This round's own figure, before it is folded into the sum: the
        // sum cannot say which round read what.
        per_round.push(turn::usage_of(&parsed).cloned().unwrap_or(Value::Null));

        // Client-side execution and requests carrying no handle still come
        // through here, so that what the provider billed is recorded either
        // way; they simply never take a round.
        let Some(retriever) = retriever else {
            return Ok(complete(
                status,
                headers,
                body,
                parsed,
                &earlier_usage,
                rounds,
                injected,
            ));
        };

        let calls = match turn::classify(family, &parsed) {
            Turn::OtherTool => {
                let mut parsed = parsed;
                let body = if turn::strip_retrieve_calls(family, &mut parsed) {
                    Bytes::from(parsed.to_string())
                } else {
                    body
                };
                return Ok(complete(
                    status,
                    headers,
                    body,
                    parsed,
                    &earlier_usage,
                    rounds,
                    injected,
                ));
            }
            Turn::Answered => {
                return Ok(complete(
                    status,
                    headers,
                    body,
                    parsed,
                    &earlier_usage,
                    rounds,
                    injected,
                ));
            }
            Turn::Retrieve(calls) => calls,
        };

        let spent = budget.is_spent();
        if spent {
            refusals += 1;
            if refusals > MAX_REFUSAL_ROUNDS {
                // Not failopen.record, for the reason given at the other
                // 502 below: that counter means "forwarded the request as it
                // arrived" and says so in its log line. This answers an error.
                tracing::warn!(
                    stage = "retrieve_round",
                    "the model kept calling retrieve after the budget refused it; answering 502"
                );
                return Ok(refusals_exhausted(rounds, injected));
            }
        }
        let mut results = Vec::with_capacity(calls.len());
        let mut round_tokens = 0u32;
        // The allowance is spent within the round, not after it. Each answer is
        // measured against what is left and refused when it does not fit, so a
        // single oversized fragment — or several ordinary ones — cannot carry
        // the request past the ceiling before anything checks.
        let mut left = budget.remaining();
        for call in &calls {
            let (payload, tokens) = if spent {
                retriever.refuse(call)
            } else {
                let (payload, tokens) = retriever.answer(call);
                if tokens > left {
                    tracing::warn!(
                        stage = "retrieve_round",
                        tokens,
                        remaining = left,
                        "refusing a retrieval that would pass the per-request token budget"
                    );
                    retriever.refuse(call)
                } else {
                    (payload, tokens)
                }
            };
            left = left.saturating_sub(tokens);
            round_tokens = round_tokens.saturating_add(tokens);
            results.push((call.clone(), payload));
        }
        // `proxy.retrieve_attach_siblings`: what is left of the allowance may
        // carry the rest of each asked-for output, so the next round need not.
        if !spent && let Some(submission) = outbound.json() {
            let attached = retriever.attach_siblings(submission, &mut results, left);
            round_tokens = round_tokens.saturating_add(attached.tokens);
        }

        let mut next = outbound.json().cloned().unwrap_or(Value::Null);
        if !turn::append_round(family, &mut next, &parsed, &results) || !outbound.set_json(next) {
            // Deliberately NOT failopen.record: that counter means "forwarded
            // the request as it arrived", and logs exactly those words. This
            // path now answers 502 rather than handing over a response holding
            // a retrieve call the client cannot answer — the opposite of
            // failing open, and counting it there would make both the ledger
            // and its log line false.
            tracing::warn!(
                stage = "retrieve_round",
                "could not build the next retrieval round; answering 502"
            );
            return Ok(loop_error(
                rounds,
                injected,
                "retrieve_round_failed",
                "kompressor: could not build the next retrieval round",
            ));
        }
        if let Some(submission) = outbound.json() {
            retriever.capture_round(rounds + 1, submission);
        }

        if let Some(usage) = turn::usage_of(&parsed) {
            turn::add_usage(&mut earlier_usage, usage);
        }
        budget.spend(round_tokens);
        injected = injected.saturating_add(round_tokens);
        rounds += 1;
    }
}

/// How many rounds of "no" the loop will send before giving up on the model
/// taking the hint. Two is enough for a model that misreads the first refusal
/// and few enough that a model ignoring them costs one extra round, not a
/// session.
pub(crate) const MAX_REFUSAL_ROUNDS: u32 = 2;

/// The answer when a model will not stop asking for fragments it cannot have.
///
/// Relaying its turn is not an option — it holds a call for a tool the client
/// never declared — so the request ends as a provider-shaped error the client
/// already knows how to retry.
fn refusals_exhausted(rounds: u32, injected: u32) -> Relayed {
    loop_error(
        rounds,
        injected,
        "retrieve_loop_not_terminating",
        "kompressor: the retrieval budget was spent and the model kept calling optscale_kompressor_retrieve after being told so",
    )
}

fn loop_error(rounds: u32, injected: u32, code: &str, message: &str) -> Relayed {
    let body = json!({"error": {
        "message": message, "type": "kompressor_retrieve_loop", "code": code,
    }});
    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Relayed {
        status: StatusCode::BAD_GATEWAY,
        headers,
        body: Bytes::from(body.to_string()),
        rounds,
        injected_tokens: injected,
        usage: None,
        round_usage: Vec::new(),
    }
}

/// Assemble the response the client sees, with the earlier rounds folded into
/// its usage.
///
/// A response nothing was added to is relayed as the exact bytes the provider
/// sent. Re-serializing it would be harmless in meaning and gratuitous in
/// practice: field order, spacing and any field we do not model would all move,
/// on every single request, to no purpose.
#[allow(clippy::too_many_arguments)]
fn complete(
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
    mut parsed: Value,
    earlier_usage: &Value,
    rounds: u32,
    injected: u32,
) -> Relayed {
    if earlier_usage
        .as_object()
        .is_none_or(|usage| usage.is_empty())
    {
        let usage = turn::usage_of(&parsed).cloned();
        return finish(status, headers, body, rounds, injected, usage);
    }
    turn::merge_usage(&mut parsed, earlier_usage);
    let usage = turn::usage_of(&parsed).cloned();
    let rewritten = serde_json::to_vec(&parsed).map(Bytes::from).unwrap_or(body);
    finish(status, headers, rewritten, rounds, injected, usage)
}

fn finish(
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
    rounds: u32,
    injected: u32,
    usage: Option<Value>,
) -> Relayed {
    Relayed {
        status,
        headers,
        body,
        rounds,
        injected_tokens: injected,
        usage,
        round_usage: Vec::new(),
    }
}
