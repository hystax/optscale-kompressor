//! Leaving out the reasoning of turns that are over.
//!
//! Some providers return a model's reasoning as plain text — a
//! `reasoning_content` field beside the answer, or a `<think>` span inside it —
//! and bill it as input when the client sends it back. Within one turn that is
//! required: a model in the middle of a tool round is given back what it was
//! thinking. Once the user has spoken again it is not; the providers that
//! document the field say to drop it then. A client that does not drop it
//! re-sends every earlier turn's reasoning on every request for the rest of
//! the conversation. `proxy.reasoning_elide_models` names the models for which
//! this proxy drops it instead.
//!
//! # The rule
//!
//! Reasoning is elided from every assistant message that comes **before the
//! request's last user message**. Nothing after it is touched, so a tool round
//! in progress keeps all of its reasoning.
//!
//! That is a function of the request alone: no conversation state, nothing a
//! restart can lose, and the same answer from any process. While a turn runs,
//! the last user message does not move and neither does a byte. When the user
//! speaks again it does move, and the turn just finished loses its reasoning.
//!
//! # What that costs, and why it is opt-in
//!
//! **That one step rewrites messages the provider has cached.** The assistant
//! messages of the finished turn went out whole while it ran, and go out
//! elided from the next user message on, so the cache is lost from the first
//! of them, once per user turn. Everything after that request reads the
//! shorter prefix. Whether the trade pays depends on how much of a turn is
//! reasoning and how long the conversation goes on, which is why nothing is
//! elided unless a model is named, and why the `/prefix` page reports each
//! such turn as history the proxy changed.
//!
//! An earlier design elided reasoning as each message arrived, which would
//! have cost nothing in cache. It cannot work: every assistant message of an
//! agent loop arrives in the middle of a tool round, exactly when the
//! provider wants its reasoning back.
//!
//! # What it never touches
//!
//! Signed or encrypted reasoning — Anthropic `thinking` blocks, Responses
//! `reasoning` items — which the provider validates or expands itself. Only
//! OpenAI Chat Completions bodies are read at all.

use kompressor_engine::RefCollector;
use serde_json::Value;

/// The smallest reasoning worth a marker.
pub(crate) const MIN_BYTES: usize = 400;

const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

/// Whether `model` is one the operator listed. An entry ending in `*` matches
/// by prefix; any other must be the whole name.
pub(crate) fn listed(models: &[String], model: &str) -> bool {
    models.iter().any(|entry| match entry.strip_suffix('*') {
        Some(prefix) => model.starts_with(prefix),
        None => entry == model,
    })
}

/// What one request had taken out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Elided {
    /// Assistant messages that lost reasoning.
    pub(crate) messages: usize,
    /// Bytes removed, net of the markers put in their place.
    pub(crate) bytes: usize,
}

/// The marker reasoning is replaced with, or `None` when it should stay: too
/// small to be worth one, already a marker, or too large to retain while
/// retention is what makes the elision recoverable.
fn marker_for(text: &str, refs: &mut RefCollector) -> Option<String> {
    if text.len() < MIN_BYTES || text.contains("ref:k_") {
        return None;
    }
    if refs.is_enabled() && text.len() > refs.max_ref_bytes() {
        return None;
    }
    let marker = match refs.stash(text) {
        Some(id) => format!("[reasoning from an earlier turn omitted [ref:{id}]]"),
        None => "[reasoning from an earlier turn omitted]".to_string(),
    };
    (marker.len() < text.len()).then_some(marker)
}

/// `content` with the inside of every closed `<think>` span elided, or `None`
/// when nothing was.
fn without_think_spans(content: &str, refs: &mut RefCollector) -> Option<String> {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    let mut changed = false;
    while let Some(open) = rest.find(OPEN) {
        let inner_from = open + OPEN.len();
        let Some(length) = rest[inner_from..].find(CLOSE) else {
            // An unclosed span is not a span: the rest is the answer.
            break;
        };
        let inner = &rest[inner_from..inner_from + length];
        out.push_str(&rest[..inner_from]);
        match marker_for(inner, refs) {
            Some(marker) => {
                out.push_str(&marker);
                changed = true;
            }
            None => out.push_str(inner),
        }
        out.push_str(CLOSE);
        rest = &rest[inner_from + length + CLOSE.len()..];
    }
    out.push_str(rest);
    changed.then_some(out)
}

/// Elide the reasoning of every assistant message before the last user
/// message of a Chat Completions body. Fragments are stashed in `refs`, for
/// the caller to retain before the body is sent.
pub(crate) fn elide_past_turns(body: &mut Value, refs: &mut RefCollector) -> Elided {
    let mut elided = Elided::default();
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return elided;
    };
    let role = |message: &Value| {
        message
            .get("role")
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let Some(last_user) = messages
        .iter()
        .rposition(|message| role(message).as_deref() == Some("user"))
    else {
        return elided;
    };
    for message in &mut messages[..last_user] {
        if role(message).as_deref() != Some("assistant") {
            continue;
        }
        let Some(object) = message.as_object_mut() else {
            continue;
        };
        let mut saved = 0;
        if let Some(Value::String(reasoning)) = object.get_mut("reasoning_content")
            && let Some(marker) = marker_for(reasoning, refs)
        {
            saved += reasoning.len() - marker.len();
            *reasoning = marker;
        }
        if let Some(Value::String(content)) = object.get_mut("content")
            && let Some(shorter) = without_think_spans(content, refs)
        {
            saved += content.len() - shorter.len();
            *content = shorter;
        }
        if saved > 0 {
            elided.messages += 1;
            elided.bytes += saved;
        }
    }
    elided
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn thinking(tag: &str) -> String {
        format!("{tag}: ").repeat(1) + &"weighing the options carefully. ".repeat(20)
    }

    fn assistant(reasoning: &str, calls: bool) -> Value {
        let mut message = json!({"role": "assistant", "content": "ok",
                                 "reasoning_content": reasoning});
        if calls {
            message["tool_calls"] = json!([{"id": "c", "type": "function",
                "function": {"name": "read", "arguments": "{}"}}]);
        }
        message
    }

    fn tool() -> Value {
        json!({"role": "tool", "tool_call_id": "c", "content": "result"})
    }

    fn user(text: &str) -> Value {
        json!({"role": "user", "content": text})
    }

    fn reasoning_of(body: &Value, index: usize) -> &str {
        body["messages"][index]["reasoning_content"]
            .as_str()
            .unwrap()
    }

    fn elide(body: &mut Value) -> (Elided, RefCollector) {
        let mut refs = RefCollector::new(1 << 20);
        let elided = elide_past_turns(body, &mut refs);
        (elided, refs)
    }

    #[test]
    fn a_turn_in_progress_keeps_every_bit_of_its_reasoning() {
        let mut body = json!({"messages": [
            user("fix it"),
            assistant(&thinking("a"), true), tool(),
            assistant(&thinking("b"), true), tool(),
        ]});
        let before = body.clone();
        let (elided, refs) = elide(&mut body);
        assert_eq!(elided, Elided::default());
        assert_eq!(body, before, "nothing precedes the last user message");
        assert!(refs.is_empty());
    }

    #[test]
    fn once_the_user_speaks_again_the_finished_turn_loses_its_reasoning() {
        let mut body = json!({"messages": [
            json!({"role": "system", "content": "be careful"}),
            user("fix it"),
            assistant(&thinking("a"), true), tool(),
            assistant(&thinking("b"), false),
            user("now the tests"),
            assistant(&thinking("c"), true), tool(),
        ]});
        let (elided, refs) = elide(&mut body);
        assert_eq!(elided.messages, 2);
        for index in [2, 4] {
            let marker = reasoning_of(&body, index);
            assert!(
                marker.starts_with("[reasoning from an earlier turn omitted [ref:k_"),
                "{marker}"
            );
        }
        assert_eq!(
            reasoning_of(&body, 6),
            thinking("c"),
            "the turn in progress is whole"
        );
        assert_eq!(
            body["messages"][2]["content"], "ok",
            "the answer is not reasoning"
        );
        assert!(body["messages"][2]["tool_calls"].is_array());
        // Each elided text is retrievable, to the byte.
        let stashed: Vec<&str> = refs.pairs().iter().map(|(_, text)| text.as_str()).collect();
        assert_eq!(stashed, [thinking("a"), thinking("b")]);
        assert_eq!(
            elided.bytes,
            2 * thinking("a").len() - reasoning_of(&body, 2).len() - reasoning_of(&body, 4).len()
        );
    }

    /// What a message is elided to does not depend on what follows it, so a
    /// turn finished long ago reads the same on every later request.
    #[test]
    fn an_elided_turn_reads_the_same_however_far_the_conversation_goes() {
        let opening = [user("fix it"), assistant(&thinking("a"), false)];
        let mut short = json!({"messages": [opening[0], opening[1], user("next")]});
        let mut long = json!({"messages": [
            opening[0], opening[1], user("next"),
            assistant(&thinking("b"), true), tool(),
            assistant(&thinking("c"), false),
            user("and again"),
        ]});
        elide(&mut short);
        elide(&mut long);
        assert_eq!(short["messages"][1], long["messages"][1]);
        // And doing it twice changes nothing more.
        let once = long.clone();
        let (again, _) = elide(&mut long);
        assert_eq!(again, Elided::default());
        assert_eq!(long, once);
    }

    #[test]
    fn inline_think_spans_are_elided_and_the_answer_around_them_is_kept() {
        let inner = thinking("plan");
        let mut body = json!({"messages": [
            user("fix it"),
            {"role": "assistant",
             "content": format!("<think>{inner}</think>Done: see the diff.<think>short</think>")},
            user("thanks"),
        ]});
        let (elided, refs) = elide(&mut body);
        let content = body["messages"][1]["content"].as_str().unwrap();
        assert!(content.starts_with("<think>[reasoning from an earlier turn omitted [ref:k_"));
        assert!(content.ends_with("</think>Done: see the diff.<think>short</think>"));
        assert_eq!(elided.messages, 1);
        assert_eq!(refs.pairs()[0].1, inner);

        // A span that never closes is not a span.
        let mut open = json!({"messages": [
            user("fix it"),
            {"role": "assistant", "content": format!("<think>{inner}")},
            user("thanks"),
        ]});
        let before = open.clone();
        elide(&mut open);
        assert_eq!(open, before);
    }

    #[test]
    fn short_reasoning_and_other_roles_are_left_alone() {
        let mut body = json!({"messages": [
            user(&format!("<think>{}</think>", thinking("mine"))),
            {"role": "tool", "tool_call_id": "c", "content": "x",
             "reasoning_content": thinking("not an assistant")},
            assistant("brief", false),
            {"role": "assistant", "content": null, "reasoning_content": null},
            user("go on"),
        ]});
        let before = body.clone();
        assert_eq!(elide(&mut body).0, Elided::default());
        assert_eq!(body, before);
        // Not a conversation at all.
        assert_eq!(elide(&mut json!({"input": "hi"})).0, Elided::default());
        assert_eq!(
            elide(&mut json!({"messages": [assistant(&thinking("a"), false)]}))
                .0
                .messages,
            0
        );
    }

    #[test]
    fn without_retention_the_marker_carries_no_handle() {
        let mut body = json!({"messages": [
            user("fix it"), assistant(&thinking("a"), false), user("next"),
        ]});
        let mut refs = RefCollector::disabled();
        assert_eq!(elide_past_turns(&mut body, &mut refs).messages, 1);
        assert_eq!(
            reasoning_of(&body, 1),
            "[reasoning from an earlier turn omitted]"
        );
    }

    #[test]
    fn reasoning_too_large_to_retain_is_kept_whole() {
        let mut body = json!({"messages": [
            user("fix it"), assistant(&thinking("a"), false), user("next"),
        ]});
        let mut refs = RefCollector::new(64);
        assert_eq!(elide_past_turns(&mut body, &mut refs).messages, 0);
        assert_eq!(reasoning_of(&body, 1), thinking("a"));
    }

    #[test]
    fn a_model_is_listed_whole_or_by_prefix() {
        let models = vec!["deepseek-reasoner".to_string(), "kimi-k2*".to_string()];
        assert!(listed(&models, "deepseek-reasoner"));
        assert!(listed(&models, "kimi-k2-thinking"));
        assert!(!listed(&models, "deepseek-reasoner-v2"));
        assert!(!listed(&models, "kimi-k1"));
        assert!(!listed(&[], "deepseek-reasoner"));
    }
}
