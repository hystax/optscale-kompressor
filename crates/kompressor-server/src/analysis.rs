//! Fixed-window compression analysis, using the same authentication as live stats.
use crate::{
    api::authenticate, error::ApiError, export::jsonl_response, live::Format,
    middleware::json_response, state::ServerState,
};
use axum::{
    extract::{Query, State, rejection::QueryRejection},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct AnalysisQuery {
    tenant: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
    conversation: Option<String>,
    format: Option<String>,
    compare_from: Option<i64>,
    compare_to: Option<i64>,
    model: Option<String>,
    mode: Option<String>,
    surface: Option<String>,
    experiment_id: Option<String>,
    run_id: Option<String>,
}

pub(crate) async fn analysis(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<AnalysisQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    respond(state, headers, query, false, View::Overview).await
}
pub(crate) async fn page_on_stats_listener(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<AnalysisQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    respond(state, headers, query, true, View::Overview).await
}
#[derive(Clone, Copy, Debug, PartialEq)]
enum View {
    Overview,
    Retrieval,
    Latency,
    Comparison,
}

pub(crate) async fn retrieval(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<AnalysisQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    respond(state, headers, query, false, View::Retrieval).await
}
pub(crate) async fn latency(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<AnalysisQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    respond(state, headers, query, false, View::Latency).await
}
pub(crate) async fn comparison(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<AnalysisQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    respond(state, headers, query, false, View::Comparison).await
}

/// A request that passed every check, ready to be answered.
#[derive(Debug)]
struct Plan {
    format: Format,
    tenant: Option<String>,
    from: i64,
    to: i64,
    conversation: Option<String>,
    filter: kompressor_stats::history::HistoryFilter,
    /// The window `compare` measures against: the one before `from..to` of
    /// the same length, unless `compare_from`/`compare_to` name another.
    baseline: (i64, i64),
}

const MAX_SPAN_MS: i64 = 90 * 86_400_000;
const DEFAULT_SPAN_MS: i64 = 86_400_000;
const MODES: [&str; 5] = ["aggressive", "lossless", "recoverable", "off", "unrecorded"];
const SURFACES: [&str; 4] = ["api", "mcp", "proxy", "cli"];

/// Every rule the analysis routes apply to a query, with no I/O, so each can
/// be tested on its own. `now` is the default end of the range.
fn plan(
    q: AnalysisQuery,
    headers: &HeaderMap,
    listener: bool,
    view: View,
    now: i64,
) -> Result<Plan, ApiError> {
    let format = match q.format.as_deref() {
        None if listener => Format::Html,
        format => Format::parse(format, headers)?,
    };
    let to = q.to.unwrap_or(now);
    let from = q.from.unwrap_or_else(|| to.saturating_sub(DEFAULT_SPAN_MS));
    if from < 0 || to <= from || to.saturating_sub(from) > MAX_SPAN_MS {
        return Err(ApiError::Invalid(
            "from/to must be epoch milliseconds with 0 <= from < to, spanning at most 90 days"
                .into(),
        ));
    }
    if let Some(c) = &q.conversation
        && (c.len() != 16
            || !c
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    {
        return Err(ApiError::Invalid(
            "conversation must be 16 lowercase hex digits".into(),
        ));
    }
    let filter = kompressor_stats::history::HistoryFilter {
        model: q.model,
        mode: q.mode,
        surface: q.surface,
        experiment_id: q.experiment_id,
        run_id: q.run_id,
    };
    if filter.mode.as_deref().is_some_and(|m| !MODES.contains(&m))
        || filter
            .surface
            .as_deref()
            .is_some_and(|s| !SURFACES.contains(&s))
    {
        return Err(ApiError::Invalid("unsupported mode or surface".into()));
    }
    if view != View::Comparison
        && (q.compare_from.is_some()
            || q.compare_to.is_some()
            || filter.model.is_some()
            || filter.mode.is_some()
            || filter.surface.is_some()
            || filter.experiment_id.is_some()
            || filter.run_id.is_some())
    {
        return Err(ApiError::Invalid(
            "comparison dates and cohort filters are supported only at /stats/analysis/compare"
                .into(),
        ));
    }
    let baseline = match (q.compare_from, q.compare_to) {
        (None, None) => (from.saturating_sub(to - from).max(0), from),
        (Some(f), Some(t)) if f >= 0 && t > f && t.saturating_sub(f) <= MAX_SPAN_MS => (f, t),
        _ => {
            return Err(ApiError::Invalid(
                "compare_from/compare_to must be a valid pair spanning at most 90 days".into(),
            ));
        }
    };
    if view == View::Comparison && baseline.1 <= baseline.0 {
        return Err(ApiError::Invalid(
            "choose a comparison window before or after this range".into(),
        ));
    }
    if view != View::Overview && q.conversation.is_some() {
        return Err(ApiError::Invalid(
            "conversation detail is available only on the overview".into(),
        ));
    }
    if format == Format::Html && view != View::Overview {
        return Err(ApiError::Invalid(
            "diagnostics serve JSON; open /stats/analysis?format=html for the dashboard".into(),
        ));
    }
    Ok(Plan {
        format,
        tenant: q.tenant,
        from,
        to,
        conversation: q.conversation,
        filter,
        baseline,
    })
}

async fn respond(
    state: Arc<ServerState>,
    headers: HeaderMap,
    query: Result<Query<AnalysisQuery>, QueryRejection>,
    listener: bool,
    view: View,
) -> Result<Response, ApiError> {
    let Query(q) = query.map_err(|e| ApiError::Invalid(format!("query: {}", e.body_text())))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let Plan {
        format,
        tenant,
        from,
        to,
        conversation,
        filter,
        baseline,
    } = plan(q, &headers, listener, view, now)?;
    if format == Format::Html {
        let settings = serde_json::json!({"endpoint": if listener {"/v1/stats/analysis"} else {""}, "listener": listener, "refs": state.core.config().refs.browse,
            "from":from,"to":to,"conversation":conversation}).to_string().replace('<', "\\u003c");
        return Ok((
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            include_str!("analysis.html").replace("/*SETTINGS*/{}", &settings),
        )
            .into_response());
    }
    let identity = authenticate(&state, &headers)?;
    let tenant = match tenant {
        None => identity.tenant.as_str().to_owned(),
        Some(t) if identity.trusted || t == identity.tenant.as_str() => t,
        Some(_) => return Err(ApiError::Forbidden),
    };
    let respond_with = |data: &serde_json::Value| match format {
        Format::Jsonl => jsonl_response(data, export_name(view), now),
        _ => json_response(data),
    };
    if view != View::Overview {
        let report_tenant = tenant.clone();
        let mut data = state
            .with_stats(move |r| match view {
                View::Retrieval => serde_json::to_value(r.retrieval_analysis(&tenant, from, to)?)
                    .map_err(Into::into),
                View::Latency => {
                    serde_json::to_value(r.latency_analysis(&tenant, from, to)?).map_err(Into::into)
                }
                View::Comparison => serde_json::to_value(r.compare_history(
                    &tenant,
                    (from, to),
                    baseline,
                    &filter,
                )?)
                .map_err(Into::into),
                View::Overview => unreachable!(),
            })
            .await?;
        if let Some(obj) = data.as_object_mut() {
            obj.insert("tenant".into(), serde_json::json!(report_tenant));
            obj.insert("from_ms".into(), serde_json::json!(from));
            obj.insert("to_ms".into(), serde_json::json!(to));
            obj.insert(
                "stats_dropped".into(),
                serde_json::json!(state.core.stats().dropped()),
            );
        }
        return Ok(respond_with(&data));
    }
    let report = state
        .with_stats(move |r| r.analysis(&tenant, from, to, conversation.as_deref()))
        .await?;
    Ok(respond_with(
        &serde_json::json!({"report":report,"stats_dropped":state.core.stats().dropped(),"retention_days":state.core.config().stats.retention_days}),
    ))
}

/// The download's file name stem, one per view.
fn export_name(view: View) -> &'static str {
    match view {
        View::Overview => "analysis",
        View::Retrieval => "analysis-retrievals",
        View::Latency => "analysis-latency",
        View::Comparison => "analysis-compare",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    const NOW: i64 = 1_800_000_000_000;
    const DAY: i64 = 86_400_000;
    const ALL: [View; 4] = [
        View::Overview,
        View::Retrieval,
        View::Latency,
        View::Comparison,
    ];

    fn q(tweak: impl FnOnce(&mut AnalysisQuery)) -> AnalysisQuery {
        let mut query = AnalysisQuery::default();
        tweak(&mut query);
        query
    }

    fn ok(query: AnalysisQuery, view: View) -> Plan {
        plan(query, &HeaderMap::new(), false, view, NOW).expect("a valid query")
    }

    fn refused(query: AnalysisQuery, view: View) -> String {
        match plan(query, &HeaderMap::new(), false, view, NOW) {
            Err(ApiError::Invalid(message)) => message,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn the_range_defaults_to_the_last_day_and_baseline_to_the_day_before() {
        let p = ok(q(|_| {}), View::Overview);
        assert_eq!((p.from, p.to), (NOW - DAY, NOW));
        assert_eq!(p.baseline, (NOW - 2 * DAY, NOW - DAY));
        assert_eq!(p.format, Format::Json, "automation gets JSON by default");
        assert_eq!(p.tenant, None);

        let p = ok(q(|q| q.to = Some(5 * DAY)), View::Overview);
        assert_eq!((p.from, p.to), (4 * DAY, 5 * DAY), "from follows to");
        let p = ok(q(|q| q.from = Some(NOW - 3 * DAY)), View::Overview);
        assert_eq!((p.from, p.to), (NOW - 3 * DAY, NOW), "to defaults to now");
    }

    #[test]
    fn a_range_must_be_ordered_non_negative_and_at_most_ninety_days() {
        for (from, to) in [(20, 10), (10, 10), (-1, 10), (0, 90 * DAY + 1)] {
            let message = refused(
                q(|q| {
                    q.from = Some(from);
                    q.to = Some(to);
                }),
                View::Overview,
            );
            assert!(message.contains("from/to"), "{from}..{to}: {message}");
        }
        let p = ok(
            q(|q| {
                q.from = Some(0);
                q.to = Some(90 * DAY);
            }),
            View::Overview,
        );
        assert_eq!(p.to - p.from, 90 * DAY, "exactly ninety days is allowed");
    }

    #[test]
    fn a_conversation_is_sixteen_lowercase_hex_digits_and_only_on_the_overview() {
        for bad in [
            "",
            "abc",
            "0123456789ABCDEF",
            "0123456789abcdeg",
            "0123456789abcdef0",
        ] {
            let message = refused(q(|q| q.conversation = Some(bad.into())), View::Overview);
            assert!(message.contains("16 lowercase hex"), "{bad:?}");
        }
        let id = "0123456789abcdef";
        let p = ok(q(|q| q.conversation = Some(id.into())), View::Overview);
        assert_eq!(p.conversation.as_deref(), Some(id));
        for view in [View::Retrieval, View::Latency, View::Comparison] {
            let message = refused(q(|q| q.conversation = Some(id.into())), view);
            assert!(message.contains("only on the overview"), "{view:?}");
        }
    }

    #[test]
    fn cohort_filters_and_compare_dates_belong_to_compare_alone() {
        let tweaks: [fn(&mut AnalysisQuery); 7] = [
            |q| q.model = Some("claude".into()),
            |q| q.mode = Some("lossless".into()),
            |q| q.surface = Some("proxy".into()),
            |q| q.experiment_id = Some("e".into()),
            |q| q.run_id = Some("r".into()),
            |q| q.compare_from = Some(0),
            |q| q.compare_to = Some(1),
        ];
        for tweak in tweaks {
            for view in [View::Overview, View::Retrieval, View::Latency] {
                let message = refused(q(tweak), view);
                assert!(message.contains("only at /stats/analysis/compare"));
            }
        }
        let p = ok(
            q(|q| {
                q.model = Some("claude".into());
                q.mode = Some("recoverable".into());
                q.surface = Some("mcp".into());
                q.experiment_id = Some("exp".into());
                q.run_id = Some("run".into());
            }),
            View::Comparison,
        );
        assert_eq!(p.filter.model.as_deref(), Some("claude"));
        assert_eq!(p.filter.mode.as_deref(), Some("recoverable"));
        assert_eq!(p.filter.surface.as_deref(), Some("mcp"));
        assert_eq!(p.filter.experiment_id.as_deref(), Some("exp"));
        assert_eq!(p.filter.run_id.as_deref(), Some("run"));
    }

    #[test]
    fn modes_and_surfaces_are_checked_against_what_is_recorded() {
        for mode in MODES {
            ok(q(|q| q.mode = Some(mode.into())), View::Comparison);
        }
        for surface in SURFACES {
            ok(q(|q| q.surface = Some(surface.into())), View::Comparison);
        }
        refused(q(|q| q.mode = Some("turbo".into())), View::Comparison);
        refused(q(|q| q.surface = Some("web".into())), View::Comparison);
    }

    #[test]
    fn a_named_baseline_must_be_a_whole_valid_pair() {
        let p = ok(
            q(|q| {
                q.compare_from = Some(10);
                q.compare_to = Some(20);
            }),
            View::Comparison,
        );
        assert_eq!(p.baseline, (10, 20));
        for (f, t) in [
            (Some(10), None),
            (None, Some(10)),
            (Some(20), Some(10)),
            (Some(-1), Some(10)),
            (Some(0), Some(90 * DAY + 1)),
        ] {
            let message = refused(
                q(|q| {
                    q.compare_from = f;
                    q.compare_to = t;
                }),
                View::Comparison,
            );
            assert!(message.contains("compare_from/compare_to"), "{f:?}..{t:?}");
        }
        // A range starting at zero has no room for a default baseline before
        // it, and one that starts nearer than its own length gets a shorter one.
        let message = refused(
            q(|q| {
                q.from = Some(0);
                q.to = Some(DAY);
            }),
            View::Comparison,
        );
        assert!(message.contains("comparison window"));
        let p = ok(
            q(|q| {
                q.from = Some(DAY / 2);
                q.to = Some(DAY);
            }),
            View::Comparison,
        );
        assert_eq!(p.baseline, (0, DAY / 2));
        let p = ok(
            q(|q| {
                q.from = Some(DAY / 4);
                q.to = Some(DAY);
            }),
            View::Comparison,
        );
        assert_eq!(p.baseline, (0, DAY / 4), "never before the epoch");
    }

    #[test]
    fn every_view_serves_json_and_jsonl_but_only_the_overview_has_a_page() {
        for view in ALL {
            for (format, expected) in [("json", Format::Json), ("jsonl", Format::Jsonl)] {
                let p = ok(q(|q| q.format = Some(format.into())), view);
                assert_eq!(p.format, expected, "{view:?} {format}");
            }
            let html = q(|q| q.format = Some("html".into()));
            if view == View::Overview {
                assert_eq!(ok(html, view).format, Format::Html);
            } else {
                assert!(refused(html, view).contains("diagnostics serve JSON"));
            }
            assert!(refused(q(|q| q.format = Some("csv".into())), view).contains("format must be"));
        }
    }

    #[test]
    fn the_accept_header_decides_only_when_no_format_is_named() {
        let mut browser = HeaderMap::new();
        browser.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/html,*/*;q=0.8"),
        );
        let mut client = HeaderMap::new();
        client.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        let format = |query, headers: &HeaderMap, listener| {
            plan(query, headers, listener, View::Overview, NOW)
                .unwrap()
                .format
        };
        assert_eq!(format(q(|_| {}), &browser, false), Format::Html);
        assert_eq!(format(q(|_| {}), &client, false), Format::Json);
        assert_eq!(
            format(q(|_| {}), &client, true),
            Format::Html,
            "the page at /analysis"
        );
        let json = || q(|q| q.format = Some("json".into()));
        assert_eq!(format(json(), &browser, false), Format::Json);
        assert_eq!(format(json(), &client, true), Format::Json);
    }

    #[test]
    fn a_named_tenant_is_carried_through_for_the_handler_to_authorise() {
        let p = ok(q(|q| q.tenant = Some("team-b".into())), View::Latency);
        assert_eq!(p.tenant.as_deref(), Some("team-b"));
    }

    #[test]
    fn each_view_downloads_under_its_own_name() {
        let names: Vec<_> = ALL.iter().map(|v| export_name(*v)).collect();
        assert_eq!(
            names,
            [
                "analysis",
                "analysis-retrievals",
                "analysis-latency",
                "analysis-compare"
            ]
        );
    }
}
