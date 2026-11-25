//! The net-cost elision gate: whether a handle-bearing compression is worth
//! the fetch it invites, priced against the conversation it arrives in.
//!
//! `min_elision_saving_chars` asks one fixed question of every payload — did
//! it save 4,000 characters — and the answer was fitted on benchmark runs at
//! $0.03 a retrieve round. A round re-reads the whole prompt, so what it costs
//! scales with the conversation: the same 3,000-character saving that loses
//! money at a 300K-token prefix pays for itself at a 20K one. This gate prices
//! both sides:
//!
//! ```text
//! gain = ΔT·(w + r·(R − 1)) − p_fetch·(r·prefix_tokens + output_cost)
//! ```
//!
//! `ΔT` is the tokens the compression removed; `w` what the provider bills to
//! write a token into its cache, `r` to read one back, both as multiples of the
//! uncached input price; `R` how many requests are expected to carry the
//! message; `p_fetch` how likely a payload of this kind and size is to be
//! fetched back; and the bracket what one retrieve round costs — the prompt
//! re-read from cache plus the model's output for the call. A payload ships
//! whole when the gain is not positive.
//!
//! **The decision must be taken once.** `prefix_tokens`, and the two prices
//! with it, move every turn, and a decision that followed them would compress a
//! message on turn five and ship it whole on turn twenty — a rewrite of bytes
//! the provider has cached, which is exactly what the gate exists to avoid
//! paying for. So the inputs that move are a [`GatePin`], taken when a message
//! is first compressed and replayed for it on every later turn by
//! [`crate::messages::compress_messages`]. Given its pin, a message's bytes are
//! a function of the message and the history ahead of it, as they always were.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::types::Kind;

/// The inputs to the gate that change from turn to turn, as they stood when a
/// message was first compressed.
///
/// Prices are in millionths of the uncached input price, the unit the proxy's
/// own price table already uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatePin {
    /// The whole billed prompt of the conversation's last answered request.
    pub prefix_tokens: u32,
    /// A cache write, `w`.
    pub write_ppm: u32,
    /// A cache read, `r`.
    pub read_ppm: u32,
}

/// One row of the fetch-probability table: payloads of `kind` (`"*"` for any)
/// whose compression saved fewer than `below_saved_chars` characters (absent
/// for no bound) are fetched back with probability `p`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchRow {
    #[serde(default = "any_kind")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub below_saved_chars: Option<usize>,
    pub p: f64,
}

fn any_kind() -> String {
    "*".to_string()
}

/// The parts of the gate that are configuration: the same for every message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetCostModel {
    /// `R`: how many requests are expected to carry a message once it has
    /// arrived, its own included. **An assumption, not a measurement**: 10 is
    /// a placeholder until the `/cohorts` page's turns-remaining figure is
    /// read into it. The larger it is, the more a removed token is worth and
    /// the looser the gate.
    pub expected_reads: f64,
    /// What one retrieve round's output costs, in tokens at the uncached input
    /// price: the model's call to the tool, priced at the output rate. **An
    /// assumption**: 500 is a ~100-token call at a 5x output price.
    pub output_cost_tokens: f64,
    /// `p_fetch` for a payload no row of [`Self::fetch_table`] matches.
    ///
    /// 0.30 is the one measured figure: on Claude Opus 5.5 across ten
    /// recoverable Harbor runs, tool results that saved under 4,000 characters
    /// were fetched back 30% of the time (`min_elision_saving_chars`). Nothing
    /// above that size was measured separately, so the same figure stands for
    /// it until a table replaces it.
    pub fetch_probability: f64,
    /// `p_fetch` by kind and size, first match wins. Empty by default; the
    /// measured table belongs here, by kind as the `/yield` page reports it.
    pub fetch_table: Vec<FetchRow>,
}

impl Default for NetCostModel {
    fn default() -> Self {
        NetCostModel {
            expected_reads: 10.0,
            output_cost_tokens: 500.0,
            fetch_probability: 0.30,
            fetch_table: Vec::new(),
        }
    }
}

impl NetCostModel {
    /// Refuse a model whose arithmetic would mean nothing.
    pub fn validate(&self) -> Result<(), String> {
        let probability = |p: f64| p.is_finite() && (0.0..=1.0).contains(&p);
        if !(self.expected_reads.is_finite() && self.expected_reads >= 1.0) {
            return Err("expected_reads must be at least 1 (the message's own request)".into());
        }
        if !(self.output_cost_tokens.is_finite() && self.output_cost_tokens >= 0.0) {
            return Err("output_cost_tokens must be zero or more".into());
        }
        if !probability(self.fetch_probability) {
            return Err("fetch_probability must be between 0 and 1".into());
        }
        if let Some(row) = self.fetch_table.iter().find(|row| !probability(row.p)) {
            return Err(format!(
                "fetch_table row for {:?} has p = {}, which is not between 0 and 1",
                row.kind, row.p
            ));
        }
        Ok(())
    }

    /// `p_fetch` for a payload of `kind` whose compression saved `saved_chars`.
    pub fn fetch_probability_for(&self, kind: Option<Kind>, saved_chars: usize) -> f64 {
        let name = kind.map(Kind::as_str).unwrap_or("");
        self.fetch_table
            .iter()
            .find(|row| {
                (row.kind == "*" || row.kind == name)
                    && row
                        .below_saved_chars
                        .is_none_or(|bound| saved_chars < bound)
            })
            .map_or(self.fetch_probability, |row| row.p)
    }

    /// What eliding is worth net of the fetch it invites, in tokens at the
    /// uncached input price. Positive means elide.
    pub fn gain(
        &self,
        pin: GatePin,
        saved_tokens: u32,
        kind: Option<Kind>,
        saved_chars: usize,
    ) -> f64 {
        let w = f64::from(pin.write_ppm) / 1_000_000.0;
        let r = f64::from(pin.read_ppm) / 1_000_000.0;
        let value = f64::from(saved_tokens) * (w + r * (self.expected_reads - 1.0));
        let round = r * f64::from(pin.prefix_tokens) + self.output_cost_tokens;
        value - self.fetch_probability_for(kind, saved_chars) * round
    }
}

/// The gate as one request carries it, on [`crate::EngineOptions::elision_gate`].
#[derive(Clone, Debug)]
pub struct ElisionGate {
    pub model: Arc<NetCostModel>,
    /// The inputs as they stand for this request: what a message compressed
    /// for the first time is decided against, and then pinned to.
    pub current: GatePin,
    /// Pins taken on earlier requests, by [`crate::messages::read_key`].
    /// Consulted only by `compress_messages`; a payload compressed on its own
    /// is decided against [`Self::current`].
    pub pinned: Arc<BTreeMap<u64, GatePin>>,
}

impl ElisionGate {
    pub fn new(model: Arc<NetCostModel>, current: GatePin) -> Self {
        ElisionGate {
            model,
            current,
            pinned: Arc::default(),
        }
    }

    /// Whether a compression that saved `saved_tokens` (`saved_chars`) and
    /// leaves a handle is worth keeping.
    pub fn admits(&self, saved_tokens: u32, kind: Option<Kind>, saved_chars: usize) -> bool {
        self.model
            .gain(self.current, saved_tokens, kind, saved_chars)
            > 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opus(prefix_tokens: u32) -> GatePin {
        GatePin {
            prefix_tokens,
            write_ppm: 1_250_000,
            read_ppm: 50_000,
        }
    }

    #[test]
    fn the_gain_is_the_formula() {
        let model = NetCostModel::default();
        let pin = GatePin {
            prefix_tokens: 100_000,
            write_ppm: 1_250_000,
            read_ppm: 100_000,
        };
        // 1,000·(1.25 + 0.1·9) − 0.3·(0.1·100,000 + 500) = 2,150 − 3,150.
        let gain = model.gain(pin, 1_000, Some(Kind::Log), 4_000);
        assert!((gain - (-1_000.0)).abs() < 1e-6, "{gain}");
        let gain = model.gain(pin, 2_000, Some(Kind::Log), 8_000);
        assert!((gain - 1_150.0).abs() < 1e-6, "{gain}");
    }

    #[test]
    fn the_gate_loosens_on_a_short_conversation_and_tightens_on_a_long_one() {
        let model = Arc::new(NetCostModel::default());
        // ~3,000 characters saved, the band the fixed gate refuses outright.
        let saved = (750, 3_000);
        let short = ElisionGate::new(model.clone(), opus(20_000));
        let long = ElisionGate::new(model, opus(300_000));
        assert!(short.admits(saved.0, Some(Kind::Log), saved.1));
        assert!(!long.admits(saved.0, Some(Kind::Log), saved.1));
    }

    #[test]
    fn the_table_matches_by_kind_and_size_and_falls_back_to_the_default() {
        let model = NetCostModel {
            fetch_table: vec![
                FetchRow {
                    kind: "source_code".into(),
                    below_saved_chars: None,
                    p: 0.6,
                },
                FetchRow {
                    kind: "*".into(),
                    below_saved_chars: Some(4_000),
                    p: 0.3,
                },
            ],
            fetch_probability: 0.1,
            ..NetCostModel::default()
        };
        assert_eq!(
            model.fetch_probability_for(Some(Kind::SourceCode), 50_000),
            0.6
        );
        assert_eq!(model.fetch_probability_for(Some(Kind::Log), 3_999), 0.3);
        assert_eq!(model.fetch_probability_for(Some(Kind::Log), 4_000), 0.1);
        assert_eq!(model.fetch_probability_for(None, 10), 0.3);
    }

    #[test]
    fn a_model_that_means_nothing_is_refused() {
        assert!(NetCostModel::default().validate().is_ok());
        for broken in [
            NetCostModel {
                expected_reads: 0.5,
                ..Default::default()
            },
            NetCostModel {
                fetch_probability: 1.5,
                ..Default::default()
            },
            NetCostModel {
                output_cost_tokens: f64::NAN,
                ..Default::default()
            },
            NetCostModel {
                fetch_table: vec![FetchRow {
                    kind: "*".into(),
                    below_saved_chars: None,
                    p: -0.1,
                }],
                ..Default::default()
            },
        ] {
            assert!(broken.validate().is_err(), "{broken:?}");
        }
    }

    #[test]
    fn a_model_reads_with_any_kind_by_default() {
        let model: NetCostModel = from_json(
            r#"{"expected_reads": 4, "fetch_table": [{"below_saved_chars": 4000, "p": 0.3}]}"#,
        );
        assert_eq!(model.expected_reads, 4.0);
        assert_eq!(model.fetch_table[0].kind, "*");
        assert_eq!(
            model.fetch_probability, 0.30,
            "unnamed fields keep their default"
        );
    }

    fn from_json(json: &str) -> NetCostModel {
        serde_json::from_str(json).expect("parses")
    }
}
