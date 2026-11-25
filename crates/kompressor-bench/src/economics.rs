//! Pricing a conversation corpus under the fixed elision gate and under the
//! net-cost one, for `kompressor bench price`.
//!
//! # Why a replay and not live runs
//!
//! DESIGN §19: cost per output token correlates +0.80 with run length, so two
//! live runs of different trajectories cannot be compared however carefully
//! they are matched. Replaying the *same* recorded turns under both gates
//! removes the trajectory from the comparison: the only thing that differs is
//! which payloads each gate let the engine elide.
//!
//! # What is priced
//!
//! Each conversation is replayed one request at a time — request `k` carries
//! turns `0..=k`, the way an agent re-sends its history — so the net-cost gate
//! sees the prompt grow and pins each message when it first arrives, exactly
//! as the proxy does. For every message the engine elided behind a handle:
//!
//! * **removal value** = `ΔT·(w + r·(reads − 1))`: the tokens removed, written
//!   to the cache once and read back on every later request that carries the
//!   message. `reads` is the *actual* number of requests that carried it in
//!   the recording, not the gate's expectation.
//! * **fetch cost** = `p_fetch·(r·prompt + output_cost)`: the expected price of
//!   the retrieve round the handle invites, at the prompt the message arrived
//!   in. A replay cannot know which handles a model would have fetched, so
//!   this is the *expected* cost under the configured fetch-probability table,
//!   the same one the gate decides with.
//!
//! Everything is in tokens at the provider's uncached input price; `w` and `r`
//! come from the provider's published price list ([`prices_for`]). Elisions
//! without a handle invite no fetch and are counted at their removal value in
//! both arms alike.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use kompressor_engine::gate::{ElisionGate, GatePin, NetCostModel};
use kompressor_engine::messages::{
    CompressStats, Message, MessageContent, RequestOptions, compress_messages,
};
use kompressor_engine::pipeline::compress_content;
use kompressor_engine::types::Context;
use kompressor_engine::{EngineOptions, estimate_tokens, refs_in};
use serde::Serialize;

use crate::conversation::Conversation;

/// Which gate an arm replays under.
#[derive(Clone, Debug)]
pub enum Gate {
    /// `min_elision_saving_chars` at this many characters.
    Fixed(usize),
    /// The net-cost gate under this model.
    NetCost(Arc<NetCostModel>),
}

/// What one arm came to, for one provider.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Priced {
    pub conversations: u32,
    /// Messages the engine elided behind at least one handle.
    pub handle_messages: u32,
    /// Tokens removed, counted once per message.
    pub removed_tokens: u64,
    /// See the module documentation. Tokens at the uncached input price.
    pub removal_value: f64,
    pub fetch_cost: f64,
}

impl Priced {
    /// Removal value minus fetch cost: what the arm is worth.
    pub fn net(&self) -> f64 {
        self.removal_value - self.fetch_cost
    }

    fn add(&mut self, other: &Priced) {
        self.conversations += other.conversations;
        self.handle_messages += other.handle_messages;
        self.removed_tokens += other.removed_tokens;
        self.removal_value += other.removal_value;
        self.fetch_cost += other.fetch_cost;
    }
}

/// A provider's cache prices for `model`, in millionths of its uncached input
/// price: Anthropic writes at 1.25x and reads at a tenth, except the models
/// whose reads are priced lower; OpenAI writes at the input price. The same
/// list the proxy prices promotions and the gate at
/// (`kompressor_proxy::prefixcache::CachePrices`), for five-minute entries.
pub fn prices_for(provider: &str, model: &str) -> GatePin {
    let model = model.strip_prefix("anthropic/").unwrap_or(model);
    if provider == "openai" {
        return GatePin {
            prefix_tokens: 0,
            write_ppm: 1_000_000,
            read_ppm: 100_000,
        };
    }
    let read_ppm = if model.starts_with("claude-opus-5-5") || model.starts_with("claude-sonnet-5-5")
    {
        50_000
    } else if model.starts_with("claude-fable-5-1") || model.starts_with("claude-mythos-5-1") {
        25_000
    } else {
        100_000
    };
    GatePin {
        prefix_tokens: 0,
        write_ppm: 1_250_000,
        read_ppm,
    }
}

/// Price every conversation under `gate`, by provider. `pricing` is the model
/// the fetch cost is priced with — the net-cost arm's own, so both arms are
/// judged by the same expectation. `outside_tokens` stands in for the system
/// prompt and tool schemas a corpus does not record.
pub fn price(
    conversations: &[Conversation],
    options: &EngineOptions,
    gate: &Gate,
    pricing: &NetCostModel,
    outside_tokens: u32,
) -> BTreeMap<String, Priced> {
    let mut out: BTreeMap<String, Priced> = BTreeMap::new();
    for conversation in conversations {
        let provider = if conversation.provider.is_empty() {
            "unknown"
        } else {
            conversation.provider.as_str()
        };
        let priced = price_one(conversation, options, gate, pricing, outside_tokens);
        out.entry(provider.to_string()).or_default().add(&priced);
    }
    out
}

fn messages_of(conversation: &Conversation, upto: usize) -> Vec<Message> {
    conversation.turns[..upto]
        .iter()
        .map(|turn| Message::text(turn.role.clone(), turn.text.clone()))
        .collect()
}

fn text_of(message: &Message) -> &str {
    match &message.content {
        MessageContent::Text(text) => text,
        _ => "",
    }
}

fn run(messages: &mut [Message], options: &EngineOptions) -> CompressStats {
    let mut refs = options.collector();
    compress_messages(messages, options, &RequestOptions::default(), &mut refs)
}

fn price_one(
    conversation: &Conversation,
    options: &EngineOptions,
    gate: &Gate,
    pricing: &NetCostModel,
    outside_tokens: u32,
) -> Priced {
    let n = conversation.turns.len();
    let prices = prices_for(&conversation.provider, &conversation.model);
    let tokens_of = |message: &Message| estimate_tokens(text_of(message), &message.role);

    // The compressed conversation, each message as it shipped on every
    // request that carried it.
    let shipped: Vec<Message> = match gate {
        Gate::Fixed(chars) => {
            let options = EngineOptions {
                min_elision_saving_chars: *chars,
                elision_gate: None,
                ..options.clone()
            };
            let mut messages = messages_of(conversation, n);
            run(&mut messages, &options);
            messages
        }
        Gate::NetCost(model) => {
            // Request by request, so each message is decided against the
            // prompt of the request before it and pinned there, as the proxy
            // does. The engine's prefix-monotonicity makes the last request's
            // output every message's output.
            let mut pinned: BTreeMap<u64, GatePin> = BTreeMap::new();
            let mut prompt = outside_tokens;
            let mut messages = Vec::new();
            for upto in 1..=n {
                messages = messages_of(conversation, upto);
                let options = EngineOptions {
                    elision_gate: Some(ElisionGate {
                        model: model.clone(),
                        current: GatePin {
                            prefix_tokens: prompt,
                            ..prices
                        },
                        pinned: Arc::new(pinned.clone()),
                    }),
                    ..options.clone()
                };
                let stats = run(&mut messages, &options);
                for (key, pin) in stats.gate_pins {
                    pinned.entry(key).or_insert(pin);
                }
                prompt = outside_tokens.saturating_add(stats.tokens_out);
            }
            messages
        }
    };

    let original = messages_of(conversation, n);
    let mut priced = Priced {
        conversations: 1,
        ..Priced::default()
    };
    let w = f64::from(prices.write_ppm) / 1_000_000.0;
    let r = f64::from(prices.read_ppm) / 1_000_000.0;
    let mut prompt = u64::from(outside_tokens);
    // Handles already priced: a later message folding onto an earlier one's
    // handle invites no fetch the earlier one did not.
    let mut priced_handles: BTreeSet<String> = BTreeSet::new();
    for (index, (before, after)) in original.iter().zip(&shipped).enumerate() {
        prompt += u64::from(tokens_of(after));
        let removed = tokens_of(before).saturating_sub(tokens_of(after));
        if removed == 0 {
            continue;
        }
        let reads = (n - index) as f64;
        priced.removed_tokens += u64::from(removed);
        priced.removal_value += f64::from(removed) * (w + r * (reads - 1.0));
        let new_handles: Vec<String> = refs_in(text_of(after))
            .into_iter()
            .map(|id| id.to_string())
            .filter(|id| priced_handles.insert(id.clone()))
            .collect();
        if new_handles.is_empty() {
            continue;
        }
        priced.handle_messages += 1;
        let saved_chars = text_of(before)
            .chars()
            .count()
            .saturating_sub(text_of(after).chars().count());
        let p = pricing.fetch_probability_for(kind_of(before, options), saved_chars);
        priced.fetch_cost += p * (r * prompt as f64 + pricing.output_cost_tokens);
    }
    priced
}

/// The kind the engine routes a payload to, for the fetch-probability table.
fn kind_of(message: &Message, options: &EngineOptions) -> Option<kompressor_engine::Kind> {
    let options = EngineOptions {
        elision_gate: None,
        min_elision_saving_chars: 0,
        ..options.clone()
    };
    let mut refs = options.collector();
    let mut applied = Vec::new();
    let ctx = Context::for_role(&message.role);
    compress_content(
        text_of(message),
        &message.role,
        &options,
        None,
        &mut refs,
        &ctx,
        &mut applied,
    )
    .1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::Turn;

    fn search(module: &str) -> String {
        (0..5)
            .flat_map(|f| {
                (0..40).map(move |i| {
                    format!(
                        "src/{module}_{f}/handler.rs:{}:    let value_{i} = compute_thing(input_{i}, &state);",
                        i * 3 + 1
                    )
                })
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn conversation(provider: &str, tools: usize) -> Conversation {
        let mut turns = vec![Turn {
            role: "user".into(),
            text: "find every caller".into(),
        }];
        for t in 0..tools {
            turns.push(Turn {
                role: "assistant".into(),
                text: format!("searching batch {t}"),
            });
            turns.push(Turn {
                role: "tool".into(),
                text: search(&format!("batch{t}")),
            });
        }
        Conversation {
            id: format!("{provider}-{tools}"),
            task: "fixture".into(),
            provider: provider.into(),
            model: String::new(),
            turns,
        }
    }

    #[test]
    fn a_fixed_gate_above_every_saving_elides_nothing_behind_a_handle() {
        let corpus = vec![conversation("anthropic", 3)];
        let model = NetCostModel::default();
        let shut = price(
            &corpus,
            &EngineOptions::default(),
            &Gate::Fixed(usize::MAX),
            &model,
            0,
        );
        assert_eq!(shut["anthropic"].handle_messages, 0);
        assert_eq!(shut["anthropic"].fetch_cost, 0.0);
        let open = price(
            &corpus,
            &EngineOptions::default(),
            &Gate::Fixed(0),
            &model,
            0,
        );
        assert_eq!(open["anthropic"].handle_messages, 3);
        assert!(open["anthropic"].fetch_cost > 0.0);
        assert!(open["anthropic"].removal_value > shut["anthropic"].removal_value);
    }

    #[test]
    fn the_net_cost_gate_refuses_what_a_long_prompt_makes_unprofitable_and_is_never_worse() {
        // A conversation whose system prompt and tools are huge: every fetch
        // re-reads them, so a handle is expected to cost more than it saves.
        let corpus = vec![conversation("openai", 3), conversation("anthropic", 2)];
        let model = Arc::new(NetCostModel::default());
        let options = EngineOptions::default();
        let fixed = price(&corpus, &options, &Gate::Fixed(0), &model, 2_000_000);
        let net = price(
            &corpus,
            &options,
            &Gate::NetCost(model.clone()),
            &model,
            2_000_000,
        );
        for provider in ["openai", "anthropic"] {
            assert_eq!(net[provider].handle_messages, 0, "{provider}: all refused");
            assert!(fixed[provider].handle_messages > 0);
            assert!(
                net[provider].net() >= fixed[provider].net(),
                "{provider}: net-cost {} against fixed {}",
                net[provider].net(),
                fixed[provider].net()
            );
        }
        // On a short prompt it admits them all, as the open fixed gate does.
        let net_short = price(&corpus, &options, &Gate::NetCost(model.clone()), &model, 0);
        let fixed_short = price(&corpus, &options, &Gate::Fixed(0), &model, 0);
        assert_eq!(net_short, fixed_short);
    }

    #[test]
    fn prices_follow_the_providers_list() {
        assert_eq!(prices_for("openai", "gpt-5").write_ppm, 1_000_000);
        assert_eq!(prices_for("anthropic", "claude-opus-5-5").read_ppm, 50_000);
        assert_eq!(
            prices_for("anthropic", "anthropic/claude-sonnet-4-5").read_ppm,
            100_000
        );
    }
}
