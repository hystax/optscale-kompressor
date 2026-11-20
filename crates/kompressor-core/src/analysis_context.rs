//! Analytics records policy, never connection credentials or payload settings.
use crate::Config;
use blake2::{Blake2s256, Digest};
use kompressor_stats::context::RunContext;
use serde_json::{Value, json};

pub(crate) fn context(config: &Config) -> RunContext {
    let p = &config.proxy;
    let settings = json!({
        "engine": config.engine,
        "proxy": {
            "compress": p.compress, "min_tokens":p.min_tokens,
            "compress_timeout_ms":p.compress_timeout.as_millis(),
            "upstream_connect_timeout_ms":p.upstream_connect_timeout.as_millis(),
            "upstream_read_timeout_ms":p.upstream_read_timeout.as_millis(),
            "max_body_bytes":p.max_body_bytes,
            "retrieve_tool":p.retrieve_tool, "retrieve_execution":p.retrieve_execution,
            "retrieve_max_rounds":p.retrieve_max_rounds,
            "retrieve_max_tokens_per_request":p.retrieve_max_tokens_per_request,
            "frozen_prefix":p.frozen_prefix, "cache_ttl":p.cache_ttl,
            "prompt_cache_retention":p.prompt_cache_retention,
            "prompt_cache_key":p.prompt_cache_key,
            "tools_order":p.tools_order,
            "sticky_beta":p.sticky_beta,
            "tools_compact":p.tools_compact,
            "reasoning_elide_models":p.reasoning_elide_models,
            "cold_recompact":p.cold_recompact,
            "superseded_reads":p.superseded_reads,
            "matured_reads":p.matured_reads,
            "matured_quiesce_turns":p.matured_quiesce_turns,
            "matured_max_hold_turns":p.matured_max_hold_turns,
            "matured_min_bytes":p.matured_min_bytes,
            "cold_recompact_margin_ms":p.cold_recompact_margin.as_millis(),
            "cache_keep_warm":p.cache_keep_warm,
            "cache_keep_warm_interval_ms":p.cache_keep_warm_interval.as_millis(),
            "cache_keep_warm_max_pings":p.cache_keep_warm_max_pings,
            "cache_keep_warm_background_pings":p.cache_keep_warm_background_pings,
            "keep_inline_max_percent":p.keep_inline_max_percent,
            "stream_buffer_bytes":p.stream_buffer_bytes,
            "stream_buffer_timeout_ms":p.stream_buffer_timeout.as_millis(),
            "providers":p.providers
        },
        "retrieve": config.retrieve
    });
    RunContext {
        build_version: env!("CARGO_PKG_VERSION").into(),
        config_fingerprint: fingerprint(&settings),
        experiment_id: config.stats.experiment_id.clone(),
        run_id: config.stats.run_id.clone().unwrap_or_else(|| {
            format!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            )
        }),
        settings,
    }
}
// Canonicalize recursively: serde_json's preserve_order feature is enabled in
// the workspace, so relying on a map's serialization order is not sufficient.
fn canonical(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let sorted: std::collections::BTreeMap<_, _> =
                m.iter().map(|(k, v)| (k.clone(), canonical(v))).collect();
            serde_json::to_value(sorted).expect("JSON values serialize")
        }
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        _ => v.clone(),
    }
}
fn fingerprint(v: &Value) -> String {
    Blake2s256::digest(serde_json::to_vec(&canonical(v)).expect("JSON values serialize"))
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fingerprint_is_order_independent_but_policy_sensitive() {
        assert_eq!(
            fingerprint(&json!({"b":{"y":2,"x":1},"a":3})),
            fingerprint(&json!({"a":3,"b":{"x":1,"y":2}}))
        );
        let mut config = Config::default();
        let original = context(&config);
        config.auth.secret = "secret-do-not-record".into();
        config.stats.run_id = Some("run-b".into());
        let changed = context(&config);
        assert_eq!(original.config_fingerprint, changed.config_fingerprint);
        assert!(
            !serde_json::to_string(&changed)
                .unwrap()
                .contains("secret-do-not-record")
        );
        config.proxy.compress = !config.proxy.compress;
        assert_ne!(
            original.config_fingerprint,
            context(&config).config_fingerprint
        );
    }
}
