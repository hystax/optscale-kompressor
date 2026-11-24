//! Reporting endpoints.
//!
//! Every one of them is scoped to the caller's own tenant. The tenant is the
//! scoping unit for everything durable, and there is deliberately no query that
//! reads across it: a key holder cannot learn what another team compressed by
//! asking nicely, and only the cluster secret — which is how a trusted
//! component in the same deployment acts on a tenant's behalf — may name one.

use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use kompressor_core::Identity;
use kompressor_stats::reader::Bucket;
use serde::Deserialize;

use crate::api::authenticate;
use crate::error::ApiError;
use crate::middleware::json_response;
use crate::state::ServerState;

/// Query parameters shared by the reporting endpoints.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StatsQuery {
    /// Only a trusted caller may name a tenant other than its own.
    tenant: Option<String>,
    /// Window start, milliseconds since the Unix epoch. Inclusive.
    from: Option<i64>,
    /// Window end, milliseconds since the Unix epoch. Exclusive.
    to: Option<i64>,
    /// `1h`/`hour` or `1d`/`day`, for the histogram.
    bucket: Option<String>,
    /// The request to report on, for `/stats/requests`.
    request_id: Option<String>,
}

impl StatsQuery {
    /// The window, defaulting to all of recorded history.
    ///
    /// A default window of "the last day" would quietly make an empty report
    /// out of a database full of older rows, which reads as "the compressor did
    /// nothing" rather than as "you asked about today".
    fn window(&self) -> (i64, i64) {
        (self.from.unwrap_or(0), self.to.unwrap_or(i64::MAX))
    }

    fn bucket(&self) -> Result<Bucket, ApiError> {
        match self.bucket.as_deref() {
            None | Some("1h") | Some("hour") | Some("h") => Ok(Bucket::Hour),
            Some("1d") | Some("day") | Some("d") => Ok(Bucket::Day),
            Some(other) => Err(ApiError::Invalid(format!(
                "bucket must be 1h or 1d, not {other}"
            ))),
        }
    }
}

/// The reporting routes, mounted under each version prefix.
pub(crate) fn stats_routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/stats/summary", get(summary))
        .route("/stats/histogram", get(histogram))
        .route("/stats/by_type", get(by_type))
        .route("/stats/retrievals", get(retrievals))
        .route("/stats/requests", get(requests))
        .route("/stats/store", get(store))
        .merge(crate::live::live_routes())
}

/// Unpack the query, refusing a parameter we do not serve rather than ignoring
/// it: a report filtered by a parameter that did nothing is worse than no
/// report, because it looks like an answer.
fn unpack(query: Result<Query<StatsQuery>, QueryRejection>) -> Result<StatsQuery, ApiError> {
    query
        .map(|Query(query)| query)
        .map_err(|rejection| ApiError::Invalid(format!("query: {}", rejection.body_text())))
}

/// Which tenant this report is about.
fn scope(identity: &Identity, requested: Option<&str>) -> Result<String, ApiError> {
    match requested {
        None => Ok(identity.tenant.as_str().to_string()),
        Some(name) if name == identity.tenant.as_str() => Ok(name.to_string()),
        Some(name) if identity.trusted => Ok(name.to_string()),
        Some(_) => Err(ApiError::Forbidden),
    }
}

async fn summary(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<StatsQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let identity = authenticate(&state, &headers)?;
    let query = unpack(query)?;
    let tenant = scope(&identity, query.tenant.as_deref())?;
    let (from, to) = query.window();

    let summary = state
        .with_stats(move |reader| reader.summary(&tenant, from, to))
        .await?;
    Ok(json_response(&summary))
}

async fn histogram(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<StatsQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let identity = authenticate(&state, &headers)?;
    let query = unpack(query)?;
    let tenant = scope(&identity, query.tenant.as_deref())?;
    let bucket = query.bucket()?;
    let (from, to) = query.window();

    let rows = state
        .with_stats(move |reader| reader.histogram(&tenant, from, to, bucket))
        .await?;
    Ok(json_response(&serde_json::json!({
        "bucket": bucket,
        "rows": rows,
    })))
}

async fn by_type(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<StatsQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let identity = authenticate(&state, &headers)?;
    let query = unpack(query)?;
    let tenant = scope(&identity, query.tenant.as_deref())?;
    let (from, to) = query.window();

    let types = state
        .with_stats(move |reader| reader.by_type(&tenant, from, to))
        .await?;
    Ok(json_response(&serde_json::json!({ "types": types })))
}

async fn retrievals(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<StatsQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let identity = authenticate(&state, &headers)?;
    let query = unpack(query)?;
    let tenant = scope(&identity, query.tenant.as_deref())?;
    let (from, to) = query.window();

    let summary = state
        .with_stats(move |reader| reader.retrievals(&tenant, from, to))
        .await?;
    Ok(json_response(&summary))
}

async fn requests(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<StatsQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let identity = authenticate(&state, &headers)?;
    let query = unpack(query)?;
    let tenant = scope(&identity, query.tenant.as_deref())?;
    let request_id = query
        .request_id
        .clone()
        .ok_or_else(|| ApiError::Invalid("request_id is required".to_string()))?;

    let economy = state
        .with_stats(move |reader| reader.request(&tenant, &request_id))
        .await?;
    match economy {
        Some(economy) => Ok(json_response(&economy)),
        None => Err(ApiError::NotFound(
            "no events recorded for that request".to_string(),
        )),
    }
}

async fn store(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<StatsQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let identity = authenticate(&state, &headers)?;
    let query = unpack(query)?;
    let tenant = scope(&identity, query.tenant.as_deref())?;

    // Two readings, because they answer different questions: the store says
    // what is held right now, the sample says what was held when the sweep last
    // ran, and a sample far behind the clock is how an operator notices the
    // sweep has stopped.
    // Scoped to the caller's tenant. The whole-store total is a
    // deployment-level diagnostic and reporting it here told every tenant how
    // much every other tenant was holding, and when the oldest of their rows
    // was created.
    let core = state.core.clone();
    let scoped = kompressor_store::types::Tenant::new(tenant.clone());
    let live = tokio::task::spawn_blocking(move || core.refs().stats_for(&scoped))
        .await
        .map_err(|error| ApiError::Internal(format!("store task: {error}")))?
        .map_err(|error| ApiError::Internal(format!("store: {error}")))?;

    let sample = state
        .with_stats(move |reader| reader.store(&tenant))
        .await
        .ok()
        .flatten();

    Ok(json_response(&serde_json::json!({
        "live_refs": live.live_refs,
        "live_bytes": live.live_bytes,
        "oldest_created_at": live.oldest_created_at,
        "last_sample": sample.map(|sample| serde_json::json!({
            "time_ms": sample.time_ms,
            "live_refs": sample.live_refs,
            "live_bytes": sample.live_bytes,
        })),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kompressor_store::types::Tenant;

    fn identity(tenant: &str, trusted: bool) -> Identity {
        Identity {
            tenant: Tenant::new(tenant),
            mode: None,
            trusted,
        }
    }

    #[test]
    fn a_report_defaults_to_the_callers_own_tenant() {
        assert_eq!(scope(&identity("team-a", false), None).unwrap(), "team-a");
    }

    #[test]
    fn a_key_holder_cannot_report_on_another_tenant() {
        let refused = scope(&identity("team-a", false), Some("team-b")).unwrap_err();
        assert_eq!(refused.status(), axum::http::StatusCode::FORBIDDEN);
        // naming its own is harmless and keeps a dashboard's URL honest
        assert_eq!(
            scope(&identity("team-a", false), Some("team-a")).unwrap(),
            "team-a"
        );
    }

    #[test]
    fn only_the_cluster_secret_may_name_a_tenant() {
        assert_eq!(
            scope(&identity("", true), Some("team-b")).unwrap(),
            "team-b"
        );
    }

    #[test]
    fn the_bucket_parameter_accepts_both_spellings_and_refuses_the_rest() {
        let hour = StatsQuery {
            bucket: Some("1h".into()),
            ..Default::default()
        };
        assert_eq!(hour.bucket().unwrap(), Bucket::Hour);
        let day = StatsQuery {
            bucket: Some("day".into()),
            ..Default::default()
        };
        assert_eq!(day.bucket().unwrap(), Bucket::Day);
        let weekly = StatsQuery {
            bucket: Some("1w".into()),
            ..Default::default()
        };
        assert_eq!(
            weekly.bucket().unwrap_err().status(),
            axum::http::StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[test]
    fn an_unbounded_window_is_the_default() {
        assert_eq!(StatsQuery::default().window(), (0, i64::MAX));
    }
}
