//! The ref browser: what the store is holding, as JSON and as a page.
//!
//! `GET /v1/refs` lists the caller's tenant's fragments a page at a time —
//! searchable, filterable by kind and request, in five orders — and serves a
//! browser the same page-or-JSON way `/v1/stats/live` does. `GET
//! /v1/refs/{handle}` is one fragment in full with every recorded ask for it,
//! and `DELETE /v1/refs/{handle}` removes one: the thing to do on finding a
//! credential in tool output that should never have been kept.
//!
//! None of it is a retrieval. A read here does not restart a fragment's idle
//! window, file a `retrieval_events` row or lazily delete an expired row, so
//! an operator looking at the store does not change what it holds or what
//! the statistics say about it.
//!
//! The whole surface is off unless `refs.browse` is set: until then a fragment
//! can only be read back by someone holding its handle, and listing them is a
//! capability a deployment should choose to grant.

use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use kompressor_engine::RefId;
use kompressor_stats::reader::RefHistory;
use kompressor_store::browse::{BrowseOrder, BrowseQuery, MAX_PAGE, MAX_PREVIEW_CHARS};
use kompressor_store::types::Tenant;
use serde::{Deserialize, Serialize};

use crate::api::authenticate;
use crate::error::ApiError;
use crate::live::prefers_html;
use crate::middleware::json_response;
use crate::state::ServerState;

/// Entries per page when the caller names no count.
pub const DEFAULT_LIMIT: usize = 50;
/// Preview characters per entry when the caller names no count.
pub const DEFAULT_PREVIEW: usize = 160;
/// Asks listed with one fragment when the caller names no count, and the most.
pub const DEFAULT_HISTORY: usize = 20;
pub const MAX_HISTORY: usize = 200;
/// How often the page re-lists, in seconds, unless `?refresh=` says otherwise.
pub const DEFAULT_REFRESH: u32 = 5;
/// The longest search accepted. A search is a scan of every fragment, and a
/// needle longer than this is a paste, not a query.
pub const MAX_SEARCH_CHARS: usize = 512;

/// The list query. Unknown parameters are refused, as on the other reporting
/// routes: a filter that silently did nothing would look like an answer.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ListQuery {
    /// Only a trusted caller may name a tenant other than its own.
    tenant: Option<String>,
    /// Substring of the fragment, the handle or the request id.
    q: Option<String>,
    kind: Option<String>,
    request_id: Option<String>,
    /// List rows past their window that are still on disk.
    expired: Option<bool>,
    /// `newest`, `oldest`, `recent`, `largest` or `expiring`.
    order: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
    /// Preview characters per entry.
    preview: Option<usize>,
    /// `json` or `html`. Without it, the `Accept` header decides.
    format: Option<String>,
    /// Seconds between the page's re-lists, 1 to 3600. HTML only.
    refresh: Option<u32>,
}

impl ListQuery {
    fn browse_query(&self) -> Result<BrowseQuery, ApiError> {
        let order = match self.order.as_deref() {
            None | Some("") => BrowseOrder::default(),
            Some(name) => BrowseOrder::parse(name).ok_or_else(|| {
                ApiError::Invalid(format!(
                    "order must be newest, oldest, recent, largest or expiring, not {name}"
                ))
            })?,
        };
        let limit = self.limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_PAGE).contains(&limit) {
            return Err(ApiError::Invalid(format!(
                "limit must be between 1 and {MAX_PAGE}, not {limit}"
            )));
        }
        let preview = self.preview.unwrap_or(DEFAULT_PREVIEW);
        if preview > MAX_PREVIEW_CHARS {
            return Err(ApiError::Invalid(format!(
                "preview must be at most {MAX_PREVIEW_CHARS}, not {preview}"
            )));
        }
        if let Some(q) = &self.q
            && q.chars().count() > MAX_SEARCH_CHARS
        {
            return Err(ApiError::Invalid(format!(
                "q must be at most {MAX_SEARCH_CHARS} characters"
            )));
        }
        let text = |value: &Option<String>| value.clone().filter(|v| !v.trim().is_empty());
        Ok(BrowseQuery {
            search: text(&self.q),
            kind: text(&self.kind),
            request_id: text(&self.request_id),
            include_expired: self.expired.unwrap_or(false),
            order,
            offset: self.offset.unwrap_or(0),
            limit,
            preview_chars: preview,
        })
    }

    fn wants_html(&self, headers: &HeaderMap) -> Result<bool, ApiError> {
        match self.format.as_deref() {
            Some("json") => Ok(false),
            Some("html") => Ok(true),
            Some(other) => Err(ApiError::Invalid(format!(
                "format must be json or html, not {other}"
            ))),
            None => Ok(prefers_html(headers)),
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

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DetailQuery {
    tenant: Option<String>,
    /// How many recorded asks to list, 0 to 200.
    history: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DeleteQuery {
    tenant: Option<String>,
}

/// One row of a page.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RefEntry {
    /// `k_` and the handle body: what `/v1/retrieve` and this route accept.
    pub handle: String,
    pub kind: String,
    pub request_id: Option<String>,
    pub bytes: u64,
    /// Unix seconds, as the store keeps them.
    pub created_at: u64,
    pub last_accessed: u64,
    pub ttl_seconds: u64,
    pub max_lifetime_seconds: u64,
    pub expires_at: u64,
    pub expired: bool,
    pub preview: String,
    /// Characters of fragment before `preview`: under a search, the preview
    /// is a window around the first match in the text.
    pub preview_start: u64,
    pub truncated: bool,
}

/// What the tenant holds right now, live rows only.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeldInfo {
    pub live_refs: u64,
    pub live_bytes: u64,
    pub oldest_created_at: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KindCount {
    pub kind: String,
    pub count: u64,
}

/// The JSON body of `GET /v1/refs`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RefsPage {
    pub generated_at_ms: i64,
    /// The store's clock, in the unit its timestamps use, so a client can
    /// say "expires in" without trusting its own clock.
    pub now: u64,
    pub tenant: String,
    pub held: HeldInfo,
    /// Entries matching the filters, across every page.
    pub total: u64,
    pub offset: usize,
    pub limit: usize,
    pub order: String,
    pub kinds: Vec<KindCount>,
    pub entries: Vec<RefEntry>,
}

/// The JSON body of `GET /v1/refs/{handle}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RefDetailResponse {
    pub now: u64,
    pub tenant: String,
    #[serde(flatten)]
    pub entry: RefEntry,
    pub fragment: String,
    pub chars: usize,
    pub lines: usize,
    /// Every recorded ask, or `None` when statistics are off.
    pub retrievals: Option<RefHistory>,
}

/// The browser's routes, mounted under each version prefix by the API router
/// and by the statistics listener.
pub(crate) fn refs_routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/refs", get(list))
        .route("/refs/{handle}", get(detail).delete(remove))
}

/// The page at `/refs` on the statistics listener, polling `/v1/refs`.
pub(crate) async fn page_on_stats_listener(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let mut query = unpack(query)?;
    query.format.get_or_insert_with(|| "html".to_string());
    respond(state, headers, query, "/v1/refs", "/").await
}

async fn list(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    respond(state, headers, unpack(query)?, "", "").await
}

fn unpack<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, ApiError> {
    query
        .map(|Query(query)| query)
        .map_err(|rejection| ApiError::Invalid(format!("query: {}", rejection.body_text())))
}

/// `endpoint` and `live` are where the page should poll and where its link
/// to the live statistics goes; empty means "work it out from this page's
/// own path", which is right everywhere except on the statistics listener.
async fn respond(
    state: Arc<ServerState>,
    headers: HeaderMap,
    query: ListQuery,
    endpoint: &str,
    live: &str,
) -> Result<Response, ApiError> {
    let browse = query.browse_query()?;
    let refresh = query.refresh()?;

    // The shell is served whether or not browsing is on, and to anyone, as
    // the live page is: a navigation cannot carry the key header, so the
    // data arrives only once the page's own fetch presents one, and a
    // deployment with browsing off gets a page that says so.
    if query.wants_html(&headers)? {
        return Ok(page(refresh, endpoint, live));
    }

    let tenant = scoped_tenant(&state, &headers, query.tenant.as_deref())?;
    let core = state.core.clone();
    let scoped = tenant.clone();
    let (page, held, now) = blocking(move || {
        let browser = core.refs().browser().ok_or_else(unbrowsable)?;
        let page = browser.browse(&scoped, &browse).map_err(store_error)?;
        let held = core.refs().stats_for(&scoped).map_err(store_error)?;
        Ok((page, held, now_s()))
    })
    .await?;

    Ok(json_response(&RefsPage {
        generated_at_ms: now_ms(),
        now,
        tenant: tenant.as_str().to_string(),
        held: HeldInfo {
            live_refs: held.live_refs,
            live_bytes: held.live_bytes,
            oldest_created_at: held.oldest_created_at,
        },
        total: page.total,
        offset: query.offset.unwrap_or(0),
        limit: query.limit.unwrap_or(DEFAULT_LIMIT),
        order: query
            .order
            .clone()
            .filter(|o| !o.is_empty())
            .unwrap_or_else(|| BrowseOrder::default().as_str().to_string()),
        kinds: page
            .kinds
            .into_iter()
            .map(|(kind, count)| KindCount { kind, count })
            .collect(),
        entries: page.entries.into_iter().map(entry).collect(),
    }))
}

async fn detail(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(handle): Path<String>,
    query: Result<Query<DetailQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let query = unpack(query)?;
    let history = query.history.unwrap_or(DEFAULT_HISTORY);
    if history > MAX_HISTORY {
        return Err(ApiError::Invalid(format!(
            "history must be at most {MAX_HISTORY}, not {history}"
        )));
    }
    let tenant = scoped_tenant(&state, &headers, query.tenant.as_deref())?;
    let id = parse_handle(&handle)?;

    let core = state.core.clone();
    let scoped = tenant.clone();
    let found = blocking(move || {
        let browser = core.refs().browser().ok_or_else(unbrowsable)?;
        browser.inspect(&scoped, &id).map_err(store_error)
    })
    .await?;
    let Some(found) = found else {
        return Err(ApiError::NotFound(format!(
            "no fragment {id} in this tenant's store"
        )));
    };

    // The history is a nicety. A statistics database that is off, not yet
    // created or failing costs the page its table, never the fragment.
    let asked = tenant.as_str().to_string();
    let retrievals = state
        .with_stats(move |reader| reader.ref_history(&asked, id.body(), history))
        .await
        .ok();

    let fragment = found.record.fragment.clone();
    let chars = fragment.chars().count();
    let lines = if fragment.is_empty() {
        0
    } else {
        fragment.lines().count()
    };
    let detail = kompressor_store::browse::RefSummary {
        id,
        kind: found.record.kind.clone(),
        request_id: found.record.request_id.clone(),
        bytes: found.record.bytes(),
        created_at: found.record.created_at,
        last_accessed: found.record.last_accessed,
        ttl_seconds: found.record.ttl_seconds,
        max_lifetime_seconds: found.max_lifetime_seconds,
        expires_at: found.expires_at,
        expired: found.expired,
        preview: String::new(),
        preview_start: 0,
        truncated: false,
    };
    Ok(json_response(&RefDetailResponse {
        now: now_s(),
        tenant: tenant.as_str().to_string(),
        entry: entry(detail),
        fragment,
        chars,
        lines,
        retrievals,
    }))
}

async fn remove(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(handle): Path<String>,
    query: Result<Query<DeleteQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let query = unpack(query)?;
    let tenant = scoped_tenant(&state, &headers, query.tenant.as_deref())?;
    let id = parse_handle(&handle)?;

    let core = state.core.clone();
    let scoped = tenant.clone();
    let removed = blocking(move || {
        let browser = core.refs().browser().ok_or_else(unbrowsable)?;
        browser.remove(&scoped, &[id]).map_err(store_error)
    })
    .await?;
    // The handle and tenant only. What was in the fragment is the reason it
    // is being deleted, and the one thing that must not reach a log.
    tracing::info!(tenant = tenant.as_str(), handle = %id, removed, "ref deleted from the browser");
    if removed == 0 {
        return Err(ApiError::NotFound(format!(
            "no fragment {id} in this tenant's store"
        )));
    }
    Ok(json_response(&serde_json::json!({
        "handle": id.to_string(),
        "removed": true,
    })))
}

/// Authenticate, refuse when browsing is off, and pick the tenant the same
/// way every reporting route does.
fn scoped_tenant(
    state: &ServerState,
    headers: &HeaderMap,
    requested: Option<&str>,
) -> Result<Tenant, ApiError> {
    if !state.core.config().refs.browse {
        return Err(ApiError::NotFound(
            "the ref browser is off on this deployment; set refs.browse = true \
             (or KOMPRESSOR__REFS__BROWSE=true) to turn it on"
                .to_string(),
        ));
    }
    let identity = authenticate(state, headers)?;
    let name = match requested {
        None => identity.tenant.as_str().to_string(),
        Some(name) if name == identity.tenant.as_str() => name.to_string(),
        Some(name) if identity.trusted => name.to_string(),
        Some(_) => return Err(ApiError::Forbidden),
    };
    Ok(Tenant::new(name))
}

fn parse_handle(handle: &str) -> Result<RefId, ApiError> {
    let text = handle.trim();
    RefId::parse(text.strip_prefix("ref:").unwrap_or(text))
        .ok_or_else(|| ApiError::Invalid(format!("{text:?} is not a ref handle")))
}

fn entry(summary: kompressor_store::browse::RefSummary) -> RefEntry {
    RefEntry {
        handle: summary.id.to_string(),
        kind: summary.kind,
        request_id: summary.request_id,
        bytes: summary.bytes,
        created_at: summary.created_at,
        last_accessed: summary.last_accessed,
        ttl_seconds: summary.ttl_seconds,
        max_lifetime_seconds: summary.max_lifetime_seconds,
        expires_at: summary.expires_at,
        expired: summary.expired,
        preview: summary.preview,
        preview_start: summary.preview_start,
        truncated: summary.truncated,
    }
}

fn unbrowsable() -> ApiError {
    ApiError::NotFound("this deployment's ref store cannot be browsed".to_string())
}

fn store_error(error: kompressor_store::types::StoreError) -> ApiError {
    ApiError::Internal(format!("store: {error}"))
}

/// A store call, off the runtime: the SQLite backend scans to search.
async fn blocking<T, F>(f: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|error| ApiError::Internal(format!("store task: {error}")))?
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Unix seconds: the store's [`kompressor_store::Clock::system`] unit.
fn now_s() -> u64 {
    (now_ms().max(0) / 1000) as u64
}

fn page(refresh: u32, endpoint: &str, live: &str) -> Response {
    // As on the live page: the settings reach the script as one JSON object,
    // so nothing from the request is spliced into markup as text.
    let settings = serde_json::json!({
        "refresh": refresh,
        "endpoint": endpoint,
        "live": live,
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

/// The browser page. Self-contained, like the live dashboard, and everything
/// it draws — fragment text above all — is inserted as text, never as markup.
const PAGE: &str = include_str!("refs.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_query_defaults_and_bounds() {
        let query = ListQuery::default().browse_query().unwrap();
        assert_eq!(query.limit, DEFAULT_LIMIT);
        assert_eq!(query.preview_chars, DEFAULT_PREVIEW);
        assert_eq!(query.order, BrowseOrder::Newest);
        assert!(!query.include_expired);

        let bad = |tweak: fn(&mut ListQuery)| {
            let mut query = ListQuery::default();
            tweak(&mut query);
            query.browse_query().is_err()
        };
        assert!(bad(|q| q.limit = Some(0)));
        assert!(bad(|q| q.limit = Some(MAX_PAGE + 1)));
        assert!(bad(|q| q.preview = Some(MAX_PREVIEW_CHARS + 1)));
        assert!(bad(|q| q.order = Some("random".into())));
        assert!(bad(|q| q.q = Some("x".repeat(MAX_SEARCH_CHARS + 1))));
    }

    #[test]
    fn blank_filters_are_no_filter() {
        let query = ListQuery {
            q: Some("  ".into()),
            kind: Some(String::new()),
            order: Some(String::new()),
            ..Default::default()
        }
        .browse_query()
        .unwrap();
        assert_eq!(query.search, None);
        assert_eq!(query.kind, None);
        assert_eq!(query.order, BrowseOrder::Newest);
    }

    #[test]
    fn handles_parse_in_every_spelling_a_marker_uses() {
        let id = kompressor_engine::refs::ref_for("x");
        for text in [id.to_string(), format!("ref:{id}"), id.body().to_string()] {
            assert_eq!(parse_handle(&text).unwrap(), id);
        }
        assert!(parse_handle("too-short").is_err());
    }

    #[test]
    fn nothing_from_the_request_reaches_the_page_unescaped() {
        let hostile = "</script><script>alert(1)</script>";
        let response = page(5, hostile, "");
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
