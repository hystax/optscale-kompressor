//! Reading a retained fragment back, in whole or in part.
//!
//! Retrieval is the other half of recoverable mode, and it has a cost the
//! compression side does not: whatever comes back is re-injected into the next
//! request and paid for again. A model that asks for a 28 KB fragment ten times
//! can spend more than the compression ever saved. So a retrieval can be
//! narrowed — a line window, a JSON pointer, a pattern — and whatever is not
//! narrowed is capped, with the response saying plainly that it was.

use serde::{Deserialize, Serialize};

/// Which part of a fragment the caller wants.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RetrieveRange {
    /// A 1-based inclusive line window, the way an editor addresses a file.
    Lines { start: usize, end: usize },
    /// An RFC 6901 pointer into a fragment that is JSON.
    JsonPointer { pointer: String },
    /// A character window, snapped outward to character boundaries.
    Chars { start: usize, end: usize },
    /// Lines matching a literal, with surrounding context.
    Grep {
        pattern: String,
        #[serde(default = "default_context")]
        context: usize,
    },
}

fn default_context() -> usize {
    2
}

/// How a range arrived: exactly as asked for, salvaged, or not at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeParse {
    /// The caller sent one variant and it deserialized.
    Exact,
    /// The caller sent several variants; one was chosen. Carries how many were
    /// on offer, for the log line that says so.
    Salvaged { offered: usize },
    /// The caller sent one window in a shape the schema does not have, but
    /// which can mean only one thing — `{"lines": 1, "end": 60}`,
    /// `{"lines": [1, 60]}`, `{"pattern": "error"}` — and it was read as that.
    Normalized,
    /// Nothing usable. The caller gets the whole fragment, capped, and is told
    /// the narrowing failed rather than left to think it asked for everything.
    Unusable,
}

impl RangeParse {
    /// The label a response and a retrieval event carry: whether the window
    /// the caller asked for is the window it got.
    pub fn as_str(self) -> &'static str {
        match self {
            RangeParse::Exact => "exact",
            RangeParse::Salvaged { .. } => "salvaged",
            RangeParse::Normalized => "normalized",
            RangeParse::Unusable => "unusable",
        }
    }
}

/// The order a salvage prefers variants in, most specific first.
///
/// `lines` beats `chars` because a model that gives both has almost always
/// meant the line window — character offsets into a fragment it has never seen
/// whole are a guess, line numbers in a marker are not. `grep` comes last
/// because it is the only one whose cost does not shrink with the argument: a
/// pattern can match everything.
const VARIANTS: [&str; 4] = ["lines", "chars", "json_pointer", "grep"];

impl RetrieveRange {
    /// Read a range the way a model actually sends one.
    ///
    /// The strict enum is tried first and is the only path that can accept an
    /// unusual shape. What it cannot read is an object naming *several*
    /// variants at once, and that is not hypothetical: offered four optional
    /// sibling objects, a real model filled in all four —
    /// `{"lines":{"start":1,"end":80},"chars":{"start":0,"end":0},"json_pointer":{"pointer":""},"grep":{"pattern":"","context":0}}`.
    /// Externally tagged enums need exactly one key, so that deserialized to
    /// nothing and the caller fell back to the whole fragment. Across two paid
    /// benchmark runs every single retrieval came back unnarrowed for this
    /// reason, and the only record of it was a `debug!` nobody captured.
    ///
    /// So: drop the members that are plainly filler — a zero-width char window,
    /// an empty pointer, an empty pattern — and if exactly one substantive
    /// variant is left, use it. On the payload above that leaves `lines`, which
    /// is what the model meant. Only a genuinely ambiguous request falls back
    /// to `VARIANTS` order, and one with nothing substantive left is
    /// [`RangeParse::Unusable`].
    pub fn parse_loose(value: &serde_json::Value) -> (Option<RetrieveRange>, RangeParse) {
        if let Ok(range) = serde_json::from_value::<RetrieveRange>(value.clone()) {
            return (Some(range), RangeParse::Exact);
        }
        let Some(object) = value.as_object() else {
            return (None, RangeParse::Unusable);
        };
        if let Some(range) = normalize(object) {
            return (Some(range), RangeParse::Normalized);
        }
        let offered = object.len();
        let mut substantive: Vec<&str> = Vec::new();
        for name in VARIANTS {
            let Some(member) = object.get(name) else {
                continue;
            };
            if is_filler(name, member) {
                continue;
            }
            substantive.push(name);
        }
        for name in &substantive {
            let mut single = serde_json::Map::new();
            single.insert((*name).to_string(), object[*name].clone());
            if let Ok(range) =
                serde_json::from_value::<RetrieveRange>(serde_json::Value::Object(single))
            {
                return (Some(range), RangeParse::Salvaged { offered });
            }
        }
        (None, RangeParse::Unusable)
    }
}

/// Read the flat shapes a model sends when it skips the inner object.
///
/// Recorded on the 2026-11-10 Harbor run: `{"lines":1,"end":60}`, logged as
/// unusable and answered with the whole fragment. Each shape accepted here has
/// exactly one reading — one variant named, its window given as a number plus
/// a sibling `end`, a two-element array, or an `"a-b"` string; or the one
/// field of a variant (`pattern`, `pointer`) given without the variant around
/// it. Anything with two readings stays unusable: `{"lines": 60}` could be the
/// first sixty lines or line sixty, and a bare `{"start", "end"}` could count
/// lines or characters.
fn normalize(object: &serde_json::Map<String, serde_json::Value>) -> Option<RetrieveRange> {
    use serde_json::Value;
    let number = |value: &Value| value.as_u64().and_then(|n| usize::try_from(n).ok());
    let window = |value: &Value, end: Option<&Value>| -> Option<(usize, usize)> {
        match (value, end) {
            (Value::Number(_), Some(end)) => Some((number(value)?, number(end)?)),
            (Value::Array(pair), None) if pair.len() == 2 => {
                Some((number(&pair[0])?, number(&pair[1])?))
            }
            (Value::String(text), None) => {
                let (start, end) = text.split_once('-')?;
                Some((start.trim().parse().ok()?, end.trim().parse().ok()?))
            }
            _ => None,
        }
    };
    let text = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };

    let named: Vec<&str> = object
        .keys()
        .map(String::as_str)
        .filter(|key| VARIANTS.contains(key))
        .collect();
    let only = |allowed: &[&str]| object.keys().all(|key| allowed.contains(&key.as_str()));
    match named.as_slice() {
        [variant] if only(&[variant, "end"]) => {
            let member = &object[*variant];
            let end = object.get("end");
            match *variant {
                "lines" => window(member, end)
                    .filter(|(start, end)| *start >= 1 && start <= end)
                    .map(|(start, end)| RetrieveRange::Lines { start, end }),
                "chars" => window(member, end)
                    .filter(|(start, end)| start < end)
                    .map(|(start, end)| RetrieveRange::Chars { start, end }),
                "json_pointer" if end.is_none() => {
                    text(Some(member)).map(|pointer| RetrieveRange::JsonPointer { pointer })
                }
                "grep" if end.is_none() => text(Some(member)).map(|pattern| RetrieveRange::Grep {
                    pattern,
                    context: default_context(),
                }),
                _ => None,
            }
        }
        [] if only(&["pattern", "context"]) => {
            let pattern = text(object.get("pattern"))?;
            let context = match object.get("context") {
                Some(value) => number(value)?,
                None => default_context(),
            };
            Some(RetrieveRange::Grep { pattern, context })
        }
        [] if only(&["pointer"]) => {
            text(object.get("pointer")).map(|pointer| RetrieveRange::JsonPointer { pointer })
        }
        _ => None,
    }
}

/// Whether a variant is present but says nothing — the shape a model emits when
/// a schema invited it to fill a field it had no use for.
fn is_filler(name: &str, member: &serde_json::Value) -> bool {
    let get = |key: &str| member.get(key);
    match name {
        // A window that selects nothing cannot have been meant. The two
        // differ in where they start counting: `lines` is 1-based inclusive,
        // so `{1, 1}` is a real request for the first line, while `chars` is
        // 0-based half-open and `{0, 0}` is empty.
        "lines" => {
            let start = get("start").and_then(serde_json::Value::as_u64);
            let end = get("end").and_then(serde_json::Value::as_u64);
            matches!((start, end), (Some(s), Some(e)) if s == 0 || s > e)
        }
        "chars" => {
            let start = get("start").and_then(serde_json::Value::as_u64);
            let end = get("end").and_then(serde_json::Value::as_u64);
            matches!((start, end), (Some(s), Some(e)) if s >= e)
        }
        "json_pointer" => get("pointer")
            .and_then(serde_json::Value::as_str)
            .is_none_or(str::is_empty),
        "grep" => get("pattern")
            .and_then(serde_json::Value::as_str)
            .is_none_or(str::is_empty),
        _ => true,
    }
}

/// The answer to a retrieval.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retrieved {
    pub content: Option<String>,
    pub found: bool,
    /// The fragment was held and its retention window has passed.
    ///
    /// Distinguished from a plain miss so an operator can tell "we let this go"
    /// from "we never had it" — the first is a retention setting to change, the
    /// second is a handle from somewhere else entirely.
    pub expired: bool,
    /// A range was applied, so `content` is part of the fragment.
    pub ranged: bool,
    pub total_lines: usize,
    pub total_chars: usize,
    /// The result was cut to the configured cap; narrow it with a range.
    pub truncated: bool,
    /// Why a range selected nothing, when one did.
    ///
    /// A narrowed retrieval that matches nothing is a *hit* — the handle
    /// resolved, the fragment is there — so [`Retrieved::miss_hint`] never
    /// fires and the caller is handed `found: true` with empty content and no
    /// idea what to do next. Measured cost: a model spent two of its four
    /// retrieval rounds on empty answers, exhausted the budget, and lost an
    /// answer it could otherwise have given. The remedy is to say which way
    /// the range missed and what to try instead.
    ///
    /// Serialized as `hint`, the same key a miss uses: both answer "what do I
    /// do now", the caller does not care which of them it got, and two names
    /// for one idea is how a client ends up handling only the case it happened
    /// to see first.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "hint")]
    pub range_hint: Option<String>,
}

impl Retrieved {
    pub fn miss() -> Retrieved {
        Retrieved {
            content: None,
            found: false,
            expired: false,
            ranged: false,
            total_lines: 0,
            total_chars: 0,
            truncated: false,
            range_hint: None,
        }
    }

    pub fn expired() -> Retrieved {
        Retrieved {
            expired: true,
            ..Retrieved::miss()
        }
    }

    /// What the model should be told when a handle does not resolve.
    ///
    /// It has to be actionable and it has to be honest: the one thing that must
    /// never happen is the model inventing the data it could not fetch.
    pub fn miss_hint(&self) -> &'static str {
        if self.expired {
            "the elided content was cached and has since expired; re-run the command or re-read \
             the file to get it again, and do not reconstruct it from memory"
        } else {
            "no such handle is held for this tenant; it may belong to another conversation. \
             Re-run the command or re-read the file rather than reconstructing the content"
        }
    }
}

/// Apply a range to a fragment and cap the result.
pub fn slice(fragment: &str, range: Option<&RetrieveRange>, max_chars: usize) -> Retrieved {
    let total_chars = fragment.chars().count();
    let total_lines = fragment.lines().count();

    let (selected, ranged) = match range {
        None => (fragment.to_string(), false),
        Some(range) => (apply(fragment, range), true),
    };

    let (content, truncated) = cap(selected, max_chars);
    // Computed here rather than by the caller because this is the only place
    // that still holds both the range and the fragment it was applied to.
    let range_hint = match range {
        Some(range) if content.is_empty() => {
            Some(empty_range_hint(range, total_lines, total_chars))
        }
        _ => None,
    };
    Retrieved {
        content: Some(content),
        found: true,
        expired: false,
        ranged,
        total_lines,
        total_chars,
        truncated,
        range_hint,
    }
}

/// What to tell a caller whose range selected nothing.
///
/// Specific to the variant, because the useful next step differs: a line
/// window that overshot wants the real line count, and a pattern that did not
/// match wants to know the match is literal rather than a regular expression —
/// which is the mistake a model makes when a plain substring fails it.
fn empty_range_hint(range: &RetrieveRange, total_lines: usize, total_chars: usize) -> String {
    match range {
        RetrieveRange::Lines { start, end } => format!(
            "lines {start}-{end} selected nothing; the fragment has {total_lines} lines. Retry \
             with a window inside that, or omit the range to get the whole fragment."
        ),
        RetrieveRange::Chars { start, end } => format!(
            "characters {start}-{end} selected nothing; the fragment has {total_chars} \
             characters. Retry with a window inside that, or omit the range."
        ),
        RetrieveRange::JsonPointer { pointer } => format!(
            "the pointer {pointer:?} resolved to nothing; the fragment may not be JSON, or that \
             path may not exist in it. Omit the range to see the fragment."
        ),
        RetrieveRange::Grep { pattern, .. } => format!(
            "nothing matched {pattern:?}; the pattern is matched literally, not as a regular \
             expression. Try a shorter literal, or omit the range."
        ),
    }
}

fn apply(fragment: &str, range: &RetrieveRange) -> String {
    match range {
        RetrieveRange::Lines { start, end } => {
            let start = (*start).max(1);
            if end < &start {
                return String::new();
            }
            fragment
                .lines()
                .skip(start - 1)
                .take(end - start + 1)
                .collect::<Vec<_>>()
                .join("\n")
        }
        RetrieveRange::Chars { start, end } => {
            if end <= start {
                return String::new();
            }
            fragment
                .chars()
                .skip(*start)
                .take(end - start)
                .collect::<String>()
        }
        RetrieveRange::JsonPointer { pointer } => {
            // A pointer into something that is not JSON is a caller mistake, not
            // a reason to hand back the whole fragment: silently widening the
            // answer is how a narrowed retrieval becomes an expensive one.
            match serde_json::from_str::<serde_json::Value>(fragment) {
                Ok(value) => match value.pointer(pointer) {
                    Some(found) => serde_json::to_string(found).unwrap_or_default(),
                    None => String::new(),
                },
                Err(_) => String::new(),
            }
        }
        RetrieveRange::Grep { pattern, context } => {
            if pattern.is_empty() {
                return String::new();
            }
            let lines: Vec<&str> = fragment.lines().collect();
            let mut keep = vec![false; lines.len()];
            for (index, line) in lines.iter().enumerate() {
                if line.contains(pattern.as_str()) {
                    let low = index.saturating_sub(*context);
                    // Saturating, like the lower bound beside it. `context` is
                    // an unrestricted `usize` off the wire, so `index + context`
                    // overflows on a large valid integer: a debug build panics,
                    // and a release build wraps to a `high` below `low`, which
                    // selects nothing and returns the lines *around* the match
                    // without the matching line itself. A huge context means
                    // "all of it", which is what this now gives.
                    let high = index
                        .saturating_add(*context)
                        .min(lines.len().saturating_sub(1));
                    for slot in keep.iter_mut().take(high + 1).skip(low) {
                        *slot = true;
                    }
                }
            }
            lines
                .iter()
                .zip(&keep)
                .filter(|(_, keep)| **keep)
                .map(|(line, _)| *line)
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}

fn cap(text: String, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text, false);
    }
    let kept: String = text.chars().take(max_chars).collect();
    (kept, true)
}

#[cfg(test)]
mod tests {
    use super::{RangeParse, RetrieveRange};
    use serde_json::json;

    use super::slice;

    const LOG: &str = "alpha one\nbravo two\ncharlie three\ndelta four";

    /// The case that cost a real answer: a `grep` range that matched nothing
    /// came back `found: true` with empty content and no hint, so the model
    /// learned only that it had wasted a round. Two of four rounds went that
    /// way before the budget ran out.
    /// The HTTP surface used to call this `range_hint` while the proxy and the
    /// MCP tool called the same thing `hint`, so a client that handled one had
    /// no reason to look for the other. One name, checked on the wire.
    #[test]
    fn the_hint_is_called_hint_on_the_wire() {
        let missed = slice(
            LOG,
            Some(&RetrieveRange::Grep {
                pattern: "absent".into(),
                context: 0,
            }),
            10_000,
        );
        let json = serde_json::to_value(&missed).unwrap();
        assert!(json.get("hint").is_some(), "serialized as {json}");
        assert!(json.get("range_hint").is_none(), "one name, not two");

        // and it is absent entirely when there is nothing to say
        let fine = slice(LOG, None, 10_000);
        let json = serde_json::to_value(&fine).unwrap();
        assert!(json.get("hint").is_none(), "serialized as {json}");
    }

    #[test]
    fn a_range_that_matches_nothing_says_how_it_missed() {
        let got = slice(
            LOG,
            Some(&RetrieveRange::Grep {
                pattern: "seq=1978".into(),
                context: 2,
            }),
            10_000,
        );
        assert!(got.found, "the handle resolved; this is a hit");
        assert_eq!(got.content.as_deref(), Some(""));
        let hint = got
            .range_hint
            .expect("an empty range has to explain itself");
        assert!(hint.contains("seq=1978"), "{hint}");
        assert!(
            hint.contains("literally"),
            "a model that sent a regex needs to be told it is not one: {hint}"
        );
        assert!(hint.contains("omit the range"), "{hint}");
    }

    #[test]
    fn each_variant_explains_its_own_miss() {
        let lines = slice(
            LOG,
            Some(&RetrieveRange::Lines { start: 90, end: 99 }),
            10_000,
        );
        let hint = lines.range_hint.expect("a window past the end misses");
        assert!(hint.contains("90-99") && hint.contains("4 lines"), "{hint}");

        let chars = slice(
            LOG,
            Some(&RetrieveRange::Chars {
                start: 9_000,
                end: 9_100,
            }),
            10_000,
        );
        assert!(chars.range_hint.is_some_and(|h| h.contains("characters")));

        let pointer = slice(
            LOG,
            Some(&RetrieveRange::JsonPointer {
                pointer: "/nope".into(),
            }),
            10_000,
        );
        assert!(
            pointer
                .range_hint
                .is_some_and(|h| h.contains("not be JSON"))
        );
    }

    #[test]
    fn a_range_that_selects_something_carries_no_hint() {
        let got = slice(
            LOG,
            Some(&RetrieveRange::Grep {
                pattern: "bravo".into(),
                context: 0,
            }),
            10_000,
        );
        assert_eq!(got.content.as_deref(), Some("bravo two"));
        assert_eq!(got.range_hint, None, "nothing went wrong, so say nothing");
    }

    #[test]
    fn an_unranged_retrieval_never_carries_a_range_hint() {
        let whole = slice(LOG, None, 10_000);
        assert_eq!(whole.range_hint, None);
        assert!(!whole.ranged);
        // and a miss keeps using the miss hint, which is a different thing
        assert_eq!(super::Retrieved::miss().range_hint, None);
    }

    #[test]
    fn a_single_variant_parses_exactly() {
        let (range, how) = RetrieveRange::parse_loose(&json!({"lines": {"start": 1, "end": 80}}));
        assert_eq!(range, Some(RetrieveRange::Lines { start: 1, end: 80 }));
        assert_eq!(how, RangeParse::Exact);
    }

    /// The literal argument a model sent on 2026-09-18, which used to
    /// deserialize to nothing at all.
    #[test]
    fn the_four_variant_payload_salvages_the_line_window() {
        let (range, how) = RetrieveRange::parse_loose(&json!({
            "lines": {"start": 1, "end": 80},
            "chars": {"start": 0, "end": 0},
            "json_pointer": {"pointer": ""},
            "grep": {"pattern": "", "context": 0}
        }));
        assert_eq!(range, Some(RetrieveRange::Lines { start: 1, end: 80 }));
        assert_eq!(how, RangeParse::Salvaged { offered: 4 });
    }

    #[test]
    fn filler_only_is_unusable_rather_than_a_wrong_window() {
        let (range, how) = RetrieveRange::parse_loose(&json!({
            "chars": {"start": 0, "end": 0},
            "json_pointer": {"pointer": ""},
            "grep": {"pattern": ""}
        }));
        assert_eq!(range, None);
        assert_eq!(how, RangeParse::Unusable);
    }

    /// A one-line window is a real request, not filler: `lines` counts from 1
    /// inclusive, so `{5, 5}` is line five.
    #[test]
    fn a_single_line_window_survives_the_filler_test() {
        let (range, how) = RetrieveRange::parse_loose(&json!({
            "lines": {"start": 5, "end": 5},
            "grep": {"pattern": ""}
        }));
        assert_eq!(range, Some(RetrieveRange::Lines { start: 5, end: 5 }));
        assert_eq!(how, RangeParse::Salvaged { offered: 2 });
    }

    /// Two variants that both say something is the only genuinely ambiguous
    /// case, and it resolves by the documented priority rather than by whatever
    /// order the map happened to iterate in.
    #[test]
    fn a_genuinely_ambiguous_request_resolves_deterministically() {
        for _ in 0..8 {
            let (range, how) = RetrieveRange::parse_loose(&json!({
                "grep": {"pattern": "needle", "context": 1},
                "chars": {"start": 10, "end": 20}
            }));
            assert_eq!(range, Some(RetrieveRange::Chars { start: 10, end: 20 }));
            assert_eq!(how, RangeParse::Salvaged { offered: 2 });
        }
    }

    /// The argument recorded on the 2026-11-10 Harbor run (`08f3d50`), logged
    /// as unusable and answered with the whole fragment.
    #[test]
    fn the_recorded_flat_line_window_is_read_as_the_window_it_names() {
        let (range, how) = RetrieveRange::parse_loose(&json!({"lines": 1, "end": 60}));
        assert_eq!(range, Some(RetrieveRange::Lines { start: 1, end: 60 }));
        assert_eq!(how, RangeParse::Normalized);
        assert_eq!(how.as_str(), "normalized");
    }

    #[test]
    fn other_shapes_with_one_reading_are_normalized() {
        for (value, want) in [
            (
                json!({"lines": [3, 9]}),
                RetrieveRange::Lines { start: 3, end: 9 },
            ),
            (
                json!({"lines": "3-9"}),
                RetrieveRange::Lines { start: 3, end: 9 },
            ),
            (
                json!({"chars": 0, "end": 500}),
                RetrieveRange::Chars { start: 0, end: 500 },
            ),
            (
                json!({"chars": [10, 20]}),
                RetrieveRange::Chars { start: 10, end: 20 },
            ),
            (
                json!({"grep": "error"}),
                RetrieveRange::Grep {
                    pattern: "error".into(),
                    context: 2,
                },
            ),
            (
                json!({"pattern": "error", "context": 0}),
                RetrieveRange::Grep {
                    pattern: "error".into(),
                    context: 0,
                },
            ),
            (
                json!({"pointer": "/items/0"}),
                RetrieveRange::JsonPointer {
                    pointer: "/items/0".into(),
                },
            ),
            (
                json!({"json_pointer": "/items/0"}),
                RetrieveRange::JsonPointer {
                    pointer: "/items/0".into(),
                },
            ),
        ] {
            let (range, how) = RetrieveRange::parse_loose(&value);
            assert_eq!(range, Some(want), "{value}");
            assert_eq!(how, RangeParse::Normalized, "{value}");
        }
    }

    /// Two readings is no reading: these stay unusable, and the caller is
    /// told so, rather than handed whichever window a guess picked.
    #[test]
    fn shapes_with_more_than_one_reading_stay_unusable() {
        for value in [
            json!({"lines": 60}),                   // the first 60, or line 60?
            json!({"start": 1, "end": 60}),         // lines or characters?
            json!({"lines": 1, "end": 60, "x": 1}), // and what is x?
            json!({"lines": 0, "end": 60}),         // lines count from 1
            json!({"lines": 60, "end": 1}),         // backwards
            json!({"chars": 5, "end": 5}),          // empty
            json!({"grep": ""}),
            json!({"pointer": ""}),
        ] {
            let (range, how) = RetrieveRange::parse_loose(&value);
            assert_eq!((range, how), (None, RangeParse::Unusable), "{value}");
        }
    }

    #[test]
    fn a_range_that_is_not_an_object_is_unusable() {
        let (range, how) = RetrieveRange::parse_loose(&json!("lines 1-80"));
        assert_eq!(range, None);
        assert_eq!(how, RangeParse::Unusable);
    }

    use super::*;

    const FRAGMENT: &str = "alpha\nbravo\ncharlie\ndelta\necho\nfoxtrot";

    #[test]
    fn a_whole_fragment_comes_back_with_its_measurements() {
        let got = slice(FRAGMENT, None, 1000);
        assert_eq!(got.content.as_deref(), Some(FRAGMENT));
        assert!(got.found && !got.ranged && !got.truncated);
        assert_eq!(got.total_lines, 6);
        assert_eq!(got.total_chars, FRAGMENT.chars().count());
    }

    #[test]
    fn a_line_window_is_one_based_and_inclusive() {
        let got = slice(
            FRAGMENT,
            Some(&RetrieveRange::Lines { start: 2, end: 4 }),
            1000,
        );
        assert_eq!(got.content.as_deref(), Some("bravo\ncharlie\ndelta"));
        assert!(got.ranged);
        assert_eq!(
            got.total_lines, 6,
            "totals describe the fragment, not the slice"
        );
    }

    #[test]
    fn a_reversed_or_out_of_range_window_yields_nothing_rather_than_everything() {
        let reversed = slice(
            FRAGMENT,
            Some(&RetrieveRange::Lines { start: 4, end: 2 }),
            1000,
        );
        assert_eq!(reversed.content.as_deref(), Some(""));
        let past_end = slice(
            FRAGMENT,
            Some(&RetrieveRange::Lines { start: 50, end: 60 }),
            1000,
        );
        assert_eq!(past_end.content.as_deref(), Some(""));
    }

    #[test]
    fn a_json_pointer_reaches_one_record_of_many() {
        let fragment = r#"[{"id":1,"name":"a"},{"id":2,"name":"b"},{"id":3,"name":"c"}]"#;
        let got = slice(
            fragment,
            Some(&RetrieveRange::JsonPointer {
                pointer: "/1/name".into(),
            }),
            1000,
        );
        assert_eq!(got.content.as_deref(), Some("\"b\""));
    }

    #[test]
    fn a_pointer_that_does_not_resolve_does_not_widen_the_answer() {
        // handing back the whole fragment would turn a narrowed retrieval into
        // the expensive one the caller was trying to avoid
        let fragment = r#"[{"id":1}]"#;
        let missing = slice(
            fragment,
            Some(&RetrieveRange::JsonPointer {
                pointer: "/9/nope".into(),
            }),
            1000,
        );
        assert_eq!(missing.content.as_deref(), Some(""));
        let not_json = slice(
            "plain text",
            Some(&RetrieveRange::JsonPointer {
                pointer: "/0".into(),
            }),
            1000,
        );
        assert_eq!(not_json.content.as_deref(), Some(""));
    }

    #[test]
    fn a_pattern_brings_its_context_with_it() {
        let got = slice(
            FRAGMENT,
            Some(&RetrieveRange::Grep {
                pattern: "delta".into(),
                context: 1,
            }),
            1000,
        );
        assert_eq!(got.content.as_deref(), Some("charlie\ndelta\necho"));
    }

    /// A context larger than the fragment means "all of it", not nothing.
    ///
    /// `context` is an unrestricted `usize` off the wire. `index + context`
    /// overflowed on a large valid integer: a debug build panicked, and a
    /// release build wrapped to a `high` below `low` and returned the lines
    /// around the match *without the matching line* — a retrieval that answers
    /// with text not containing what was asked for, and no empty-range hint.
    #[test]
    fn a_context_wider_than_the_fragment_selects_all_of_it() {
        for context in [usize::MAX, usize::MAX / 2, 1_000_000] {
            let got = slice(
                FRAGMENT,
                Some(&RetrieveRange::Grep {
                    pattern: "delta".into(),
                    context,
                }),
                10_000,
            );
            let content = got.content.unwrap_or_default();
            assert!(
                content.contains("delta"),
                "context {context} dropped the matching line: {content:?}"
            );
            assert_eq!(content, FRAGMENT.trim_end(), "context {context}");
        }
    }

    #[test]
    fn overlapping_matches_are_not_duplicated() {
        let got = slice(
            FRAGMENT,
            Some(&RetrieveRange::Grep {
                pattern: "a".into(),
                context: 2,
            }),
            1000,
        );
        let content = got.content.unwrap();
        assert_eq!(content.matches("alpha").count(), 1);
        assert_eq!(content.matches("charlie").count(), 1);
    }

    #[test]
    fn an_oversized_result_is_cut_and_says_so() {
        let big = "x".repeat(5000);
        let got = slice(&big, None, 100);
        assert!(got.truncated);
        assert_eq!(got.content.unwrap().chars().count(), 100);
        assert_eq!(
            got.total_chars, 5000,
            "the caller learns what it is missing"
        );
    }

    #[test]
    fn the_cap_respects_character_boundaries() {
        let text = "é".repeat(100);
        let got = slice(&text, None, 10);
        assert_eq!(got.content.unwrap().chars().count(), 10);
    }

    #[test]
    fn a_miss_tells_the_model_to_regenerate_rather_than_invent() {
        let miss = Retrieved::miss();
        assert!(!miss.found && !miss.expired);
        assert!(miss.miss_hint().contains("Re-run"));

        let expired = Retrieved::expired();
        assert!(expired.expired);
        assert!(expired.miss_hint().contains("expired"));
        assert!(expired.miss_hint().contains("do not reconstruct"));
    }
}
