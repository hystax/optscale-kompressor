//! The second set of dashboards: a health overview, trends, compression
//! yield, a conversation timeline, store health, keep-warm return, cache
//! losses set against compression, upstream health, traffic composition, the
//! engine, data quality, a per-model scorecard, activity by hour of the week,
//! conversation cohorts, what moved in the cacheable prefix, and which opt-in
//! settings that evidence says would pay.
//!
//! Each view has a JSON route under each version prefix —
//! `/v1/stats/{health,trends,yield,store-health,keepwarm,cache-loss,upstream,traffic,engine,quality,models,activity,cohorts,prefix,advisor}`
//! — and one
//! shared page that draws whichever view it was opened on. The timeline has no
//! route of its own: it draws a conversation's turns from
//! `/v1/stats/analysis?conversation=`, which already carries them. On the
//! statistics listener the pages are at `/health`, `/trends`, `/yield`,
//! `/timeline`, `/cache-loss`, `/traffic`, `/quality`,
//! `/store`, `/keepwarm`, `/upstream`, `/engine`, `/models`, `/activity`,
//! `/cohorts`, `/prefix` and `/advisor`.
//!
//! Credentials, tenant scoping, formats and the page-shell rule are the cache
//! route's: the shell is served to anyone, the data only to a request that
//! presents a key, and only a trusted caller may name another tenant.
//!
//! The yield and store-health views also read the ref store's bookkeeping — a
//! census of handles, kinds, sizes and times, never fragment text — so they
//! work whether or not the ref browser (`refs.browse`) is turned on.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use kompressor_stats::advisor::AdvisorReport;
use kompressor_stats::cache::compression::CompressionCacheReport;
use kompressor_stats::cache::keepwarm::KeepWarmReport;
use kompressor_stats::dashboards::{
    EngineReport, StoreHealth, StoredRef, TrendBucket, TrendsReport, UpstreamReport, YieldReport,
};
use kompressor_stats::health::{HealthReport, Runtime};
use kompressor_stats::prefix::PrefixReport;
use kompressor_stats::quality::{QualityReport, QualityRuntime};
use kompressor_stats::traffic::TrafficReport;
use kompressor_store::{RefCensus, Tenant};
use serde::{Deserialize, Serialize};

use crate::api::authenticate;
use crate::error::ApiError;
use crate::export::jsonl_response;
use crate::live::{Format, ServerInfo, parse_window, server_info};
use crate::middleware::json_response;
use crate::state::ServerState;

/// The widest trend served. Trends read the daily archive, which outlives
/// the raw rows, so they reach further back than the other views.
pub const MAX_TREND_WINDOW: Duration = Duration::from_secs(730 * 86_400);
pub const DEFAULT_TREND_WINDOW: &str = "26w";
/// The window the other views use when the caller names none.
pub const DEFAULT_WINDOW: &str = "24h";
/// The health view looks at the last hour unless told otherwise: it answers
/// "is it fine now", not "how was the day".
pub const DEFAULT_HEALTH_WINDOW: &str = "1h";
/// Conversations the keep-warm view lists by default, and the most it will.
pub const DEFAULT_CONVERSATIONS: usize = 50;
pub const MAX_CONVERSATIONS: usize = 500;
/// How often the page polls, in seconds, unless `?refresh=` says otherwise.
pub const DEFAULT_REFRESH: u32 = 30;

/// Which dashboard a route serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum View {
    Health,
    Trends,
    Yield,
    Timeline,
    Store,
    KeepWarm,
    CacheLoss,
    Upstream,
    Traffic,
    Engine,
    Quality,
    Models,
    Activity,
    Cohorts,
    Prefix,
    Advisor,
}

impl View {
    pub(crate) const ALL: [View; 16] = [
        View::Health,
        View::Trends,
        View::Yield,
        View::Timeline,
        View::Store,
        View::KeepWarm,
        View::CacheLoss,
        View::Upstream,
        View::Traffic,
        View::Engine,
        View::Quality,
        View::Models,
        View::Activity,
        View::Cohorts,
        View::Prefix,
        View::Advisor,
    ];

    /// The page's path on the statistics listener.
    pub(crate) fn page_path(self) -> &'static str {
        match self {
            View::Health => "/health",
            View::Trends => "/trends",
            View::Yield => "/yield",
            View::Timeline => "/timeline",
            View::Store => "/store",
            View::KeepWarm => "/keepwarm",
            View::CacheLoss => "/cache-loss",
            View::Traffic => "/traffic",
            View::Quality => "/quality",
            View::Upstream => "/upstream",
            View::Engine => "/engine",
            View::Models => "/models",
            View::Activity => "/activity",
            View::Cohorts => "/cohorts",
            View::Prefix => "/prefix",
            View::Advisor => "/advisor",
        }
    }

    /// The JSON route under a version prefix.
    fn api_path(self) -> &'static str {
        match self {
            View::Health => "/stats/health",
            View::Trends => "/stats/trends",
            View::Yield => "/stats/yield",
            View::Timeline => "/stats/timeline",
            View::Store => "/stats/store-health",
            View::KeepWarm => "/stats/keepwarm",
            View::CacheLoss => "/stats/cache-loss",
            View::Traffic => "/stats/traffic",
            View::Quality => "/stats/quality",
            View::Upstream => "/stats/upstream",
            View::Engine => "/stats/engine",
            View::Models => "/stats/models",
            View::Activity => "/stats/activity",
            View::Cohorts => "/stats/cohorts",
            View::Prefix => "/stats/prefix",
            View::Advisor => "/stats/advisor",
        }
    }

    fn name(self) -> &'static str {
        match self {
            View::Health => "health",
            View::Trends => "trends",
            View::Yield => "yield",
            View::Timeline => "timeline",
            View::Store => "store",
            View::KeepWarm => "keepwarm",
            View::CacheLoss => "cache-loss",
            View::Traffic => "traffic",
            View::Quality => "quality",
            View::Upstream => "upstream",
            View::Engine => "engine",
            View::Models => "models",
            View::Activity => "activity",
            View::Cohorts => "cohorts",
            View::Prefix => "prefix",
            View::Advisor => "advisor",
        }
    }
}

/// The query every dashboard route accepts. A parameter one view has no use
/// for is accepted and ignored; one no view knows is refused.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct DashboardQuery {
    tenant: Option<String>,
    /// Trailing window: `30m`, `24h`, `7d`; for trends also `26w`.
    window: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
    /// Trends only: `day`, `week` or `month`.
    bucket: Option<String>,
    /// Keep-warm only: how many conversations to list.
    conversations: Option<usize>,
    /// Timeline only: the conversation to draw.
    conversation: Option<String>,
    format: Option<String>,
    refresh: Option<u32>,
}

impl DashboardQuery {
    fn refresh(&self) -> Result<u32, ApiError> {
        match self.refresh {
            None => Ok(DEFAULT_REFRESH),
            Some(n @ 1..=3600) => Ok(n),
            Some(n) => Err(ApiError::Invalid(format!(
                "refresh must be between 1 and 3600 seconds, not {n}"
            ))),
        }
    }

    fn conversations(&self) -> Result<usize, ApiError> {
        match self.conversations {
            None => Ok(DEFAULT_CONVERSATIONS),
            Some(n) if n <= MAX_CONVERSATIONS => Ok(n),
            Some(n) => Err(ApiError::Invalid(format!(
                "conversations must be at most {MAX_CONVERSATIONS}, not {n}"
            ))),
        }
    }

    fn bucket(&self) -> Result<TrendBucket, ApiError> {
        match self.bucket.as_deref() {
            None => Ok(TrendBucket::default()),
            Some(name) => TrendBucket::parse(name).ok_or_else(|| {
                ApiError::Invalid(format!("bucket must be day, week or month, not {name}"))
            }),
        }
    }

    /// A conversation id is sixteen lowercase hex digits, as on `/cache`.
    fn conversation(&self) -> Result<Option<String>, ApiError> {
        let Some(id) = self.conversation.as_deref().map(str::trim) else {
            return Ok(None);
        };
        if id.is_empty() {
            return Ok(None);
        }
        if id.len() == 16
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            Ok(Some(id.to_string()))
        } else {
            Err(ApiError::Invalid(format!(
                "conversation must be 16 lowercase hex digits, not {id:?}"
            )))
        }
    }

    /// The window label and its length, with the trend view's wider limit.
    fn window(&self, view: View) -> Result<(String, Duration), ApiError> {
        let default = if view == View::Trends {
            DEFAULT_TREND_WINDOW
        } else if view == View::Health {
            DEFAULT_HEALTH_WINDOW
        } else {
            DEFAULT_WINDOW
        };
        let label = self.window.clone().unwrap_or_else(|| default.to_string());
        let length = if view == View::Trends {
            parse_trend_window(&label)?
        } else {
            parse_window(&label)?
        };
        Ok((label, length))
    }

    /// A fixed `from`/`to` range, if one was named.
    fn fixed(&self, view: View) -> Result<Option<(i64, i64)>, ApiError> {
        let span = if view == View::Trends {
            MAX_TREND_WINDOW
        } else {
            Duration::from_secs(90 * 86_400)
        };
        let days = span.as_secs() / 86_400;
        match (self.from, self.to) {
            (None, None) => Ok(None),
            (Some(from), Some(to))
                if from >= 0 && to > from && (to - from) as u128 <= span.as_millis() =>
            {
                Ok(Some((from, to)))
            }
            _ => Err(ApiError::Invalid(format!(
                "from/to must both be epoch milliseconds with 0 <= from < to, spanning at most {days} days"
            ))),
        }
    }
}

/// `<n>d` or `<n>w`, up to [`MAX_TREND_WINDOW`]; anything the other views
/// accept is accepted too.
fn parse_trend_window(text: &str) -> Result<Duration, ApiError> {
    let invalid = || {
        ApiError::Invalid(format!(
            "window must look like 30d or 26w, at most 730 days, not {text}"
        ))
    };
    let trimmed = text.trim();
    let window = if let Some(weeks) = trimmed.strip_suffix('w') {
        let n: u64 = weeks.parse().map_err(|_| invalid())?;
        Duration::from_secs(n.saturating_mul(7 * 86_400))
    } else if let Some(days) = trimmed.strip_suffix('d') {
        let n: u64 = days.parse().map_err(|_| invalid())?;
        Duration::from_secs(n.saturating_mul(86_400))
    } else {
        return parse_window(trimmed);
    };
    if window.is_zero() || window > MAX_TREND_WINDOW {
        return Err(invalid());
    }
    Ok(window)
}

/// The JSON body of every dashboard route: the report, and what it covers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DashboardResponse<T> {
    pub generated_at_ms: i64,
    pub window: String,
    #[serde(flatten)]
    pub report: T,
}

/// The engine view adds what only the process knows: its slots and whether
/// the statistics queue is dropping events.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EngineResponse {
    pub server: ServerInfo,
    #[serde(flatten)]
    pub report: EngineReport,
}

/// The health view adds the process's own figures, as the engine view does.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthResponse {
    pub server: ServerInfo,
    #[serde(flatten)]
    pub report: HealthReport,
}

/// The store-health view adds the store's configured limits.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoreResponse {
    /// The byte cap past which the least recently read entries are evicted.
    pub max_bytes: u64,
    /// The idle window a new entry gets, and its absolute lifetime.
    pub ttl_seconds: u64,
    pub max_lifetime_seconds: u64,
    #[serde(flatten)]
    pub report: StoreHealth,
}

/// Every dashboard route, mounted under each version prefix.
pub(crate) fn dashboard_routes() -> Router<Arc<ServerState>> {
    let mut router = Router::new();
    for view in View::ALL {
        router = router.route(
            view.api_path(),
            get(
                move |state: State<Arc<ServerState>>,
                      headers: HeaderMap,
                      query: Result<Query<DashboardQuery>, QueryRejection>| async move {
                    respond(view, state.0, headers, unpack(query)?, false).await
                },
            ),
        );
    }
    router
}

/// The pages at `/trends`, `/yield` and the rest on the statistics listener.
pub(crate) fn listener_pages(router: Router<Arc<ServerState>>) -> Router<Arc<ServerState>> {
    let mut router = router;
    for view in View::ALL {
        router = router.route(
            view.page_path(),
            get(
                move |state: State<Arc<ServerState>>,
                      headers: HeaderMap,
                      query: Result<Query<DashboardQuery>, QueryRejection>| async move {
                    let mut query = unpack(query)?;
                    query.format.get_or_insert_with(|| "html".to_string());
                    respond(view, state.0, headers, query, true).await
                },
            ),
        );
    }
    router
}

fn unpack(
    query: Result<Query<DashboardQuery>, QueryRejection>,
) -> Result<DashboardQuery, ApiError> {
    query
        .map(|Query(query)| query)
        .map_err(|rejection| ApiError::Invalid(format!("query: {}", rejection.body_text())))
}

async fn respond(
    view: View,
    state: Arc<ServerState>,
    headers: HeaderMap,
    query: DashboardQuery,
    listener: bool,
) -> Result<Response, ApiError> {
    let format = Format::parse(query.format.as_deref(), &headers)?;
    let (label, length) = query.window(view)?;
    let fixed = query.fixed(view)?;
    let refresh = query.refresh()?;
    let bucket = query.bucket()?;
    let conversations = query.conversations()?;
    let conversation = query.conversation()?;

    if format == Format::Html {
        let settings = serde_json::json!({
            "view": view.name(),
            "listener": listener,
            "window": label,
            "fixed": fixed,
            "refresh": refresh,
            "bucket": bucket.as_str(),
            "conversation": conversation,
            "refs": state.core.config().refs.browse,
        });
        return Ok(page(&settings));
    }
    if view == View::Timeline {
        return Err(ApiError::Invalid(
            "the timeline is a page; its data is /v1/stats/analysis?conversation=<id>".into(),
        ));
    }

    let identity = authenticate(&state, &headers)?;
    let tenant = match query.tenant.as_deref() {
        None => identity.tenant.as_str().to_string(),
        Some(name) if name == identity.tenant.as_str() => name.to_string(),
        Some(name) if identity.trusted => name.to_string(),
        Some(_) => return Err(ApiError::Forbidden),
    };
    let now = now_ms();
    let (from_ms, to_ms) = fixed.unwrap_or((now - length.as_millis() as i64, now + 1));
    let window = if fixed.is_some() {
        "fixed range".to_string()
    } else {
        label
    };

    match view {
        View::Health => {
            let (census, truncated) = census(&state, &tenant).await?;
            let server = server_info(&state);
            let runtime = Runtime {
                now_ms: now,
                stats_dropped: server.stats_dropped,
                inflight_limit: server.inflight_limit,
                inflight_free: server.inflight_free,
                store_max_bytes: state.core.config().refs.max_bytes,
                backup: backup_watch(state.core.config()),
            };
            let report: HealthReport = state
                .with_stats(move |r| {
                    r.health(&tenant, from_ms, to_ms, &census, truncated, &runtime)
                })
                .await?;
            let report = HealthResponse { server, report };
            Ok(answer(format, view, now, window, report))
        }
        View::CacheLoss => {
            let report: CompressionCacheReport = state
                .with_stats(move |r| r.compression_cache(&tenant, from_ms, to_ms, conversations))
                .await?;
            Ok(answer(format, view, now, window, report))
        }
        View::Advisor => {
            let mut report: AdvisorReport = state
                .with_stats(move |r| r.advisor(&tenant, from_ms, to_ms))
                .await?;
            // The report knows the traffic; only this side knows what is on.
            // "On" is on for any provider the setting can apply to.
            use kompressor_core::config::{ColdRecompact, PromptCacheKey, ToolsOrder};
            let core = &state.core;
            let any = |on: &dyn Fn(&kompressor_core::config::ProxyConfig) -> bool| {
                [None, Some("anthropic"), Some("openai")]
                    .into_iter()
                    .any(|provider| on(&core.config_for(provider).proxy))
            };
            for (setting, enabled) in [
                ("tools_order", any(&|p| p.tools_order == ToolsOrder::Pinned)),
                ("sticky_beta", any(&|p| p.sticky_beta)),
                (
                    "prompt_cache_key",
                    any(&|p| p.prompt_cache_key == PromptCacheKey::Conversation),
                ),
                ("tools_compact", any(&|p| p.tools_compact)),
                (
                    "cold_recompact",
                    any(&|p| p.cold_recompact != ColdRecompact::Off),
                ),
                (
                    "superseded_reads, matured_reads",
                    any(&|p| p.superseded_reads || p.matured_reads),
                ),
                (
                    "reasoning_elide_models",
                    any(&|p| !p.reasoning_elide_models.is_empty()),
                ),
                ("tool deferral", false),
            ] {
                report.mark_enabled(setting, enabled);
            }
            Ok(answer(format, view, now, window, report))
        }
        View::Prefix => {
            let report: PrefixReport = state
                .with_stats(move |r| r.prefix(&tenant, from_ms, to_ms))
                .await?;
            Ok(answer(format, view, now, window, report))
        }
        View::Traffic => {
            let report: TrafficReport = state
                .with_stats(move |r| r.traffic(&tenant, from_ms, to_ms))
                .await?;
            Ok(answer(format, view, now, window, report))
        }
        View::Quality => {
            let runtime = QualityRuntime {
                now_ms: now,
                stats_dropped: state.core.stats().dropped(),
                retention_days: state.core.config().stats.retention_days,
            };
            let report: QualityReport = state
                .with_stats(move |r| r.quality(&tenant, from_ms, to_ms, &runtime))
                .await?;
            Ok(answer(format, view, now, window, report))
        }
        View::Trends => {
            let report: TrendsReport = state
                .with_stats(move |r| r.trends(&tenant, from_ms, to_ms, bucket))
                .await?;
            Ok(answer(format, view, now, window, report))
        }
        View::Engine => {
            let report = state
                .with_stats(move |r| r.engine(&tenant, from_ms, to_ms))
                .await?;
            let report = EngineResponse {
                server: server_info(&state),
                report,
            };
            Ok(answer(format, view, now, window, report))
        }
        View::Upstream => {
            let report: UpstreamReport = state
                .with_stats(move |r| r.upstream_health(&tenant, from_ms, to_ms))
                .await?;
            Ok(answer(format, view, now, window, report))
        }
        View::KeepWarm => {
            let report: KeepWarmReport = state
                .with_stats(move |r| r.keep_warm(&tenant, from_ms, to_ms, conversations))
                .await?;
            Ok(answer(format, view, now, window, report))
        }
        View::Yield => {
            let (census, truncated) = census(&state, &tenant).await?;
            let report: YieldReport = state
                .with_stats(move |r| {
                    r.compression_yield(&tenant, from_ms, to_ms, &census, truncated)
                })
                .await?;
            Ok(answer(format, view, now, window, report))
        }
        View::Store => {
            let (census, truncated) = census(&state, &tenant).await?;
            let now_s = (now / 1_000).max(0) as u64;
            let report = state
                .with_stats(move |r| {
                    r.store_health(&tenant, from_ms, to_ms, &census, truncated, now_s)
                })
                .await?;
            let config = state.core.config();
            let report = StoreResponse {
                max_bytes: config.refs.max_bytes,
                ttl_seconds: config.engine.ref_ttl_seconds,
                max_lifetime_seconds: config
                    .engine
                    .ref_ttl_seconds
                    .saturating_mul(u64::from(config.engine.ref_max_lifetime_multiplier)),
                report,
            };
            Ok(answer(format, view, now, window, report))
        }
        View::Models => {
            let report = state
                .with_stats(move |r| r.models(&tenant, from_ms, to_ms))
                .await?;
            Ok(answer(format, view, now, window, report))
        }
        View::Activity => {
            let report = state
                .with_stats(move |r| r.activity(&tenant, from_ms, to_ms))
                .await?;
            Ok(answer(format, view, now, window, report))
        }
        View::Cohorts => {
            let report = state
                .with_stats(move |r| r.cohorts(&tenant, from_ms, to_ms))
                .await?;
            Ok(answer(format, view, now, window, report))
        }
        View::Timeline => unreachable!("answered above"),
    }
}

fn answer<T: Serialize>(
    format: Format,
    view: View,
    now: i64,
    window: String,
    report: T,
) -> Response {
    let response = DashboardResponse {
        generated_at_ms: now,
        window,
        report,
    };
    match format {
        Format::Jsonl => jsonl_response(&response, view.name(), now),
        _ => json_response(&response),
    }
}

/// The tenant's ref-store bookkeeping, read off the runtime. A store that
/// cannot be browsed (none in this build) reads as empty.
async fn census(state: &ServerState, tenant: &str) -> Result<(Vec<StoredRef>, bool), ApiError> {
    let core = state.core.clone();
    let tenant = Tenant::new(tenant);
    let census: RefCensus = tokio::task::spawn_blocking(move || match core.refs().browser() {
        Some(browser) => browser
            .census(&tenant)
            .map_err(|error| ApiError::Internal(format!("store: {error}"))),
        None => Ok(RefCensus::default()),
    })
    .await
    .map_err(|error| ApiError::Internal(format!("store task: {error}")))??;
    let refs = census
        .entries
        .into_iter()
        .map(|entry| StoredRef {
            id: entry.id,
            kind: entry.kind,
            bytes: entry.bytes,
            created_at: entry.created_at,
            last_accessed: entry.last_accessed,
            expires_at: entry.expires_at,
            expired: entry.expired,
        })
        .collect();
    Ok((refs, census.truncated))
}

/// What the Health page's "last backup" check is told: how old one may be,
/// and the newest archive the schedule has written. What the statistics have
/// on record is added where the report is read.
fn backup_watch(config: &kompressor_core::Config) -> kompressor_stats::health::BackupWatch {
    let maintenance = &config.maintenance;
    kompressor_stats::health::BackupWatch {
        max_age_ms: maintenance
            .max_age()
            .map(|age| i64::try_from(age.as_millis()).unwrap_or(i64::MAX)),
        last_ok_ms: (!maintenance.backup_dir.as_os_str().is_empty())
            .then(|| kompressor_core::backup::list_archives(&maintenance.backup_dir))
            .and_then(|archives| archives.first().map(|archive| archive.created_at_ms)),
        last_failed_ms: None,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn page(settings: &serde_json::Value) -> Response {
    // As on the other pages: the settings reach the script as one JSON
    // object, so nothing from the request is spliced in as markup.
    let settings = settings.to_string().replace('<', "\\u003c");
    let body = PAGE.replace("/*SETTINGS*/{}", &settings);
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

/// The page. Self-contained like the others: no external scripts, fonts or
/// styles, and everything it draws is inserted as text.
const PAGE: &str = include_str!("dashboards.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_query_defaults_and_bounds() {
        let query = DashboardQuery::default();
        assert_eq!(query.refresh().unwrap(), DEFAULT_REFRESH);
        assert_eq!(query.bucket().unwrap(), TrendBucket::Week);
        assert_eq!(query.conversations().unwrap(), DEFAULT_CONVERSATIONS);
        assert_eq!(query.window(View::Trends).unwrap().0, DEFAULT_TREND_WINDOW);
        assert_eq!(query.window(View::Engine).unwrap().0, DEFAULT_WINDOW);
        assert_eq!(query.window(View::Health).unwrap().0, DEFAULT_HEALTH_WINDOW);

        let with = |tweak: fn(&mut DashboardQuery)| {
            let mut query = DashboardQuery::default();
            tweak(&mut query);
            query
        };
        assert!(with(|q| q.refresh = Some(0)).refresh().is_err());
        assert!(with(|q| q.bucket = Some("year".into())).bucket().is_err());
        assert!(
            with(|q| q.conversations = Some(501))
                .conversations()
                .is_err()
        );
        assert!(
            with(|q| q.conversation = Some("XYZ".into()))
                .conversation()
                .is_err()
        );
        assert!(
            with(|q| q.conversation = Some("00000000000000aa".into()))
                .conversation()
                .is_ok()
        );
        // Trends reach two years back; the raw-row views stop at ninety days.
        let long = with(|q| q.window = Some("104w".into()));
        assert!(long.window(View::Trends).is_ok());
        assert!(long.window(View::Engine).is_err());
        assert!(
            with(|q| q.window = Some("105w".into()))
                .window(View::Trends)
                .is_err()
        );
        assert!(
            with(|q| q.window = Some("0d".into()))
                .window(View::Trends)
                .is_err()
        );
        assert!(
            with(|q| q.window = Some("12h".into()))
                .window(View::Trends)
                .is_ok()
        );
        let range = with(|q| {
            q.from = Some(0);
            q.to = Some(200 * 86_400_000);
        });
        assert!(range.fixed(View::Trends).unwrap().is_some());
        assert!(range.fixed(View::Upstream).is_err());
        assert!(with(|q| q.from = Some(5)).fixed(View::Engine).is_err());
    }

    #[test]
    fn nothing_from_the_request_reaches_the_page_unescaped() {
        let hostile = "</script><script>alert(1)</script>";
        let response = page(&serde_json::json!({ "window": hostile }));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let body = runtime.block_on(async {
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            String::from_utf8(bytes.to_vec()).unwrap()
        });
        assert!(!body.contains("</script><script>"));
        assert!(!body.contains("/*SETTINGS*/{}"));
    }

    #[test]
    fn every_view_has_distinct_paths() {
        let mut pages: Vec<&str> = View::ALL.iter().map(|v| v.page_path()).collect();
        let mut apis: Vec<&str> = View::ALL.iter().map(|v| v.api_path()).collect();
        pages.sort_unstable();
        pages.dedup();
        apis.sort_unstable();
        apis.dedup();
        assert_eq!(
            (pages.len(), apis.len()),
            (View::ALL.len(), View::ALL.len())
        );
        assert!(
            pages
                .iter()
                .all(|p| !["/", "/cache", "/refs", "/analysis"].contains(p))
        );
    }
}
