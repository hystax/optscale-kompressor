//! Cache analysis, as JSON for machines and as a page for people.
//!
//! `GET /v1/stats/cache` answers with one [`CacheReport`] for a trailing
//! window: what the provider's prompt cache read, wrote and lost, why each
//! loss happened, what the keep-warm pings did, and the same per conversation.
//! `?conversation=<id>` adds that conversation's rows one by one. A browser
//! gets a page instead, chosen as on `/v1/stats/live` by `?format=html` or an
//! `Accept` header that prefers HTML; on the statistics listener
//! (`server.stats_listen`) the page is also at `/cache`.
//!
//! Every figure is read from the statistics database's `proxy_requests` rows;
//! see [`kompressor_stats::cache`] for how a row is classified. Like the live
//! view it holds no content, only counts, so it is served wherever the live
//! view is and with the same credentials.

use std::sync::Arc;

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use kompressor_stats::cache::CacheReport;
use serde::{Deserialize, Serialize};

use crate::api::authenticate;
use crate::error::ApiError;
use crate::export::jsonl_response;
use crate::live::{Format, parse_window};
use crate::middleware::json_response;
use crate::state::ServerState;

/// The window when the caller names none: wide enough to hold the idle gaps
/// the report is about.
pub const DEFAULT_WINDOW: &str = "6h";
/// Conversations listed when the caller names no count, and the most served.
pub const DEFAULT_CONVERSATIONS: usize = 50;
pub const MAX_CONVERSATIONS: usize = 500;
/// How often the page polls, in seconds, unless `?refresh=` says otherwise.
pub const DEFAULT_REFRESH: u32 = 10;

/// The query the cache routes accept. Unknown parameters are refused, as on
/// the other reporting routes.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct CacheQuery {
    /// Only a trusted caller may name a tenant other than its own.
    tenant: Option<String>,
    /// Trailing window: `30m`, `6h`, `24h`, `7d`.
    window: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
    /// How many conversations to list, 0 to 500.
    conversations: Option<usize>,
    /// One conversation's id, to list its rows.
    conversation: Option<String>,
    /// `json`, `jsonl` or `html`. Without it, the `Accept` header decides.
    format: Option<String>,
    /// Seconds between the page's polls, 1 to 3600. HTML only.
    refresh: Option<u32>,
}

impl CacheQuery {
    fn format(&self, headers: &HeaderMap) -> Result<Format, ApiError> {
        Format::parse(self.format.as_deref(), headers)
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

    /// A conversation id is the prefix tracker's fingerprint: sixteen
    /// lowercase hex digits. Anything else cannot name one.
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

/// The JSON body of `/v1/stats/cache`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CacheResponse {
    pub generated_at_ms: i64,
    pub window: String,
    #[serde(flatten)]
    pub report: CacheReport,
}

/// The page at `/cache` on the statistics listener, polling
/// `/v1/stats/cache`.
pub(crate) async fn page_on_stats_listener(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<CacheQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let mut query = unpack(query)?;
    query.format.get_or_insert_with(|| "html".to_string());
    respond(state, headers, query, "/v1/stats/cache", "/", "/refs").await
}

pub(crate) async fn cache(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<CacheQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    respond(state, headers, unpack(query)?, "", "", "").await
}

fn unpack(query: Result<Query<CacheQuery>, QueryRejection>) -> Result<CacheQuery, ApiError> {
    query
        .map(|Query(query)| query)
        .map_err(|rejection| ApiError::Invalid(format!("query: {}", rejection.body_text())))
}

/// `endpoint`, `live` and `refs` are where the page polls and where its links
/// go; empty means "work it out from this page's own path", which is right
/// everywhere except on the statistics listener.
async fn respond(
    state: Arc<ServerState>,
    headers: HeaderMap,
    query: CacheQuery,
    endpoint: &str,
    live: &str,
    refs: &str,
) -> Result<Response, ApiError> {
    let format = query.format(&headers)?;
    let label = query
        .window
        .clone()
        .unwrap_or_else(|| DEFAULT_WINDOW.to_string());
    let window = parse_window(&label)?;
    let conversations = query.conversations()?;
    let conversation = query.conversation()?;
    let refresh = query.refresh()?;
    let fixed = match (query.from, query.to) {
        (None, None) => None,
        (Some(from), Some(to))
            if from >= 0 && to > from && to.saturating_sub(from) <= 90 * 86_400_000 =>
        {
            Some((from, to))
        }
        _ => return Err(ApiError::Invalid(
            "from/to must both be epoch milliseconds with 0 <= from < to, spanning at most 90 days"
                .into(),
        )),
    };

    // As on the live page: the shell is served to anyone, and the data only
    // to a poll that presents a key.
    if format == Format::Html {
        let browse = state.core.config().refs.browse;
        return Ok(page(
            &label,
            refresh,
            conversation.as_deref(),
            endpoint,
            live,
            browse.then_some(refs),
            fixed,
        ));
    }

    let identity = authenticate(&state, &headers)?;
    let tenant = match query.tenant.as_deref() {
        None => identity.tenant.as_str().to_string(),
        Some(name) if name == identity.tenant.as_str() => name.to_string(),
        Some(name) if identity.trusted => name.to_string(),
        Some(_) => return Err(ApiError::Forbidden),
    };

    let to_ms = now_ms();
    let (from_ms, end_ms) = fixed.unwrap_or((to_ms - window.as_millis() as i64, to_ms + 1));
    let report = state
        .with_stats(move |reader| {
            reader.cache(
                &tenant,
                from_ms,
                end_ms,
                conversations,
                conversation.as_deref(),
            )
        })
        .await?;
    let response = CacheResponse {
        generated_at_ms: to_ms,
        window: if fixed.is_some() {
            "fixed range".into()
        } else {
            label
        },
        report,
    };
    Ok(match format {
        Format::Jsonl => jsonl_response(&response, "cache", to_ms),
        _ => json_response(&response),
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn page(
    window: &str,
    refresh: u32,
    conversation: Option<&str>,
    endpoint: &str,
    live: &str,
    refs: Option<&str>,
    fixed: Option<(i64, i64)>,
) -> Response {
    // The settings reach the script as one JSON object, so nothing from the
    // request is spliced into markup or script as text.
    let settings = serde_json::json!({
        "window": window,
        "fixed": fixed,
        "refresh": refresh,
        "conversation": conversation,
        "endpoint": endpoint,
        "live": live,
        // Whether to link the ref browser (it 404s when off), and where; an
        // empty path is worked out from the page's own.
        "refs": refs.is_some(),
        "refs_path": refs.unwrap_or(""),
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

/// The page. Self-contained, like the live dashboard: no external scripts,
/// fonts or styles, and everything it draws is inserted as text.
const PAGE: &str = include_str!("cache.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_query_defaults_and_bounds() {
        let query = CacheQuery::default();
        assert_eq!(query.conversations().unwrap(), DEFAULT_CONVERSATIONS);
        assert_eq!(query.refresh().unwrap(), DEFAULT_REFRESH);
        assert_eq!(query.conversation().unwrap(), None);
        let bad = |tweak: fn(&mut CacheQuery)| {
            let mut query = CacheQuery::default();
            tweak(&mut query);
            query.conversations().is_err()
                || query.refresh().is_err()
                || query.conversation().is_err()
                || query.format(&HeaderMap::new()).is_err()
        };
        assert!(bad(|q| q.conversations = Some(MAX_CONVERSATIONS + 1)));
        assert!(bad(|q| q.refresh = Some(0)));
        assert!(bad(|q| q.conversation = Some("not-hex".into())));
        assert!(bad(|q| q.conversation = Some("00000000000000AA".into())));
        assert!(bad(|q| q.format = Some("xml".into())));
        assert!(!bad(|q| q.conversation = Some("00000000000000aa".into())));
        assert!(!bad(|q| q.conversation = Some("  ".into())));
        assert!(!bad(|q| q.format = Some("jsonl".into())));
    }

    #[test]
    fn nothing_from_the_request_reaches_the_page_unescaped() {
        let hostile = "</script><script>alert(1)</script>";
        let response = page(hostile, 10, None, hostile, "", Some(hostile), None);
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
}
