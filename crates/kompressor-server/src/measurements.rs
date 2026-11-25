//! The three measurements the headroom plan's later tiers are gated on, each
//! returned with the SQL that produced it (`kompressor_stats::measured`):
//!
//! - `/v1/stats/restarts` — process starts against the conversations open
//!   across them, and what the first turn after each read from cache;
//! - `/v1/stats/retrieve-rounds` — what the retrieve loop's extra rounds were
//!   billed, measured per round;
//! - `/v1/stats/fetch-probability` — the share of elided handles asked for
//!   back, by kind, fragment size and prompt size.
//!
//! JSON only. Credentials and tenant scoping are the other reports': the
//! caller's own tenant unless a trusted caller names another.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use serde::Deserialize;

use crate::api::authenticate;
use crate::error::ApiError;
use crate::live::parse_window;
use crate::middleware::json_response;
use crate::state::ServerState;

/// The window when the caller names none: the plan's baseline window.
pub const DEFAULT_WINDOW: &str = "7d";

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct MeasureQuery {
    tenant: Option<String>,
    /// Trailing window ending now: `24h`, `7d`, at most `90d`.
    window: Option<String>,
    /// A fixed window instead, epoch milliseconds, `[from, to)`.
    from: Option<i64>,
    to: Option<i64>,
    /// Only requests recorded under this `stats.experiment_id`.
    experiment_id: Option<String>,
}

impl MeasureQuery {
    fn range(&self, now_ms: i64) -> Result<(i64, i64), ApiError> {
        match (self.from, self.to) {
            (None, None) => {
                let label = self.window.as_deref().unwrap_or(DEFAULT_WINDOW);
                let length: Duration = parse_window(label)?;
                Ok((
                    now_ms.saturating_sub(length.as_millis() as i64),
                    now_ms.saturating_add(1),
                ))
            }
            (Some(from), Some(to)) if from >= 0 && to > from => Ok((from, to)),
            _ => Err(ApiError::Invalid(
                "from/to must both be epoch milliseconds with 0 <= from < to".to_string(),
            )),
        }
    }

    fn experiment(&self) -> Option<String> {
        self.experiment_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
    }
}

#[derive(Clone, Copy)]
enum Measure {
    Restarts,
    RetrieveRounds,
    FetchProbability,
}

/// The routes, mounted under each version prefix beside the other reports.
pub(crate) fn measurement_routes() -> Router<Arc<ServerState>> {
    let mut router = Router::new();
    for (path, measure) in [
        ("/stats/restarts", Measure::Restarts),
        ("/stats/retrieve-rounds", Measure::RetrieveRounds),
        ("/stats/fetch-probability", Measure::FetchProbability),
    ] {
        router = router.route(
            path,
            get(
                move |state: State<Arc<ServerState>>,
                      headers: HeaderMap,
                      query: Result<Query<MeasureQuery>, QueryRejection>| async move {
                    respond(measure, state.0, headers, query).await
                },
            ),
        );
    }
    router
}

async fn respond(
    measure: Measure,
    state: Arc<ServerState>,
    headers: HeaderMap,
    query: Result<Query<MeasureQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let identity = authenticate(&state, &headers)?;
    let query = query
        .map(|Query(query)| query)
        .map_err(|rejection| ApiError::Invalid(format!("query: {}", rejection.body_text())))?;
    let tenant = crate::stats_api::scope(&identity, query.tenant.as_deref())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let (from, to) = query.range(now)?;
    let experiment = query.experiment();
    let report = state
        .with_stats(move |reader| {
            let experiment = experiment.as_deref();
            Ok(match measure {
                Measure::Restarts => {
                    serde_json::to_value(reader.restarts(&tenant, from, to, experiment)?)
                }
                Measure::RetrieveRounds => {
                    serde_json::to_value(reader.retrieve_round_cost(&tenant, from, to, experiment)?)
                }
                Measure::FetchProbability => {
                    serde_json::to_value(reader.fetch_probability(&tenant, from, to, experiment)?)
                }
            })
        })
        .await?
        .map_err(|error| ApiError::Internal(format!("report: {error}")))?;
    Ok(json_response(&report))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_defaults_to_a_week_and_a_fixed_range_must_be_whole() {
        let now = 10 * 86_400_000;
        let (from, to) = MeasureQuery::default().range(now).unwrap();
        assert_eq!((from, to), (now - 7 * 86_400_000, now + 1));
        let fixed = MeasureQuery {
            from: Some(5),
            to: Some(9),
            ..MeasureQuery::default()
        };
        assert_eq!(fixed.range(now).unwrap(), (5, 9));
        let half = MeasureQuery {
            from: Some(5),
            ..MeasureQuery::default()
        };
        assert!(half.range(now).is_err());
        let blank = MeasureQuery {
            experiment_id: Some("  ".into()),
            ..MeasureQuery::default()
        };
        assert_eq!(blank.experiment(), None);
    }
}
