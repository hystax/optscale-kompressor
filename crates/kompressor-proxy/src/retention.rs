//! Asking OpenAI to keep a prompt's cache entry for a day instead of minutes.
//!
//! OpenAI caches without being asked and charges nothing to write on the
//! models listed in [`SUPPORTED`]; how long it keeps an entry is the request's
//! `prompt_cache_retention`. With `proxy.prompt_cache_retention = "24h"` this
//! module adds `"prompt_cache_retention": "24h"` to a request that names no
//! retention of its own. The field is not part of the prompt, so the cache key
//! the provider computes is the same with or without it.
//!
//! Where it might not be understood it is not sent: only through the `openai`
//! upstream or one on `api.openai.com`, never on the ChatGPT-subscription
//! route that shares the mount, only on Chat Completions and Responses, and
//! only for models OpenAI documents `24h` for. GPT-5.6 and later take
//! `prompt_cache_options.ttl` instead, at a write price, and are left alone.
//!
//! And where it is refused anyway, the request does not fail: an upstream that
//! answers 400 naming the field gets the request again without it, and
//! [`Rejections`] remembers the upstream and model so the field is not added
//! there again for the life of the process.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use kompressor_wire::ApiFamily;
use serde_json::Value;

use crate::bodycodec::BodyEncoding;

/// The request field.
pub(crate) const FIELD: &str = "prompt_cache_retention";
/// The value this module writes.
pub(crate) const EXTENDED: &str = "24h";

/// The models OpenAI documents `prompt_cache_retention: "24h"` for, as of
/// 2026-11. A dated snapshot of one (`gpt-4.1-2025-04-14`) counts as it.
pub(crate) const SUPPORTED: &[&str] = &[
    "gpt-5.5",
    "gpt-5.5-pro",
    "gpt-5.4",
    "gpt-5.2",
    "gpt-5.1-codex-max",
    "gpt-5.1",
    "gpt-5.1-codex",
    "gpt-5.1-codex-mini",
    "gpt-5.1-chat-latest",
    "gpt-5",
    "gpt-5-codex",
    "gpt-4.1",
];

/// The host whose API documents the field.
const OPENAI_HOST: &str = "api.openai.com";
/// The upstream the shipped configs and `kompressor wrap` name for it.
const OPENAI_UPSTREAM: &str = "openai";

/// Whether `model` is one of [`SUPPORTED`], or a dated snapshot of one.
///
/// Matched whole, never by prefix: `gpt-5` must not take in `gpt-5.6`, which
/// prices its writes, nor `gpt-5-mini`, which is not listed.
pub(crate) fn model_supported(model: &str) -> bool {
    let model = model.strip_prefix("openai/").unwrap_or(model);
    SUPPORTED.iter().any(|name| {
        model == *name
            || model.strip_prefix(name).is_some_and(|rest| {
                rest.strip_prefix('-')
                    .is_some_and(|date| date.starts_with(|c: char| c.is_ascii_digit()))
            })
    })
}

/// Whether the upstream is OpenAI's own API.
pub(crate) fn upstream_is_openai(upstream_name: &str, origin: &str) -> bool {
    if upstream_name == OPENAI_UPSTREAM {
        return true;
    }
    let host = origin
        .split_once("://")
        .map_or(origin, |(_, rest)| rest)
        .split(['/', ':'])
        .next()
        .unwrap_or_default();
    host.eq_ignore_ascii_case(OPENAI_HOST)
}

/// Whether the family carries the field.
pub(crate) fn family_supported(family: Option<ApiFamily>) -> bool {
    matches!(
        family,
        Some(ApiFamily::OpenAiChat | ApiFamily::OpenAiResponses)
    )
}

/// Add the field to `body` unless the client named a retention of its own.
/// Returns whether it was added.
pub(crate) fn extend(body: &mut Value) -> bool {
    let Some(object) = body.as_object_mut() else {
        return false;
    };
    if object.contains_key(FIELD) {
        return false;
    }
    object.insert(FIELD.to_string(), Value::String(EXTENDED.to_string()));
    true
}

/// Whether an error body is about the field: the upstream named it.
pub(crate) fn names_field(body: &[u8]) -> bool {
    String::from_utf8_lossy(body).contains(FIELD)
}

/// `body` without the field, in the coding it was in, or `None` when it cannot
/// be read or carried no such field.
pub(crate) fn strip(body: &Bytes, encoding: BodyEncoding, limit: usize) -> Option<Bytes> {
    let plain = encoding.decode(body, limit)?;
    let mut value: Value = serde_json::from_slice(&plain).ok()?;
    value.as_object_mut()?.remove(FIELD)?;
    let bytes = serde_json::to_vec(&value).ok()?;
    encoding.encode(&bytes)
}

/// The upstreams and models that refused the field, so it is not sent there
/// again. Process-wide and never pruned: the set is bounded by the configured
/// upstreams times [`SUPPORTED`].
#[derive(Debug, Default)]
pub struct Rejections {
    refused: Mutex<HashSet<(String, String)>>,
}

impl Rejections {
    pub(crate) fn refused(&self, upstream: &str, model: &str) -> bool {
        self.refused
            .lock()
            .map(|set| set.contains(&(upstream.to_string(), model.to_string())))
            .unwrap_or(false)
    }

    fn refuse(&self, upstream: &str, model: &str) {
        if let Ok(mut set) = self.refused.lock() {
            set.insert((upstream.to_string(), model.to_string()));
        }
    }
}

/// What one request that carries the field needs to recover if it is refused.
#[derive(Clone, Debug)]
pub(crate) struct Guard {
    rejections: Arc<Rejections>,
    upstream: String,
    model: String,
    /// The most a body may decode to, for [`strip`].
    limit: usize,
}

impl Guard {
    pub(crate) fn new(
        rejections: Arc<Rejections>,
        upstream: &str,
        model: &str,
        limit: usize,
    ) -> Guard {
        Guard {
            rejections,
            upstream: upstream.to_string(),
            model: model.to_string(),
            limit,
        }
    }

    /// Whether this upstream and model have refused the field.
    pub(crate) fn refused(&self) -> bool {
        self.rejections.refused(&self.upstream, &self.model)
    }

    /// Record a refusal, once, with a warning: from here on the setting does
    /// nothing for this upstream and model, and that should not be silent.
    pub(crate) fn refuse(&self) {
        if !self.refused() {
            tracing::warn!(
                upstream = %self.upstream,
                model = %self.model,
                "the upstream refused prompt_cache_retention; sending without it from now on"
            );
        }
        self.rejections.refuse(&self.upstream, &self.model);
    }

    pub(crate) fn strip(&self, body: &Bytes, encoding: BodyEncoding) -> Option<Bytes> {
        strip(body, encoding, self.limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_the_documented_models_and_their_snapshots_are_asked() {
        for model in [
            "gpt-5.5",
            "gpt-5",
            "gpt-5-codex",
            "gpt-5.1-codex-mini",
            "gpt-4.1",
            "gpt-4.1-2025-04-14",
            "openai/gpt-5.5",
        ] {
            assert!(model_supported(model), "{model}");
        }
        for model in [
            "gpt-5.6",
            "gpt-5.6-2026-10-01",
            "gpt-5-mini",
            "gpt-5.5-turbo",
            "gpt-4o",
            "o3",
            "",
            "claude-opus-5-5",
        ] {
            assert!(!model_supported(model), "{model}");
        }
    }

    #[test]
    fn only_openais_own_api_is_asked() {
        assert!(upstream_is_openai("openai", "http://127.0.0.1:9"));
        assert!(upstream_is_openai("mine", "https://api.openai.com"));
        assert!(upstream_is_openai("mine", "https://API.openai.com:443"));
        assert!(!upstream_is_openai(
            "codex-desktop-chatgpt",
            "https://chatgpt.com"
        ));
        assert!(!upstream_is_openai("opencode-zen", "https://opencode.ai"));
        assert!(!upstream_is_openai(
            "gateway",
            "http://api.openai.com.example"
        ));
    }

    #[test]
    fn a_retention_the_client_named_is_kept() {
        let mut body = json!({"model": "gpt-5.5", "input": []});
        assert!(extend(&mut body));
        assert_eq!(body[FIELD], EXTENDED);
        let mut theirs = json!({"model": "gpt-5.5", FIELD: "in_memory"});
        assert!(!extend(&mut theirs));
        assert_eq!(theirs[FIELD], "in_memory");
        assert!(!extend(&mut json!([1, 2])));
    }

    #[test]
    fn a_refused_field_comes_back_out_in_the_body_s_own_coding() {
        let body = json!({"model": "gpt-5.5", "input": "hi", FIELD: EXTENDED});
        let plain = Bytes::from(serde_json::to_vec(&body).unwrap());
        let stripped = strip(&plain, BodyEncoding::Identity, 1 << 20).unwrap();
        let value: Value = serde_json::from_slice(&stripped).unwrap();
        assert!(value.get(FIELD).is_none());
        assert_eq!(value["input"], "hi");

        let zstd = BodyEncoding::Zstd.encode(&plain).unwrap();
        let stripped = strip(&zstd, BodyEncoding::Zstd, 1 << 20).unwrap();
        let decoded = BodyEncoding::Zstd.decode(&stripped, 1 << 20).unwrap();
        let value: Value = serde_json::from_slice(&decoded).unwrap();
        assert!(value.get(FIELD).is_none());

        let without = Bytes::from_static(br#"{"model":"gpt-5.5"}"#);
        assert!(strip(&without, BodyEncoding::Identity, 1 << 20).is_none());
    }

    #[test]
    fn a_refusal_is_remembered_per_upstream_and_model() {
        let rejections = Arc::new(Rejections::default());
        let guard = Guard::new(rejections.clone(), "openai", "gpt-5.5", 1 << 20);
        assert!(!guard.refused());
        guard.refuse();
        assert!(guard.refused());
        assert!(rejections.refused("openai", "gpt-5.5"));
        assert!(!rejections.refused("openai", "gpt-5.4"));
        assert!(!rejections.refused("other", "gpt-5.5"));
        assert!(names_field(
            br#"{"error":{"message":"Unknown parameter: 'prompt_cache_retention'."}}"#
        ));
        assert!(!names_field(
            br#"{"error":{"message":"max_output_tokens too small"}}"#
        ));
    }
}
