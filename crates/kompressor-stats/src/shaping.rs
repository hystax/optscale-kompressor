//! `proxy.output_shaping`, treated against holdout.
//!
//! The experiment is judged on two numbers taken over one window (HEADROOM-PLAN
//! Tier 7b): output tokens per client turn, which it exists to lower, and client
//! turns per conversation, which must not rise — a shorter answer that costs an
//! extra turn is a loss. The cache figures ride along so a prefix the block
//! moved would show as a difference between the arms.
//!
//! A client turn is a proxied request the client sent: keep-warm pings are the
//! proxy's own and are left out, and so are rows outside the experiment.
use crate::{StatsError, StatsReader};
use rusqlite::params;
use serde::{Deserialize, Serialize};

/// One arm over the window.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ArmFigures {
    /// `treated` or `holdout`.
    pub arm: String,
    pub client_turns: u64,
    pub conversations: u64,
    /// Turns that reported usage, which the per-turn mean is taken over.
    pub usage_turns: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens_per_turn: Option<f64>,
    pub turns_per_conversation: Option<f64>,
}

/// Both arms, and treated relative to holdout.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OutputShapingReport {
    pub arms: Vec<ArmFigures>,
    /// `treated / holdout - 1` for output tokens per turn; negative is the
    /// experiment working.
    pub output_per_turn_change: Option<f64>,
    /// The same for turns per conversation; positive is the experiment losing.
    pub turns_per_conversation_change: Option<f64>,
}

fn relative(treated: Option<f64>, holdout: Option<f64>) -> Option<f64> {
    let (treated, holdout) = (treated?, holdout?);
    (holdout != 0.0).then(|| treated / holdout - 1.0)
}

impl StatsReader {
    /// The two arms of `proxy.output_shaping` over `from..to`.
    ///
    /// Empty on a database that predates the column (schema v19), which then
    /// has no experiment rows to report.
    pub fn output_shaping_arms(
        &self,
        tenant: &str,
        from: i64,
        to: i64,
    ) -> Result<OutputShapingReport, StatsError> {
        let has_column: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('proxy_requests') WHERE name = 'output_shaping'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        if has_column == 0 {
            return Ok(OutputShapingReport::default());
        }
        let mut stmt = self.conn.prepare(
            "SELECT output_shaping, COUNT(*), COUNT(DISTINCT conversation),
                    SUM(completion_tokens IS NOT NULL),
                    coalesce(SUM(completion_tokens), 0),
                    coalesce(SUM(cache_read_tokens), 0),
                    coalesce(SUM(cache_write_tokens), 0)
               FROM proxy_requests
              WHERE tenant = ?1 AND time_ms >= ?2 AND time_ms < ?3
                AND keep_warm = 0 AND output_shaping IS NOT NULL
              GROUP BY output_shaping ORDER BY output_shaping DESC",
        )?;
        let arms = stmt
            .query_map(params![tenant, from, to], |row| {
                let client_turns = row.get::<_, i64>(1)? as u64;
                let conversations = row.get::<_, i64>(2)? as u64;
                let usage_turns = row.get::<_, i64>(3)? as u64;
                let output_tokens = row.get::<_, i64>(4)? as u64;
                Ok(ArmFigures {
                    arm: row.get(0)?,
                    client_turns,
                    conversations,
                    usage_turns,
                    output_tokens,
                    cache_read_tokens: row.get::<_, i64>(5)? as u64,
                    cache_write_tokens: row.get::<_, i64>(6)? as u64,
                    output_tokens_per_turn: (usage_turns > 0)
                        .then(|| output_tokens as f64 / usage_turns as f64),
                    turns_per_conversation: (conversations > 0)
                        .then(|| client_turns as f64 / conversations as f64),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let find = |name: &str| arms.iter().find(|arm| arm.arm == name);
        let (treated, holdout) = (find("treated"), find("holdout"));
        Ok(OutputShapingReport {
            output_per_turn_change: relative(
                treated.and_then(|arm| arm.output_tokens_per_turn),
                holdout.and_then(|arm| arm.output_tokens_per_turn),
            ),
            turns_per_conversation_change: relative(
                treated.and_then(|arm| arm.turns_per_conversation),
                holdout.and_then(|arm| arm.turns_per_conversation),
            ),
            arms,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{Event, ProxyRequestEvent, StatsConfig, StatsReader, StatsWriter};

    const T: i64 = 1_700_006_400_000;

    fn turn(at: i64, conversation: &str, arm: Option<&str>, output: u32, ping: bool) -> Event {
        Event::ProxyRequest(ProxyRequestEvent {
            prefix: None,
            context: None,
            timing: None,
            time_ms: at,
            tenant: "acme".into(),
            request_id: format!("r{at}"),
            api_family: "anthropic_messages".into(),
            model: Some("claude-opus-5-5".into()),
            upstream: "https://api.anthropic.com".into(),
            status: Some(200),
            stream: true,
            strip_usage: false,
            prompt_tokens: Some(10),
            completion_tokens: Some(output),
            cache_read_tokens: Some(1_000),
            cache_write_tokens: Some(50),
            cache_write_1h_tokens: Some(0),
            shadow_prompt_tokens: None,
            conversation: Some(conversation.into()),
            retrieve_rounds: 0,
            retrieve_tokens_injected: 0,
            upstream_ms: None,
            total_ms: None,
            error: None,
            keep_warm: ping,
            output_shaping: arm.map(str::to_string),
        })
    }

    #[test]
    fn each_arm_is_reported_per_turn_and_per_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.db");
        let mut writer = StatsWriter::open(&StatsConfig::at(&path)).unwrap();
        writer
            .write_batch(&[
                // treated: two conversations, three turns, 300 output tokens
                turn(T, "000000000000000a", Some("treated"), 100, false),
                turn(T + 1, "000000000000000a", Some("treated"), 80, false),
                turn(T + 2, "000000000000000b", Some("treated"), 120, false),
                // a ping in the treated arm is not a client turn
                turn(T + 3, "000000000000000a", Some("treated"), 1, true),
                // holdout: one conversation, two turns, 400 output tokens
                turn(T + 4, "000000000000000c", Some("holdout"), 150, false),
                turn(T + 5, "000000000000000c", Some("holdout"), 250, false),
                // outside the experiment, and outside the window
                turn(T + 6, "000000000000000d", None, 999, false),
                turn(T - 1, "000000000000000a", Some("treated"), 999, false),
            ])
            .unwrap();
        let reader = StatsReader::open(&path).unwrap();
        let report = reader.output_shaping_arms("acme", T, T + 100).unwrap();
        assert_eq!(report.arms.len(), 2);
        let treated = &report.arms[0];
        assert_eq!(treated.arm, "treated");
        assert_eq!(
            (
                treated.client_turns,
                treated.conversations,
                treated.output_tokens
            ),
            (3, 2, 300)
        );
        assert_eq!(treated.output_tokens_per_turn, Some(100.0));
        assert_eq!(treated.turns_per_conversation, Some(1.5));
        let holdout = &report.arms[1];
        assert_eq!(holdout.arm, "holdout");
        assert_eq!(holdout.output_tokens_per_turn, Some(200.0));
        assert_eq!(holdout.turns_per_conversation, Some(2.0));
        assert_eq!(report.output_per_turn_change, Some(-0.5));
        assert_eq!(report.turns_per_conversation_change, Some(-0.25));
        assert!(
            reader
                .output_shaping_arms("other", T, T + 100)
                .unwrap()
                .arms
                .is_empty()
        );
    }
}
