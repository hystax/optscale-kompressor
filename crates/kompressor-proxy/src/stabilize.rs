//! Keeping a conversation's prefix spelled the way its first request spelled it.
//!
//! A provider caches the prompt it renders, and it renders the tool list ahead
//! of the conversation. A client that sends the same tools in another order on
//! a later turn, or drops a beta token it sent before, has changed that
//! rendering without meaning to change anything, and the cache is lost from
//! the first byte that moved. Both are visible on the `/prefix` dashboard, as
//! `tool_order` and `beta` drift on the client's side.
//!
//! Two things are remembered per conversation, each from its first request:
//! the order of its tool names, and the beta tokens it has sent. With
//! `proxy.tools_order = "pinned"` a later request's tools go upstream in the
//! remembered order; with `proxy.sticky_beta = true` a later request that
//! carries fewer tokens goes upstream with all of them.
//!
//! # What this never does
//!
//! - **Touch the first request of a conversation.** There is nothing to pin it
//!   to. So a client that never reorders is never rewritten, and switching
//!   either setting on changes no conversation that was already consistent.
//! - **Sort.** A sorted list is stable too, but it differs from what every
//!   client already sends, so switching it on would cost each open
//!   conversation its cache and Claude Code the opening it shares across
//!   conversations.
//! - **Add a beta token the conversation never sent.** The union is of the
//!   client's own tokens.
//! - **Move a tool across a cache breakpoint.** A `cache_control` marker pins
//!   everything up to and including the tool that carries it; a reorder that
//!   would change what a marker covers is not made.
//!
//! Both are off by default, and both edit bytes the client sent, which the
//! rest of this proxy declines to do. They exist for a client the dashboard
//! has shown to drift.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Instant;

use http::{HeaderMap, HeaderValue};
use serde_json::Value;

use crate::drift::tool_name;
use crate::tool::RETRIEVE_TOOL_NAME;

/// Conversations remembered, the same bound the prefix tracker keeps.
const REMEMBER: usize = 4096;

/// The header whose tokens are kept.
const BETA: &str = "anthropic-beta";

#[derive(Default)]
struct Pinned {
    at: Option<Instant>,
    /// Tool names in the order they were first seen, ours left out.
    tools: Vec<String>,
    /// Beta tokens in the order they were first seen, as first spelled.
    betas: Vec<String>,
}

/// What each conversation's first request established.
#[derive(Default)]
pub struct Stabilizer {
    entries: Mutex<HashMap<u64, Pinned>>,
}

impl Stabilizer {
    fn with<T>(&self, conversation: u64, apply: impl FnOnce(&mut Pinned) -> T) -> Option<T> {
        let mut entries = self.entries.lock().ok()?;
        if entries.len() >= REMEMBER
            && !entries.contains_key(&conversation)
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.at)
                .map(|(key, _)| *key)
        {
            entries.remove(&oldest);
        }
        let entry = entries.entry(conversation).or_default();
        entry.at = Some(Instant::now());
        Some(apply(entry))
    }

    /// Record the tool names this request carries, and return the order they
    /// should go upstream in when that is not the order they came in.
    ///
    /// `None` on the conversation's first request, on one that already keeps
    /// the pinned order, and on one whose names cannot be told apart.
    pub(crate) fn pin_tools(&self, conversation: u64, names: &[&str]) -> Option<Vec<String>> {
        let client: Vec<&str> = names
            .iter()
            .copied()
            .filter(|name| *name != RETRIEVE_TOOL_NAME)
            .collect();
        let distinct: HashSet<&str> = client.iter().copied().collect();
        if client.is_empty() || distinct.len() != client.len() || distinct.contains("") {
            return None;
        }
        self.with(conversation, |pinned| {
            for name in &client {
                if !pinned.tools.iter().any(|known| known == name) {
                    pinned.tools.push((*name).to_string());
                }
            }
            let wanted: Vec<&str> = pinned
                .tools
                .iter()
                .map(String::as_str)
                .filter(|name| distinct.contains(name))
                .collect();
            (wanted != client).then(|| pinned.tools.clone())
        })
        .flatten()
    }

    /// Record the beta tokens this request carries, and put back the ones the
    /// conversation sent before and this request left out. Returns whether
    /// the header was rewritten.
    pub(crate) fn keep_betas(&self, conversation: u64, headers: &mut HeaderMap) -> bool {
        // A line that is not text cannot be carried into a rewritten header,
        // and dropping it would be a change the client never made.
        if headers
            .get_all(BETA)
            .iter()
            .any(|value| value.to_str().is_err())
        {
            return false;
        }
        let sent = beta_tokens(headers);
        let union = self.with(conversation, |pinned| {
            for token in &sent {
                if !pinned
                    .betas
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(token))
                {
                    pinned.betas.push(token.clone());
                }
            }
            pinned.betas.clone()
        });
        let Some(union) = union else { return false };
        if union.len() <= sent.len() {
            return false;
        }
        let Ok(value) = HeaderValue::from_str(&union.join(",")) else {
            return false;
        };
        // One line in place of however many the client sent.
        headers.remove(BETA);
        headers.insert(BETA, value);
        true
    }

    /// Conversations remembered. For tests.
    pub fn tracked(&self) -> usize {
        self.entries.lock().map_or(0, |entries| entries.len())
    }

    /// What each conversation pinned, for the state file. A conversation
    /// whose names are too many or too long to keep is left out, and is new
    /// to the next process.
    pub(crate) fn snapshot(&self, clock: &crate::persist::Clock) -> Vec<PinnedRecord> {
        let Ok(entries) = self.entries.lock() else {
            return Vec::new();
        };
        entries
            .iter()
            .map(|(&conversation, pinned)| PinnedRecord {
                conversation,
                age_ms: pinned.at.map_or(0, |at| clock.age(at)),
                tools: pinned.tools.clone(),
                betas: pinned.betas.clone(),
            })
            .filter(PinnedRecord::bounded)
            .collect()
    }

    /// Replace what is pinned with `records`, newest first up to the cap.
    pub(crate) fn restore(&self, records: Vec<PinnedRecord>, clock: &crate::persist::Clock) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        let records = records.into_iter().filter(PinnedRecord::bounded).collect();
        *entries = crate::prefixcache::newest(records, REMEMBER, |record| record.age_ms)
            .into_iter()
            .map(|record| {
                (
                    record.conversation,
                    Pinned {
                        at: Some(clock.at(record.age_ms)),
                        tools: record.tools,
                        betas: record.betas,
                    },
                )
            })
            .collect();
    }
}

/// Most tool names, or beta tokens, one conversation may pin and still be
/// written to the state file.
const MAX_PINNED: usize = 1024;

/// One conversation's pinned tool order and beta tokens in the state file:
/// tool names and header tokens, never a tool's definition.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct PinnedRecord {
    conversation: u64,
    age_ms: u64,
    tools: Vec<String>,
    betas: Vec<String>,
}

impl PinnedRecord {
    fn bounded(&self) -> bool {
        let short = |names: &[String]| {
            names.len() <= MAX_PINNED
                && names
                    .iter()
                    .all(|name| name.len() <= crate::persist::MAX_TEXT_BYTES)
        };
        short(&self.tools) && short(&self.betas)
    }
}

/// The beta tokens a request carries, in order, each once.
fn beta_tokens(headers: &HeaderMap) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    for value in headers.get_all(BETA) {
        let Ok(text) = value.to_str() else { continue };
        for token in text.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            if !tokens.iter().any(|known| known.eq_ignore_ascii_case(token)) {
                tokens.push(token.to_string());
            }
        }
    }
    tokens
}

fn marked(tool: &Value) -> bool {
    tool.get("cache_control")
        .is_some_and(|mark| !mark.is_null())
}

/// Put `tools` into the `pinned` order. Returns whether anything moved.
///
/// Tools `pinned` does not name keep their relative order after the ones it
/// does, and the retrieve tool stays last, where [`crate::tool::inject`] put
/// it. Nothing moves when a move would change which tools a `cache_control`
/// marker covers.
pub(crate) fn reorder(tools: &mut Vec<Value>, pinned: &[String]) -> bool {
    let rank = |tool: &Value| {
        let name = tool_name(tool);
        if name == RETRIEVE_TOOL_NAME {
            usize::MAX
        } else {
            pinned
                .iter()
                .position(|known| known == name)
                .unwrap_or(usize::MAX - 1)
        }
    };
    let mut order: Vec<usize> = (0..tools.len()).collect();
    // Stable, so tools of equal rank keep the order they arrived in.
    order.sort_by_key(|index| rank(&tools[*index]));
    if order.iter().enumerate().all(|(now, was)| now == *was) {
        return false;
    }
    // A marker pins the tools up to and including its own. After the move it
    // must still be the last of exactly the same set.
    for (now, was) in order.iter().enumerate() {
        if !marked(&tools[*was]) {
            continue;
        }
        let before: HashSet<usize> = (0..=*was).collect();
        let after: HashSet<usize> = order[..=now].iter().copied().collect();
        if before != after {
            return false;
        }
    }
    let mut taken: Vec<Option<Value>> = std::mem::take(tools).into_iter().map(Some).collect();
    *tools = order
        .into_iter()
        .filter_map(|index| taken[index].take())
        .collect();
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tools(names: &[&str]) -> Vec<Value> {
        names
            .iter()
            .map(|name| json!({"name": name, "description": "d", "input_schema": {}}))
            .collect()
    }

    fn names(tools: &[Value]) -> Vec<&str> {
        tools.iter().map(tool_name).collect()
    }

    #[test]
    fn the_first_request_pins_the_order_and_is_left_alone() {
        let pins = Stabilizer::default();
        assert_eq!(pins.pin_tools(1, &["Read", "Bash", "Grep"]), None);
        assert_eq!(
            pins.pin_tools(1, &["Read", "Bash", "Grep"]),
            None,
            "same order"
        );
        assert_eq!(
            pins.pin_tools(1, &["Grep", "Read", "Bash"]),
            Some(vec![
                "Read".to_string(),
                "Bash".to_string(),
                "Grep".to_string()
            ])
        );
        // Another conversation has its own first request.
        assert_eq!(pins.pin_tools(2, &["Grep", "Read", "Bash"]), None);
    }

    #[test]
    fn a_new_tool_goes_after_the_pinned_ones_wherever_the_client_put_it() {
        let pins = Stabilizer::default();
        pins.pin_tools(1, &["Read", "Bash"]);
        let pinned = pins.pin_tools(1, &["Edit", "Read", "Bash"]).unwrap();
        assert_eq!(pinned, ["Read", "Bash", "Edit"]);
        let mut list = tools(&["Edit", "Read", "Bash"]);
        assert!(reorder(&mut list, &pinned));
        assert_eq!(names(&list), ["Read", "Bash", "Edit"]);
        // And it is pinned there from now on.
        assert_eq!(pins.pin_tools(1, &["Read", "Bash", "Edit"]), None);
        // A tool the client stops sending leaves no hole to trip over.
        assert_eq!(pins.pin_tools(1, &["Read", "Edit"]), None);
    }

    #[test]
    fn names_that_cannot_be_told_apart_pin_nothing() {
        let pins = Stabilizer::default();
        assert_eq!(pins.pin_tools(1, &["Read", "Read"]), None);
        assert_eq!(pins.pin_tools(1, &["Read", ""]), None);
        assert_eq!(pins.pin_tools(1, &[]), None);
        assert_eq!(pins.tracked(), 0, "and nothing was remembered");
    }

    #[test]
    fn the_retrieve_tool_is_not_pinned_and_stays_last() {
        let pins = Stabilizer::default();
        assert_eq!(
            pins.pin_tools(1, &["Read", "Bash", RETRIEVE_TOOL_NAME]),
            None
        );
        // Absent on a later request: not a reorder.
        assert_eq!(pins.pin_tools(1, &["Read", "Bash"]), None);
        let pinned = pins
            .pin_tools(1, &["Bash", "Read", RETRIEVE_TOOL_NAME])
            .unwrap();
        let mut list = tools(&["Bash", RETRIEVE_TOOL_NAME, "Read"]);
        assert!(reorder(&mut list, &pinned));
        assert_eq!(names(&list), ["Read", "Bash", RETRIEVE_TOOL_NAME]);
    }

    #[test]
    fn a_reorder_keeps_every_definition_whole() {
        let mut list = vec![
            json!({"type": "function", "function": {"name": "b", "parameters": {"z": 1, "a": 2}}}),
            json!({"type": "function", "function": {"name": "a", "parameters": {}}}),
        ];
        let before = list.clone();
        assert!(reorder(&mut list, &["a".to_string(), "b".to_string()]));
        assert_eq!(list, vec![before[1].clone(), before[0].clone()]);
        assert_eq!(
            serde_json::to_string(&list[1]).unwrap(),
            serde_json::to_string(&before[0]).unwrap(),
            "key order inside a definition is the client's"
        );
        // Idempotent: its own output is already in order.
        assert!(!reorder(&mut list, &["a".to_string(), "b".to_string()]));
    }

    #[test]
    fn a_tool_is_not_moved_across_a_cache_breakpoint() {
        let pinned = ["Read".to_string(), "Bash".to_string(), "Grep".to_string()];
        // The marker is on the last tool and covers all three either way.
        let mut covered = tools(&["Bash", "Read", "Grep"]);
        covered[2]["cache_control"] = json!({"type": "ephemeral"});
        assert!(reorder(&mut covered, &pinned));
        assert_eq!(names(&covered), ["Read", "Bash", "Grep"]);
        assert!(marked(&covered[2]));

        // The marker covers Grep alone; pinning would put two tools under it.
        let mut split = tools(&["Grep", "Read", "Bash"]);
        split[0]["cache_control"] = json!({"type": "ephemeral"});
        let before = split.clone();
        assert!(!reorder(&mut split, &pinned));
        assert_eq!(split, before, "left exactly as the client sent it");
    }

    fn betas(lines: &[&'static str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for line in lines {
            headers.append(BETA, HeaderValue::from_static(line));
        }
        headers
    }

    fn sent(headers: &HeaderMap) -> Vec<&str> {
        headers
            .get_all(BETA)
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect()
    }

    #[test]
    fn a_dropped_beta_token_is_put_back_and_none_is_invented() {
        let pins = Stabilizer::default();
        let mut first = betas(&["tools-2024, context-1m"]);
        assert!(
            !pins.keep_betas(1, &mut first),
            "the first request is left alone"
        );
        assert_eq!(sent(&first), ["tools-2024, context-1m"]);

        let mut fewer = betas(&["context-1m"]);
        assert!(pins.keep_betas(1, &mut fewer));
        assert_eq!(
            sent(&fewer),
            ["tools-2024,context-1m"],
            "in first-seen order"
        );

        let mut none = HeaderMap::new();
        assert!(pins.keep_betas(1, &mut none));
        assert_eq!(sent(&none), ["tools-2024,context-1m"]);

        // Another conversation never sent them.
        let mut other = betas(&["context-1m"]);
        assert!(!pins.keep_betas(2, &mut other));
        assert_eq!(sent(&other), ["context-1m"]);
    }

    #[test]
    fn a_header_line_that_is_not_text_is_never_rewritten() {
        let pins = Stabilizer::default();
        pins.keep_betas(1, &mut betas(&["tools-2024,context-1m"]));
        let mut odd = betas(&["context-1m"]);
        odd.append(BETA, HeaderValue::from_bytes(b"caf\xe9").unwrap());
        assert!(!pins.keep_betas(1, &mut odd));
        assert_eq!(
            odd.get_all(BETA).iter().count(),
            2,
            "both lines still there"
        );
    }

    #[test]
    fn the_same_tokens_in_another_spelling_are_not_rewritten() {
        let pins = Stabilizer::default();
        pins.keep_betas(1, &mut betas(&["tools-2024,context-1m"]));
        // Two lines, another order, another case: the same set.
        let mut same = betas(&["Context-1M", "tools-2024"]);
        assert!(!pins.keep_betas(1, &mut same));
        assert_eq!(sent(&same), ["Context-1M", "tools-2024"], "byte for byte");
        // A token added later joins the union and is kept from then on.
        let mut more = betas(&["tools-2024,context-1m,files-api"]);
        assert!(!pins.keep_betas(1, &mut more));
        let mut back = betas(&["tools-2024"]);
        assert!(pins.keep_betas(1, &mut back));
        assert_eq!(sent(&back), ["tools-2024,context-1m,files-api"]);
    }

    #[test]
    fn the_table_stays_bounded() {
        let pins = Stabilizer::default();
        for conversation in 0..(REMEMBER as u64 + 16) {
            pins.pin_tools(conversation, &["Read"]);
        }
        assert_eq!(pins.tracked(), REMEMBER);
    }
}
