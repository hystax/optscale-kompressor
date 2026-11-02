//! Live statistics, as JSON for machines and as a page for people.
//!
//! `GET /v1/stats/live` answers with one [`LiveSnapshot`] for a trailing
//! window ending now, wrapped with what only the running process knows: its
//! version, uptime, mode and whether the statistics queue is dropping events.
//! The same path serves an HTML dashboard to a browser — chosen by
//! `?format=html` or by an `Accept` header that prefers HTML — which polls the
//! JSON every couple of seconds and redraws.
//!
//! Every number comes from the statistics database, which the writer commits
//! every 100 ms, so "live" means a request is on the page a fraction of a
//! second after it finished. There is no second set of in-memory counters to
//! drift away from what `kompressor stats` reports.
//!
//! [`stats_listener_router`] serves the same thing on a listener of its own
//! (`server.stats_listen`), with the page at `/`, so a deployment that runs
//! only the proxy still has a dashboard and the port can be exposed on its own.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use kompressor_stats::live::LiveSnapshot;
use serde::{Deserialize, Serialize};

use crate::api::authenticate;
use crate::error::ApiError;
use crate::middleware::{json_response, with_request_middleware};
use crate::state::ServerState;

/// The window when the caller names none.
pub const DEFAULT_WINDOW: &str = "1h";
/// The widest window served. Raw rows are pruned after the retention period
/// (90 days by default), so asking further back would only read nothing.
pub const MAX_WINDOW: Duration = Duration::from_secs(90 * 86_400);
/// Recent requests listed when the caller names no count, and the most served.
pub const DEFAULT_RECENT: usize = 25;
pub const MAX_RECENT: usize = 200;
/// How often the page polls, in seconds, unless `?refresh=` says otherwise.
pub const DEFAULT_REFRESH: u32 = 2;

/// When this process started serving, for `uptime_s`.
///
/// Set by the first [`ServerState`] built. Every listener in the process
/// builds its own state, so a per-state instant would reset whenever a
/// listener is composed; a process-wide one does not.
static STARTED: OnceLock<(Instant, i64)> = OnceLock::new();

pub(crate) fn mark_started() {
    STARTED.get_or_init(|| (Instant::now(), now_ms()));
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The query the live routes accept. Unknown parameters are refused, as on
/// the other reporting routes.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LiveQuery {
    /// Only a trusted caller may name a tenant other than its own.
    tenant: Option<String>,
    /// Trailing window: `30s`, `15m`, `1h`, `24h`, `7d`.
    window: Option<String>,
    /// How many recent requests to list, 0 to 200.
    recent: Option<usize>,
    /// `json` or `html`. Without it, the `Accept` header decides.
    format: Option<String>,
    /// Seconds between the page's polls, 1 to 3600. HTML only.
    refresh: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Json,
    Html,
}

impl LiveQuery {
    fn format(&self, headers: &HeaderMap) -> Result<Format, ApiError> {
        match self.format.as_deref() {
            Some("json") => Ok(Format::Json),
            Some("html") => Ok(Format::Html),
            Some(other) => Err(ApiError::Invalid(format!(
                "format must be json or html, not {other}"
            ))),
            None => Ok(if prefers_html(headers) {
                Format::Html
            } else {
                Format::Json
            }),
        }
    }

    fn recent(&self) -> Result<usize, ApiError> {
        match self.recent {
            None => Ok(DEFAULT_RECENT),
            Some(n) if n <= MAX_RECENT => Ok(n),
            Some(n) => Err(ApiError::Invalid(format!(
                "recent must be at most {MAX_RECENT}, not {n}"
            ))),
        }
    }

    fn refresh(&self) -> Result<u32, ApiError> {
        match self.refresh {
            None => Ok(DEFAULT_REFRESH),
            Some(n @ 1..=3600) => Ok(n),
            Some(n) => Err(ApiError::Invalid(format!(
                "refresh must be between 1 and 3600 seconds, not {n}"
            ))),
        }
    }
}

/// A browser asks for `text/html` ahead of anything else; `curl` and API
/// clients send `*/*` or `application/json`, and get JSON.
pub(crate) fn prefers_html(headers: &HeaderMap) -> bool {
    let Some(accept) = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let html = accept.find("text/html");
    let json = accept.find("application/json");
    match (html, json) {
        (Some(h), Some(j)) => h < j,
        (Some(_), None) => true,
        _ => false,
    }
}

/// Parse a trailing window: a number and one of `s`, `m`, `h`, `d`.
pub fn parse_window(text: &str) -> Result<Duration, ApiError> {
    let text = text.trim();
    let invalid = || {
        ApiError::Invalid(format!(
            "window must be a number and a unit (s, m, h, d), like 15m or 24h, not {text:?}"
        ))
    };
    let unit_at = text
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(invalid)?;
    let (number, unit) = text.split_at(unit_at);
    let number: u64 = number.parse().map_err(|_| invalid())?;
    let seconds = match unit {
        "s" => 1,
        "m" | "min" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return Err(invalid()),
    };
    if number == 0 {
        return Err(invalid());
    }
    let window = Duration::from_secs(number.saturating_mul(seconds));
    if window > MAX_WINDOW {
        return Err(ApiError::Invalid(format!(
            "window can be at most 90d — older rows are pruned — not {text}"
        )));
    }
    Ok(window)
}

/// What the process knows that the database does not.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerInfo {
    pub version: String,
    pub started_at_ms: i64,
    pub uptime_s: u64,
    /// The configured compression mode.
    pub mode: String,
    pub proxy_enabled: bool,
    /// Events the statistics queue dropped since start. Non-zero means every
    /// figure on this page is an undercount.
    pub stats_dropped: u64,
    pub inflight_limit: usize,
    /// Engine slots free right now; `inflight_limit` minus this is the load.
    pub inflight_free: usize,
}

/// The window the snapshot covers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WindowInfo {
    pub label: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub bucket_ms: i64,
}

/// The JSON body of `/v1/stats/live`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveResponse {
    pub generated_at_ms: i64,
    pub server: ServerInfo,
    pub window: WindowInfo,
    #[serde(flatten)]
    pub stats: LiveSnapshot,
}

/// The live routes, mounted under each version prefix by the API router.
pub(crate) fn live_routes() -> Router<Arc<ServerState>> {
    Router::new().route("/stats/live", get(live))
}

/// Everything the dedicated statistics listener serves: the page at `/`, the
/// ref browser at `/refs`, the versioned live and ref routes, and the probes.
pub fn stats_listener_router(state: Arc<ServerState>) -> Router {
    let routes = live_routes()
        .merge(crate::refs::refs_routes())
        .with_state(state.clone());
    let router = Router::new()
        .route("/", get(page_at_root))
        .route("/refs", get(crate::refs::page_on_stats_listener))
        .with_state(state.clone())
        .nest("/v1", routes.clone())
        .nest("/v2", routes)
        .merge(crate::ops::ops_router(state.clone()));
    with_request_middleware(router, state, "stats")
}

async fn page_at_root(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<LiveQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let mut query = unpack(query)?;
    query.format.get_or_insert_with(|| "html".to_string());
    respond(state, headers, query, "/v1/stats/live").await
}

async fn live(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<LiveQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    respond(state, headers, unpack(query)?, "").await
}

fn unpack(query: Result<Query<LiveQuery>, QueryRejection>) -> Result<LiveQuery, ApiError> {
    query
        .map(|Query(query)| query)
        .map_err(|rejection| ApiError::Invalid(format!("query: {}", rejection.body_text())))
}

/// `endpoint` is where the page should poll: empty means "the path this page
/// was served from", which is right everywhere except at `/`.
async fn respond(
    state: Arc<ServerState>,
    headers: HeaderMap,
    query: LiveQuery,
    endpoint: &str,
) -> Result<Response, ApiError> {
    let format = query.format(&headers)?;
    let label = query
        .window
        .clone()
        .unwrap_or_else(|| DEFAULT_WINDOW.to_string());
    let window = parse_window(&label)?;
    let recent = query.recent()?;
    let refresh = query.refresh()?;

    // The page authenticates its own polls: a browser cannot attach the key
    // header to a navigation, so the shell is served to anyone and the data
    // arrives only once the poll presents a key — see `PAGE`.
    if format == Format::Html {
        let refs = state.core.config().refs.browse;
        return Ok(page(&label, recent, refresh, endpoint, refs));
    }

    let identity = authenticate(&state, &headers)?;
    let tenant = match query.tenant.as_deref() {
        None => identity.tenant.as_str().to_string(),
        Some(name) if name == identity.tenant.as_str() => name.to_string(),
        Some(name) if identity.trusted => name.to_string(),
        Some(_) => return Err(ApiError::Forbidden),
    };

    let to_ms = now_ms();
    let from_ms = to_ms - window.as_millis() as i64;
    let stats = state
        .with_stats(move |reader| reader.live(&tenant, from_ms, to_ms + 1, recent))
        .await?;
    Ok(json_response(&LiveResponse {
        generated_at_ms: to_ms,
        server: server_info(&state),
        window: WindowInfo {
            label,
            from_ms,
            to_ms,
            bucket_ms: stats.bucket_ms,
        },
        stats,
    }))
}

fn server_info(state: &ServerState) -> ServerInfo {
    let config = state.core.config();
    let (started, started_at_ms) = *STARTED.get_or_init(|| (Instant::now(), now_ms()));
    ServerInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_at_ms,
        uptime_s: started.elapsed().as_secs(),
        mode: config.engine.mode.to_string(),
        proxy_enabled: config.proxy.enabled,
        stats_dropped: state.core.stats().dropped(),
        inflight_limit: config.server.inflight_limit(),
        inflight_free: state.available_permits(),
    }
}

fn page(window: &str, recent: usize, refresh: u32, endpoint: &str, refs: bool) -> Response {
    // The values reach the page as one JSON object, so nothing a caller
    // put in the query string is ever spliced into markup or script as text.
    let settings = serde_json::json!({
        "window": window,
        "recent": recent,
        "refresh": refresh,
        "endpoint": endpoint,
        // Whether to link the ref browser; it 404s when browsing is off.
        "refs": refs,
    })
    .to_string()
    .replace('<', "\\u003c");
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

/// The dashboard. Self-contained — no external scripts, fonts or styles — so
/// it works on a machine with no internet access, which is where a local proxy
/// usually runs. Everything it draws comes from the JSON and is inserted as
/// text, never as markup.
const PAGE: &str = include_str!("live.html");

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn accept(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn windows_parse_in_four_units_and_refuse_everything_else() {
        assert_eq!(parse_window("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_window("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_window("24h").unwrap(), Duration::from_secs(86_400));
        assert_eq!(parse_window("7d").unwrap(), Duration::from_secs(604_800));
        for bad in ["", "h", "15", "0m", "1w", "-5m", "1.5h", "91d"] {
            assert!(parse_window(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn a_browser_gets_the_page_and_everything_else_gets_json() {
        let browser = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
        let query = LiveQuery::default();
        assert_eq!(query.format(&accept(browser)).unwrap(), Format::Html);
        assert_eq!(query.format(&accept("*/*")).unwrap(), Format::Json);
        assert_eq!(
            query.format(&accept("application/json")).unwrap(),
            Format::Json
        );
        assert_eq!(query.format(&HeaderMap::new()).unwrap(), Format::Json);
        let explicit = LiveQuery {
            format: Some("json".into()),
            ..Default::default()
        };
        assert_eq!(explicit.format(&accept(browser)).unwrap(), Format::Json);
        let wrong = LiveQuery {
            format: Some("xml".into()),
            ..Default::default()
        };
        assert!(wrong.format(&HeaderMap::new()).is_err());
    }

    #[test]
    fn recent_and_refresh_are_bounded() {
        let q = |recent, refresh| LiveQuery {
            recent,
            refresh,
            ..Default::default()
        };
        assert_eq!(q(None, None).recent().unwrap(), DEFAULT_RECENT);
        assert_eq!(q(Some(0), None).recent().unwrap(), 0);
        assert!(q(Some(MAX_RECENT + 1), None).recent().is_err());
        assert_eq!(q(None, None).refresh().unwrap(), DEFAULT_REFRESH);
        assert!(q(None, Some(0)).refresh().is_err());
        assert!(q(None, Some(3_601)).refresh().is_err());
    }

    #[test]
    fn query_text_reaches_the_page_only_inside_escaped_json() {
        let hostile = "</script><script>alert(1)</script>";
        let response = page(hostile, 5, 2, "", false);
        let body = futures_body(response);
        assert!(
            !body.contains("</script><script>"),
            "the query must not close the tag"
        );
        assert!(body.contains("\\u003c/script>"));
        assert!(
            !body.contains("/*SETTINGS*/{}"),
            "the placeholder is replaced"
        );
    }

    fn futures_body(response: Response) -> String {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            String::from_utf8(bytes.to_vec()).unwrap()
        })
    }
}
