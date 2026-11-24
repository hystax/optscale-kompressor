//! JSON Lines downloads of the reporting dashboards.
//!
//! `?format=jsonl` on `/v1/stats/live`, `/v1/stats/cache` and every
//! `/v1/stats/analysis` route answers with the same report the JSON form
//! carries, laid out one record per line so it can be loaded into a
//! spreadsheet, `jq -c`, DuckDB or pandas without first picking the report
//! apart:
//!
//! - the first line is `{"record":"summary", ...}`: every field of the report
//!   that is not a list of rows, nested objects kept as they are;
//! - every element of every list follows as a line of its own, tagged with the
//!   dotted path of the list it came from — `{"record":"timeline", ...}`,
//!   `{"record":"report.conversations", ...}`. An element that is not an object
//!   is carried as `{"record":"<path>","value":<element>}`.
//!
//! Lists are found by walking objects only; a list inside a list element stays
//! inside that element's line. The response is an attachment, so a browser
//! saves it rather than showing it.

use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Map, Value};

/// The media type of a JSON Lines body.
pub const CONTENT_TYPE: &str = "application/x-ndjson";

/// The key every line carries to say which part of the report it is.
pub const RECORD_KEY: &str = "record";

/// Lay `value` out as JSON Lines; see the module documentation for the shape.
pub fn to_jsonl(value: &Value) -> String {
    let mut lines = Vec::new();
    let summary = match value {
        Value::Object(object) => split(object, "", &mut lines),
        other => {
            let mut summary = Map::new();
            summary.insert("value".into(), other.clone());
            summary
        }
    };
    let mut head = Map::new();
    head.insert(RECORD_KEY.into(), Value::String("summary".into()));
    head.extend(summary);

    let mut out = String::new();
    for line in std::iter::once(Value::Object(head)).chain(lines) {
        // A `Value` always serializes, and never with a raw newline in it.
        out.push_str(&serde_json::to_string(&line).unwrap_or_default());
        out.push('\n');
    }
    out
}

/// What is left of `object` once its lists are moved out into `lines`.
fn split(object: &Map<String, Value>, prefix: &str, lines: &mut Vec<Value>) -> Map<String, Value> {
    let mut rest = Map::new();
    for (key, value) in object {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match value {
            Value::Array(items) => {
                for item in items {
                    let mut line = Map::new();
                    line.insert(RECORD_KEY.into(), Value::String(path.clone()));
                    match item {
                        Value::Object(fields) => {
                            for (k, v) in fields {
                                // The tag wins; a field of the same name is
                                // kept under a name that cannot collide.
                                let k = if k == RECORD_KEY {
                                    format!("{RECORD_KEY}_field")
                                } else {
                                    k.clone()
                                };
                                line.insert(k, v.clone());
                            }
                        }
                        other => {
                            line.insert("value".into(), other.clone());
                        }
                    }
                    lines.push(Value::Object(line));
                }
            }
            Value::Object(inner) => {
                rest.insert(key.clone(), Value::Object(split(inner, &path, lines)));
            }
            other => {
                rest.insert(key.clone(), other.clone());
            }
        }
    }
    rest
}

/// A JSON Lines attachment named `<name>-<generated_at_ms>.jsonl`.
pub(crate) fn jsonl_response<T: Serialize>(
    value: &T,
    name: &str,
    generated_at_ms: i64,
) -> Response {
    let value = match serde_json::to_value(value) {
        Ok(value) => value,
        Err(error) => {
            return crate::ApiError::Internal(format!("could not render response: {error}"))
                .into_response();
        }
    };
    let disposition = format!("attachment; filename=\"kompressor-{name}-{generated_at_ms}.jsonl\"");
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE)),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&disposition)
                    .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
            ),
        ],
        to_jsonl(&value),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lines(value: Value) -> Vec<Value> {
        to_jsonl(&value)
            .lines()
            .map(|line| serde_json::from_str(line).expect("every line is JSON"))
            .collect()
    }

    #[test]
    fn the_summary_comes_first_and_every_list_element_gets_a_line() {
        let out = lines(json!({
            "tenant": "a",
            "totals": {"requests": 3, "by_hour": [{"h": 1}, {"h": 2}]},
            "timeline": [{"t": 1, "n": 2}, {"t": 2, "n": 0}],
            "empty": [],
            "tags": ["x", 7],
            "rows": [{"record": "clash"}],
        }));
        assert_eq!(
            out[0],
            json!({"record": "summary", "tenant": "a", "totals": {"requests": 3}})
        );
        assert_eq!(
            &out[1..],
            &[
                json!({"record": "totals.by_hour", "h": 1}),
                json!({"record": "totals.by_hour", "h": 2}),
                json!({"record": "timeline", "t": 1, "n": 2}),
                json!({"record": "timeline", "t": 2, "n": 0}),
                json!({"record": "tags", "value": "x"}),
                json!({"record": "tags", "value": 7}),
                json!({"record": "rows", "record_field": "clash"}),
            ]
        );
    }

    #[test]
    fn every_line_ends_with_a_newline_and_holds_no_other() {
        let text = to_jsonl(&json!({"note": "two\nlines", "xs": [{"s": "a\nb"}]}));
        assert!(text.ends_with('\n'));
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn a_report_that_is_not_an_object_is_still_one_summary_line() {
        assert_eq!(
            lines(json!(5)),
            vec![json!({"record": "summary", "value": 5})]
        );
    }
}
