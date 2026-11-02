//! A provider that caches the way Anthropic's prompt cache does, so a test can
//! ask what a conversation *would* have cost rather than what a mock was told
//! to report.
//!
//! Every other Anthropic test in this crate hands the proxy usage figures
//! written into the mock. That checks what the proxy does with an answer, and
//! says nothing about whether the bytes it sent would have been served from
//! cache. This computes the answer from the bytes:
//!
//! * The prompt is laid out in cache order — tools, then system, then messages
//!   — one block per tool, system block and content block, and each prefix is
//!   hashed. `cache_control` is left out of the hash, as it is not content.
//! * A block carrying `cache_control` is a breakpoint. The lookup walks back up
//!   to [`LOOKBACK`] blocks from each breakpoint for an entry an earlier request
//!   wrote, and the longest live one is the cache read.
//! * Everything from there to the last breakpoint is written, each span at the
//!   lifetime of the breakpoint that closes it, and a hit renews its entry.
//! * Prefixes shorter than [`MIN_CACHEABLE`] tokens are neither read nor written.
//!
//! Tokens are bytes of canonical JSON over four. That is not Anthropic's
//! tokenizer, so the dollar figures are estimates; both arms of a comparison are
//! measured with the same ruler, which is what the comparison needs. The model
//! never answers anything but "ok", so output, retrieval rounds and extra agent
//! steps — the costs a model's *behaviour* adds — are outside what this measures.
//! Harbor runs remain the measure of those.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use wiremock::{Request, Respond, ResponseTemplate};

/// How far back from a breakpoint the provider looks for an earlier entry.
pub const LOOKBACK: usize = 20;
/// The shortest prefix the provider caches, in tokens.
pub const MIN_CACHEABLE: u64 = 1024;
/// Output tokens every answer reports. The same in every arm.
pub const OUTPUT_TOKENS: u64 = 1;

const FIVE_MINUTES_MS: u64 = 5 * 60 * 1000;
const ONE_HOUR_MS: u64 = 60 * 60 * 1000;

/// What the provider billed one request for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Input neither read from nor written to the cache.
    pub fresh: u64,
    pub read: u64,
    pub write_5m: u64,
    pub write_1h: u64,
    /// Tokens up to the request's last breakpoint: what the *next* request of
    /// the same conversation should be able to read, if nothing it repeats has
    /// changed and the entry has not expired.
    pub frontier: u64,
}

impl Usage {
    pub fn prompt(&self) -> u64 {
        self.fresh + self.read + self.write_5m + self.write_1h
    }
}

/// Published per-million-token prices, in dollars.
#[derive(Clone, Copy, Debug)]
pub struct Prices {
    pub fresh: f64,
    pub read: f64,
    pub write_5m: f64,
    pub write_1h: f64,
}

impl Prices {
    /// Anthropic's list prices for `model`, checked 2026-11-02. An unknown
    /// model is priced as Opus 5 / 4.8, the most expensive of the three rows,
    /// which keeps a saving from looking larger than it is.
    pub fn for_model(model: &str) -> Prices {
        let model = model.strip_prefix("anthropic/").unwrap_or(model);
        if model.starts_with("claude-opus-5-5") {
            Prices {
                fresh: 4.0,
                read: 0.20,
                write_5m: 5.0,
                write_1h: 8.0,
            }
        } else if model.starts_with("claude-sonnet-5-5") {
            Prices {
                fresh: 2.0,
                read: 0.10,
                write_5m: 2.5,
                write_1h: 4.0,
            }
        } else {
            Prices {
                fresh: 5.0,
                read: 0.50,
                write_5m: 6.25,
                write_1h: 10.0,
            }
        }
    }

    /// Input-side dollars for `usage`.
    pub fn input_cost(&self, usage: &Usage) -> f64 {
        (usage.fresh as f64 * self.fresh
            + usage.read as f64 * self.read
            + usage.write_5m as f64 * self.write_5m
            + usage.write_1h as f64 * self.write_1h)
            / 1e6
    }
}

/// Usage summed over many requests, with what it cost.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ledger {
    pub requests: u64,
    pub usage: Usage,
    pub dollars: f64,
}

impl Ledger {
    pub fn add(&mut self, model: &str, usage: &Usage) {
        self.requests += 1;
        self.usage.fresh += usage.fresh;
        self.usage.read += usage.read;
        self.usage.write_5m += usage.write_5m;
        self.usage.write_1h += usage.write_1h;
        self.dollars += Prices::for_model(model).input_cost(usage);
    }

    /// Cache reads as a share of the billed prompt.
    pub fn read_share(&self) -> f64 {
        self.usage.read as f64 / self.usage.prompt().max(1) as f64
    }

    pub fn row(&self, name: &str) -> String {
        format!(
            "{name:<10} {:>5} req  prompt {:>10}  fresh {:>9}  read {:>10}  write5m {:>9}  write1h {:>9}  read share {:>6.2}%  input ${:.4}",
            self.requests,
            self.usage.prompt(),
            self.usage.fresh,
            self.usage.read,
            self.usage.write_5m,
            self.usage.write_1h,
            self.read_share() * 100.0,
            self.dollars,
        )
    }
}

#[derive(Default)]
struct State {
    /// Prefix hash → when the entry expires, on the simulated clock.
    entries: HashMap<u64, u64>,
    now_ms: u64,
    /// Every request answered, in order, with its model.
    log: Vec<(String, Usage)>,
}

/// The simulated provider. Clones share one cache.
#[derive(Clone, Default)]
pub struct CacheSim {
    state: Arc<Mutex<State>>,
}

impl CacheSim {
    pub fn new() -> CacheSim {
        CacheSim::default()
    }

    /// Move the simulated clock forward.
    pub fn advance_ms(&self, ms: u64) {
        self.state.lock().unwrap().now_ms += ms;
    }

    /// Set the simulated clock, never backwards.
    pub fn at_ms(&self, ms: u64) {
        let mut state = self.state.lock().unwrap();
        state.now_ms = state.now_ms.max(ms);
    }

    /// Every request answered so far, in order.
    pub fn log(&self) -> Vec<(String, Usage)> {
        self.state.lock().unwrap().log.clone()
    }

    pub fn ledger(&self) -> Ledger {
        let mut ledger = Ledger::default();
        for (model, usage) in self.log() {
            ledger.add(&model, &usage);
        }
        ledger
    }

    /// Bill one Messages request against the cache and remember what it wrote.
    pub fn bill(&self, body: &Value) -> Usage {
        let blocks = blocks(body);
        let model = body["model"].as_str().unwrap_or_default().to_string();

        // Running hash and token count at the end of each block. The model is
        // the seed: caches are per model.
        let mut hasher = DefaultHasher::new();
        model.hash(&mut hasher);
        let mut seed = hasher.finish();
        let mut hashes = Vec::with_capacity(blocks.len());
        let mut cumulative = Vec::with_capacity(blocks.len());
        let mut total = 0u64;
        for block in &blocks {
            let mut hasher = DefaultHasher::new();
            seed.hash(&mut hasher);
            block.text.hash(&mut hasher);
            seed = hasher.finish();
            hashes.push(seed);
            total += tokens(&block.text);
            cumulative.push(total);
        }
        let breakpoints: Vec<(usize, u64)> = blocks
            .iter()
            .enumerate()
            .filter_map(|(i, block)| block.ttl_ms.map(|ttl| (i, ttl)))
            .collect();

        let mut state = self.state.lock().unwrap();
        let now = state.now_ms;
        state.entries.retain(|_, expires| *expires > now);

        // The longest live entry reachable from any breakpoint.
        let mut hit: Option<usize> = None;
        for &(at, _) in &breakpoints {
            let lowest = at.saturating_sub(LOOKBACK - 1);
            if let Some(found) = (lowest..=at)
                .rev()
                .find(|&i| state.entries.contains_key(&hashes[i]))
            {
                hit = hit.max(Some(found));
            }
        }
        let read = hit.map_or(0, |i| cumulative[i]);
        if let Some(i) = hit {
            // A hit renews the entry it read, at the lifetime it was written with.
            let lifetime = breakpoints
                .iter()
                .find(|&&(at, _)| at >= i)
                .map_or(FIVE_MINUTES_MS, |&(_, ttl)| ttl);
            state.entries.insert(hashes[i], now + lifetime);
        }

        let mut usage = Usage {
            read,
            ..Usage::default()
        };
        let mut written_to = read;
        for &(at, ttl) in &breakpoints {
            let end = cumulative[at];
            if end <= written_to || end < MIN_CACHEABLE {
                continue;
            }
            let span = end - written_to;
            if ttl == ONE_HOUR_MS {
                usage.write_1h += span;
            } else {
                usage.write_5m += span;
            }
            state.entries.insert(hashes[at], now + ttl);
            written_to = end;
        }
        usage.fresh = total - usage.read - usage.write_5m - usage.write_1h;
        usage.frontier = breakpoints
            .last()
            .map_or(0, |&(at, _)| cumulative[at])
            .max(read);
        state.log.push((model, usage));
        usage
    }
}

impl Respond for CacheSim {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let usage = self.bill(&body);
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_sim",
            "type": "message",
            "role": "assistant",
            "model": body["model"],
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": usage.fresh,
                "cache_read_input_tokens": usage.read,
                "cache_creation_input_tokens": usage.write_5m + usage.write_1h,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": usage.write_5m,
                    "ephemeral_1h_input_tokens": usage.write_1h
                },
                "output_tokens": OUTPUT_TOKENS
            }
        }))
    }
}

/// One cacheable unit of the prompt.
struct Block {
    /// Canonical JSON, `cache_control` removed, with its role for messages.
    text: String,
    /// The lifetime its `cache_control` asks for, when it is a breakpoint.
    ttl_ms: Option<u64>,
}

fn tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

fn block(role: &str, value: &Value) -> Block {
    let mut value = value.clone();
    let ttl_ms = value.as_object_mut().and_then(|object| {
        object.remove("cache_control").map(|control| {
            if control["ttl"] == "1h" {
                ONE_HOUR_MS
            } else {
                FIVE_MINUTES_MS
            }
        })
    });
    Block {
        text: format!("{role}:{value}"),
        ttl_ms,
    }
}

fn blocks(body: &Value) -> Vec<Block> {
    let mut out = Vec::new();
    for tool in body["tools"].as_array().into_iter().flatten() {
        out.push(block("tool", tool));
    }
    match &body["system"] {
        Value::String(text) => out.push(block("system", &json!({"type": "text", "text": text}))),
        Value::Array(parts) => out.extend(parts.iter().map(|part| block("system", part))),
        _ => {}
    }
    for message in body["messages"].as_array().into_iter().flatten() {
        let role = message["role"].as_str().unwrap_or_default();
        match &message["content"] {
            Value::String(text) => out.push(block(role, &json!({"type": "text", "text": text}))),
            Value::Array(parts) => out.extend(parts.iter().map(|part| block(role, part))),
            _ => {}
        }
    }
    out
}
