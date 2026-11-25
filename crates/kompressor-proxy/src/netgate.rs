//! Handing the engine the net-cost elision gate (`proxy.elision_gate =
//! "net_cost"`), built from what this proxy knows of the conversation.
//!
//! The engine decides; this only supplies the inputs, and they are the ones
//! that move turn to turn: the prompt the provider last billed for the
//! conversation, and the cache prices for the model and TTL this request will
//! be sent under. The pins the engine reports for messages it decided for the
//! first time are filed in the [`PrefixTracker`] and handed back on every
//! later request, which is what keeps a message's bytes the same as the
//! conversation grows behind it (`kompressor_engine::gate`).

use std::sync::Arc;

use kompressor_core::config::{ElisionGate as GateSetting, ProxyConfig};
use kompressor_engine::gate::{ElisionGate, GatePin};
use kompressor_engine::{EngineOptions, Mode};
use kompressor_wire::ApiFamily;

use crate::prefixcache::{CachePrices, PrefixTracker};

/// `options` with the net-cost gate set, where it applies.
///
/// It applies in `recoverable` mode, which alone mints handles, and only to a
/// request with a conversation: without one there is nowhere to keep a pin, a
/// message would be re-decided every turn against a prompt that has grown, and
/// the fixed gate — which needs no pin — is the honest fallback.
///
/// `estimated_prompt` stands in for the billed prompt on a conversation's
/// first request, before any usage has been reported for it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn gated_options(
    options: EngineOptions,
    proxy: &ProxyConfig,
    prefixes: &PrefixTracker,
    conversation: Option<u64>,
    family: ApiFamily,
    model: Option<&str>,
    long_ttl: bool,
    estimated_prompt: u32,
) -> EngineOptions {
    if proxy.elision_gate != GateSetting::NetCost || options.mode != Mode::Recoverable {
        return options;
    }
    let Some(conversation) = conversation else {
        return options;
    };
    let prices = CachePrices::for_model(family, model, long_ttl);
    let current = GatePin {
        prefix_tokens: prefixes
            .last_prompt(conversation)
            .unwrap_or(estimated_prompt),
        write_ppm: prices.write_ppm,
        read_ppm: prices.read_ppm,
    };
    EngineOptions {
        elision_gate: Some(ElisionGate {
            model: Arc::new(proxy.net_cost_gate.clone()),
            current,
            pinned: Arc::new(prefixes.gate_pins(Some(conversation))),
        }),
        ..options
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn net_cost() -> ProxyConfig {
        ProxyConfig {
            elision_gate: GateSetting::NetCost,
            ..ProxyConfig::default()
        }
    }

    #[test]
    fn the_fixed_gate_and_a_conversationless_request_are_left_alone() {
        let prefixes = PrefixTracker::new(Duration::ZERO);
        let fixed = gated_options(
            EngineOptions::default(),
            &ProxyConfig::default(),
            &prefixes,
            Some(1),
            ApiFamily::AnthropicMessages,
            None,
            false,
            1_000,
        );
        assert!(fixed.elision_gate.is_none());
        let anonymous = gated_options(
            EngineOptions::default(),
            &net_cost(),
            &prefixes,
            None,
            ApiFamily::AnthropicMessages,
            None,
            false,
            1_000,
        );
        assert!(anonymous.elision_gate.is_none());
        let aggressive = gated_options(
            EngineOptions {
                mode: Mode::Aggressive,
                ..EngineOptions::default()
            },
            &net_cost(),
            &prefixes,
            Some(1),
            ApiFamily::AnthropicMessages,
            None,
            false,
            1_000,
        );
        assert!(
            aggressive.elision_gate.is_none(),
            "no handles, nothing to gate"
        );
    }

    #[test]
    fn the_gate_reads_the_last_billed_prompt_the_prices_and_the_pins() {
        let prefixes = PrefixTracker::new(Duration::ZERO);
        let build = |prefixes: &PrefixTracker| {
            gated_options(
                EngineOptions::default(),
                &net_cost(),
                prefixes,
                Some(7),
                ApiFamily::AnthropicMessages,
                Some("claude-opus-5-5"),
                true,
                1_234,
            )
            .elision_gate
            .expect("gated")
        };
        let first = build(&prefixes);
        assert_eq!(
            first.current.prefix_tokens, 1_234,
            "estimated before any usage"
        );
        assert_eq!(first.current.read_ppm, 50_000);
        assert_eq!(first.current.write_ppm, 2_000_000, "one-hour entries");

        // A retrieve turn of two upstream calls reports both prompts summed.
        prefixes.record_prompt(7, 90_000, 1);
        let pin = GatePin {
            prefix_tokens: 10,
            write_ppm: 1,
            read_ppm: 1,
        };
        prefixes.record_gate_pins(Some(7), &[(42, pin)]);
        prefixes.record_gate_pins(
            Some(7),
            &[(
                42,
                GatePin {
                    prefix_tokens: 99,
                    ..pin
                },
            )],
        );
        let later = build(&prefixes);
        assert_eq!(later.current.prefix_tokens, 45_000);
        assert_eq!(later.pinned.get(&42), Some(&pin), "the first pin stands");
    }
}
