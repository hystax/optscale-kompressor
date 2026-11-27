//! Keeping a long tool catalogue out of the prompt until it is asked for.
//!
//! A client with many tools — opencode's ~135 built-in definitions, or a few
//! MCP servers — sends every schema on every request, at the front of the
//! cached prefix. Anthropic's tool search lets a request mark a tool
//! `defer_loading: true`: it is not rendered into the prompt, and the model
//! finds it through a search tool when it needs it, after which it is callable
//! as usual. `proxy.tools_defer` turns that on for clients that do not do it
//! themselves. Headroom does the same (`inject_tool_search_deferral`); the
//! shape and the resident set below follow it. See
//! docs/design/HEADROOM-PORT.md, Tier 1.
//!
//! # What is deferred
//!
//! Every tool the client declared except:
//!
//! - one whose name is in `proxy.tools_defer_keep`, compared lower-cased with
//!   leading underscores dropped (`Bash`, `bash` and `_bash` are one name):
//!   the tools an agent calls every turn would cost a search round each;
//! - a typed server tool (`web_search_…`, `computer_…`), which has no schema
//!   to defer;
//! - the retrieve tool this proxy adds.
//!
//! Nothing is done when the list is shorter than `tools_defer_min_tools`,
//! when the client already defers (any `defer_loading`, or a tool-search entry
//! in any spelling — deferring on top of a client's own mechanism inlines what
//! it kept out), or when nothing would be deferred.
//!
//! # Why the cached prefix survives it
//!
//! The decision is taken on a conversation's first request and held for its
//! life, like `tools_compact`'s, so a conversation is deferred from its first
//! byte or never. The transform is a pure function of the tool list and the
//! keep set, so the resident tools are the same bytes every turn; a tool the
//! client adds later is deferred too, and a deferred tool is not rendered, so
//! adding one does not move the prefix. The search tool goes after the
//! client's tools so the resident ones keep their position.
//!
//! A deferred tool may not carry `cache_control`. A breakpoint found on one is
//! moved to the last resident tool, keeping its TTL, so the (shorter) tools
//! prefix still caches and the breakpoint count does not change.
//!
//! # History repair
//!
//! Once deferral is on, the provider answers with a `server_tool_use` and a
//! `tool_search_tool_result` naming the tools it loaded, and the client keeps
//! both in its transcript. The provider checks every reference in the history
//! against the request's `tools` and refuses one that names a tool it does not
//! carry. Claude Code replays the transcript on side requests (a Stop-hook
//! evaluation, `/compact`) with a smaller tool list, so [`repair_history`]
//! replaces an unresolvable pair with a text block — in place, because
//! deleting a block shifts the position of the thinking blocks after it.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use crate::drift::tool_name;
use crate::tool::RETRIEVE_TOOL_NAME;

/// The search tool appended to a deferred list.
pub const SEARCH_TOOL_TYPE: &str = "tool_search_tool_regex_20251119";
/// Its name on the wire.
pub const SEARCH_TOOL_NAME: &str = "tool_search_tool_regex";

/// Names a client's own tool-search tool goes by, in the shapes seen in the
/// wild: Codex and the Responses API (`tool_search`), Copilot CLI
/// (`tool_search_tool`), Codebuff (`composio_search_tools`). A list carrying
/// one is a client deferring by itself.
const CLIENT_SEARCH_NAMES: [&str; 4] = [
    "tool_search",
    "tool_search_tool",
    "tool_search_tool_regex",
    "composio_search_tools",
];

/// What a replaced search pair says in the transcript.
const REPAIRED_TEXT: &str = "[tool search result no longer available in this request]";

/// A tool name as the keep set compares it.
pub fn normalised(name: &str) -> String {
    name.trim_start_matches('_').to_ascii_lowercase()
}

/// Whether the client is already deferring tool schemas itself.
pub fn client_defers(tools: &[Value]) -> bool {
    tools.iter().any(|tool| {
        tool.get("defer_loading").and_then(Value::as_bool) == Some(true)
            || tool
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.starts_with("tool_search"))
            || tool
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| CLIENT_SEARCH_NAMES.contains(&name))
    })
}

/// What a deferral took out of the rendered prompt.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Deferred {
    /// Tool definitions marked `defer_loading`.
    pub tools: u32,
    /// Their serialised size: what stops being rendered every request.
    pub bytes: u32,
}

/// Whether `tool` stays in the prompt.
fn resident(tool: &Value, keep: &HashSet<String>) -> bool {
    let Some(object) = tool.as_object() else {
        return true;
    };
    // A typed server tool has no schema of its own to defer.
    if object
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind != "custom")
    {
        return true;
    }
    let name = tool_name(tool);
    name.is_empty() || name == RETRIEVE_TOOL_NAME || keep.contains(&normalised(name))
}

/// Mark the non-resident tools `defer_loading` and append the search tool.
///
/// Returns what was deferred, or `None` with `tools` untouched when the list is
/// shorter than `min_tools`, the client already defers, or every tool is
/// resident.
pub fn defer_tools(tools: &mut Vec<Value>, keep: &[String], min_tools: usize) -> Option<Deferred> {
    if tools.len() < min_tools || client_defers(tools) {
        return None;
    }
    let keep: HashSet<String> = keep.iter().map(|name| normalised(name)).collect();
    if tools.iter().all(|tool| resident(tool, &keep)) {
        return None;
    }
    let mut deferred = Deferred::default();
    let mut moved_marker: Option<Value> = None;
    let mut resident_marked = false;
    let mut last_resident: Option<usize> = None;
    for (index, tool) in tools.iter_mut().enumerate() {
        if resident(tool, &keep) {
            if tool.get("type").and_then(Value::as_str).is_none()
                || tool.get("type").and_then(Value::as_str) == Some("custom")
            {
                last_resident = Some(index);
                resident_marked |= tool.get("cache_control").is_some();
            }
            continue;
        }
        let Some(object) = tool.as_object_mut() else {
            continue;
        };
        if let Some(marker) = object.remove("cache_control") {
            moved_marker = Some(marker);
        }
        deferred.tools += 1;
        deferred.bytes = deferred
            .bytes
            .saturating_add(serde_json::to_vec(&*object).map_or(0, |b| b.len() as u32));
        object.insert("defer_loading".to_string(), Value::Bool(true));
    }
    if let Some(marker) = moved_marker
        && !resident_marked
        && let Some(index) = last_resident
        && let Some(object) = tools[index].as_object_mut()
    {
        object.insert("cache_control".to_string(), marker);
    }
    tools.push(json!({"type": SEARCH_TOOL_TYPE, "name": SEARCH_TOOL_NAME}));
    Some(deferred)
}

/// The tool names a `tool_search_tool_result` block refers to.
fn references(block: &Value) -> Vec<&str> {
    let content = block.get("content");
    let entries = content
        .and_then(|content| content.get("tool_references"))
        .and_then(Value::as_array)
        .or_else(|| content.and_then(Value::as_array));
    entries
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            entry
                .get("tool_name")
                .or_else(|| entry.get("name"))
                .and_then(Value::as_str)
        })
        .collect()
}

/// Replace the tool-search pairs this request's `tools` cannot resolve.
/// Returns how many blocks were replaced.
///
/// A pair is unresolvable when its result names a tool absent from `tools`,
/// or when the request carries no search tool at all (the provider refuses a
/// search result with nothing to have searched). Deterministic, so a request
/// repaired the same way every turn still caches.
pub fn repair_history(body: &mut Value) -> usize {
    let tools: HashSet<String> = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .map(|tool| tool_name(tool).to_string())
                .collect()
        })
        .unwrap_or_default();
    let has_search = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools.iter().any(|tool| {
                tool.get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind.starts_with("tool_search_tool_"))
            })
        });
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return 0;
    };
    // First pass: the results that cannot be resolved, by the id of the call
    // that produced them.
    let mut broken: HashMap<String, ()> = HashMap::new();
    for message in messages.iter() {
        for block in message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if block.get("type").and_then(Value::as_str) != Some("tool_search_tool_result") {
                continue;
            }
            let unresolved =
                !has_search || references(block).iter().any(|name| !tools.contains(*name));
            if unresolved && let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                broken.insert(id.to_string(), ());
            }
        }
    }
    if broken.is_empty() {
        return 0;
    }
    let mut replaced = 0;
    for message in messages.iter_mut() {
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in blocks.iter_mut() {
            let kind = block.get("type").and_then(Value::as_str);
            let id = match kind {
                Some("server_tool_use") => block.get("id").and_then(Value::as_str),
                Some("tool_search_tool_result") => block.get("tool_use_id").and_then(Value::as_str),
                _ => None,
            };
            if id.is_some_and(|id| broken.contains_key(id)) {
                *block = json!({"type": "text", "text": REPAIRED_TEXT});
                replaced += 1;
            }
        }
    }
    replaced
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keep() -> Vec<String> {
        kompressor_core::config::ProxyConfig::default().tools_defer_keep
    }

    fn tool(name: &str) -> Value {
        json!({"name": name, "description": "d".repeat(100), "input_schema": {"type": "object"}})
    }

    fn catalogue(n: usize) -> Vec<Value> {
        let mut tools = vec![tool("Bash"), tool("Read"), tool("_edit")];
        tools.extend((0..n).map(|i| tool(&format!("mcp__srv{i}__do"))));
        tools
    }

    #[test]
    fn non_core_tools_are_deferred_and_the_search_tool_goes_last() {
        let mut tools = catalogue(17);
        let deferred = defer_tools(&mut tools, &keep(), 12).expect("deferred");
        assert_eq!(deferred.tools, 17);
        assert!(deferred.bytes > 17 * 100);
        assert_eq!(tools.len(), 21);
        for core in &tools[..3] {
            assert!(core.get("defer_loading").is_none(), "{core}");
        }
        for mcp in &tools[3..20] {
            assert_eq!(mcp["defer_loading"], true);
        }
        assert_eq!(tools[20]["type"], SEARCH_TOOL_TYPE);
        assert_eq!(tools[20]["name"], SEARCH_TOOL_NAME);
    }

    #[test]
    fn the_resident_bytes_are_the_same_every_turn_and_a_new_tool_is_deferred() {
        let mut first = catalogue(15);
        defer_tools(&mut first, &keep(), 12).unwrap();
        let mut later = catalogue(16);
        defer_tools(&mut later, &keep(), 12).unwrap();
        assert_eq!(first[..3], later[..3], "resident tools unchanged");
        assert_eq!(later[18]["defer_loading"], true, "the new tool is deferred");
        let mut again = catalogue(15);
        defer_tools(&mut again, &keep(), 12).unwrap();
        assert_eq!(first, again, "pure function of the list");
    }

    #[test]
    fn nothing_happens_below_the_threshold_or_when_the_client_defers() {
        let mut short = catalogue(5);
        let before = short.clone();
        assert!(defer_tools(&mut short, &keep(), 12).is_none());
        assert_eq!(short, before);

        let mut own = catalogue(20);
        own[5]["defer_loading"] = json!(true);
        let before = own.clone();
        assert!(defer_tools(&mut own, &keep(), 12).is_none());
        assert_eq!(own, before);

        for search in [
            json!({"type": "tool_search_tool_bm25_20251119", "name": "tool_search_tool_bm25"}),
            json!({"type": "tool_search"}),
            json!({"name": "tool_search_tool", "input_schema": {}}),
        ] {
            let mut tools = catalogue(20);
            tools.push(search);
            assert!(defer_tools(&mut tools, &keep(), 12).is_none());
        }
    }

    #[test]
    fn applying_it_twice_changes_nothing_the_second_time() {
        let mut tools = catalogue(20);
        defer_tools(&mut tools, &keep(), 12).unwrap();
        let once = tools.clone();
        assert!(defer_tools(&mut tools, &keep(), 12).is_none());
        assert_eq!(tools, once);
    }

    #[test]
    fn every_tool_resident_is_left_alone() {
        let mut tools: Vec<Value> = keep().iter().map(|name| tool(name)).collect();
        let before = tools.clone();
        assert!(defer_tools(&mut tools, &keep(), 3).is_none());
        assert_eq!(tools, before);
    }

    #[test]
    fn typed_server_tools_and_the_retrieve_tool_stay_resident() {
        let mut tools = catalogue(14);
        tools.push(json!({"type": "web_search_20250305", "name": "web_search"}));
        tools.push(tool(RETRIEVE_TOOL_NAME));
        defer_tools(&mut tools, &keep(), 12).unwrap();
        assert!(tools[17].get("defer_loading").is_none());
        assert!(tools[18].get("defer_loading").is_none());
    }

    #[test]
    fn a_breakpoint_on_a_deferred_tool_moves_to_the_last_resident_one() {
        let mut tools = catalogue(14);
        let marker = json!({"type": "ephemeral", "ttl": "1h"});
        tools[16]["cache_control"] = marker.clone();
        defer_tools(&mut tools, &keep(), 12).unwrap();
        assert!(tools[16].get("cache_control").is_none());
        assert_eq!(tools[2]["cache_control"], marker, "TTL kept");
        let marks = tools
            .iter()
            .filter(|t| t.get("cache_control").is_some())
            .count();
        assert_eq!(marks, 1, "breakpoint count unchanged");
        assert!(tools.last().unwrap().get("cache_control").is_none());
    }

    #[test]
    fn a_resident_breakpoint_is_not_doubled() {
        let mut tools = catalogue(14);
        tools[0]["cache_control"] = json!({"type": "ephemeral"});
        tools[10]["cache_control"] = json!({"type": "ephemeral"});
        defer_tools(&mut tools, &keep(), 12).unwrap();
        let marks = tools
            .iter()
            .filter(|t| t.get("cache_control").is_some())
            .count();
        assert_eq!(marks, 1);
    }

    #[test]
    fn names_compare_without_case_or_leading_underscores() {
        assert_eq!(normalised("Bash"), "bash");
        assert_eq!(normalised("__Read"), "read");
        assert_eq!(normalised("TodoWrite"), "todowrite");
    }

    fn with_history(tools: Vec<Value>) -> Value {
        json!({
            "tools": tools,
            "messages": [
                {"role": "user", "content": "find the slack tool"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "t", "signature": "s"},
                    {"type": "server_tool_use", "id": "srv_1", "name": SEARCH_TOOL_NAME,
                     "input": {"pattern": "slack"}},
                    {"type": "tool_search_tool_result", "tool_use_id": "srv_1",
                     "content": {"type": "tool_search_tool_search_result",
                                 "tool_references": [{"type": "tool_reference", "tool_name": "mcp__slack__post"}]}},
                    {"type": "text", "text": "found it"}
                ]},
                {"role": "user", "content": "go on"}
            ]
        })
    }

    #[test]
    fn a_resolvable_search_pair_is_left_alone() {
        let mut body = with_history(vec![
            tool("mcp__slack__post"),
            json!({"type": SEARCH_TOOL_TYPE, "name": SEARCH_TOOL_NAME}),
        ]);
        let before = body.clone();
        assert_eq!(repair_history(&mut body), 0);
        assert_eq!(body, before);
    }

    #[test]
    fn an_unresolvable_pair_is_replaced_in_place() {
        let mut body = with_history(vec![
            tool("Bash"),
            json!({"type": SEARCH_TOOL_TYPE, "name": SEARCH_TOOL_NAME}),
        ]);
        assert_eq!(repair_history(&mut body), 2);
        let blocks = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 4, "positions kept");
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[2]["type"], "text");
        assert_eq!(blocks[3]["text"], "found it");
        // and again: deterministic, nothing left to repair
        assert_eq!(repair_history(&mut body), 0);
    }

    #[test]
    fn a_request_with_no_search_tool_cannot_carry_search_results() {
        let mut body = with_history(vec![tool("mcp__slack__post")]);
        assert_eq!(repair_history(&mut body), 2);
    }
}
