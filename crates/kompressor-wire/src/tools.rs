//! Recovering what a tool output was the output *of*.
//!
//! The engine's read lifecycle can only notice that a file read was invalidated
//! by a later edit if each tool result says which tool ran and on which file.
//! The wire puts that information in the *call*, several messages earlier, and
//! links the two by id. Resolving that link here is what makes the whole
//! feature reachable through a proxy — the Python plugin never sent the
//! metadata, so the classifier it fed had nothing to classify.
//!
//! Every lookup is by exact id. An output whose call is not in this request
//! keeps `None` for both fields: a wrong tool name produces a wrong staleness
//! verdict, which replaces content the model still needs.

use std::collections::BTreeMap;

use serde_json::Value;

/// A tool call seen in the same request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToolCall {
    /// The tool's name, as the caller spelled it.
    pub(crate) name: String,
    /// The file the call names, when its arguments carry one.
    pub(crate) file_path: Option<String>,
    /// The lines a read asked for; see [`read_range_in_arguments`].
    pub(crate) read_range: Option<(u64, u64)>,
}

/// How many lines a read returns when the call names no limit. Claude Code's
/// `Read` stops there, and a call that names neither offset nor limit is then
/// a read of lines 1 to 2000 — the whole file, for nearly every file.
pub(crate) const DEFAULT_READ_LINES: u64 = 2_000;

/// The lines a file read asked for, as `[first, past)` with lines from 1.
///
/// Understood only for a call whose arguments are a path and, optionally, a
/// numeric `offset` and `limit` — the shape Claude Code's `Read` has. Any
/// other argument (a page range, a byte count, a pattern) means the result is
/// some other selection of the file, and the answer is `None`: a range that
/// is guessed wrong makes one read look as if it contained another.
pub(crate) fn read_range_in_arguments(arguments: &Value) -> Option<(u64, u64)> {
    let object = arguments.as_object()?;
    let mut offset = 1;
    let mut limit = DEFAULT_READ_LINES;
    for (key, value) in object {
        match key.as_str() {
            "file_path" | "path" => {}
            "offset" => offset = value.as_u64()?.max(1),
            "limit" => limit = value.as_u64().filter(|limit| *limit > 0)?,
            _ => return None,
        }
    }
    Some((offset, offset.checked_add(limit)?))
}

/// Calls indexed by the id their output will quote.
pub(crate) type ToolCallIndex = BTreeMap<String, ToolCall>;

/// The file an invocation names, from already-parsed arguments.
///
/// `file_path` is checked before `path` only to be deterministic when a tool
/// declares both; no harness in practice does.
pub(crate) fn file_path_in_arguments(arguments: &Value) -> Option<String> {
    let object = arguments.as_object()?;
    for key in ["file_path", "path"] {
        if let Some(path) = object.get(key).and_then(Value::as_str)
            && !path.is_empty()
        {
            return Some(path.to_string());
        }
    }
    None
}

/// The file an invocation names, from OpenAI's arguments — a JSON *string*
/// rather than an object, and one a model can emit malformed, in which case
/// there is nothing to recover and nothing is guessed.
pub(crate) fn file_path_in_json_arguments(arguments: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(arguments).ok()?;
    file_path_in_arguments(&parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_read_range_is_known_only_for_a_call_it_fully_understands() {
        let range = |arguments: Value| read_range_in_arguments(&arguments);
        assert_eq!(range(json!({"file_path": "a.rs"})), Some((1, 2_001)));
        assert_eq!(
            range(json!({"file_path": "a.rs", "offset": 40, "limit": 20})),
            Some((40, 60))
        );
        assert_eq!(range(json!({"path": "a.rs", "limit": 100})), Some((1, 101)));
        assert_eq!(
            range(json!({"path": "a.rs", "offset": 0})),
            Some((1, 2_001))
        );
        // Another selection of the file, or one that cannot be read as lines.
        assert_eq!(range(json!({"file_path": "a.pdf", "pages": "1-3"})), None);
        assert_eq!(range(json!({"file_path": "a.rs", "offset": "40"})), None);
        assert_eq!(range(json!({"file_path": "a.rs", "limit": 0})), None);
        assert_eq!(range(json!({"file_path": "a.rs", "limit": u64::MAX})), None);
        assert_eq!(range(json!("a.rs")), None);
    }

    #[test]
    fn either_spelling_of_the_path_argument_is_recovered() {
        assert_eq!(
            file_path_in_arguments(&json!({"file_path": "src/app.rs"})),
            Some("src/app.rs".to_string())
        );
        assert_eq!(
            file_path_in_arguments(&json!({"path": "src/app.rs", "limit": 100})),
            Some("src/app.rs".to_string())
        );
    }

    #[test]
    fn arguments_naming_no_file_yield_none() {
        assert_eq!(file_path_in_arguments(&json!({"command": "ls -la"})), None);
        assert_eq!(file_path_in_arguments(&json!({"path": ""})), None);
        assert_eq!(file_path_in_arguments(&json!("not an object")), None);
    }

    #[test]
    fn malformed_json_arguments_yield_none_rather_than_a_guess() {
        assert_eq!(
            file_path_in_json_arguments("{\"path\": \"a.rs\"}"),
            Some("a.rs".to_string())
        );
        assert_eq!(file_path_in_json_arguments("{\"path\": \"a.rs"), None);
    }
}
