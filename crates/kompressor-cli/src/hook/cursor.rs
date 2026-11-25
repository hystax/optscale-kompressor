//! Cursor's hook events, translated to the engine and back.
//!
//! The payloads are documented in docs/design/CURSOR.md, Appendix B, and
//! recorded in `fixtures/cursor-hooks/<build>/`. Every field is read
//! optionally: Cursor ships a new build every few days, and a payload this
//! does not recognise is answered with `{}` — the tool result unchanged —
//! rather than with a guess.
//!
//! | Event | Reply |
//! |---|---|
//! | `sessionStart` | `env` with `KOMPRESSOR_CONFIG`; the marker guidance as `additional_context` unless shadowing |
//! | `postToolUse`, `MCP:*` | `updated_mcp_tool_output` when the engine shrank the text, else `{}` |
//! | `postToolUse`, anything else | `{}`, and a shadow row when shadowing |
//! | `preToolUse`, `Shell` | `updated_input.command` when the command is eligible |
//! | `stop` | `{}`; the turn's token usage is recorded |
//! | anything else | `{}` |
//!
//! Never returned: `permission`. A rewrite must not approve a command on the
//! user's behalf, so approval stays wherever it was.

use std::path::Path;

use kompressor_core::config::Config;
use kompressor_core::{AppCore, Attribution, Identity, RequestInfo};
use kompressor_stats::events::{
    Event as StatsEvent, OptimizationEvent, ProxyRequestEvent, Surface,
};
use serde_json::{Value, json};

use super::{OneShot, note, nothing};

/// The prefix of every tool this project's MCP server offers. A retrieval
/// result is a fragment the model just asked for; compressing it would hand
/// back a marker for the thing the marker pointed at.
pub const OWN_TOOL_PREFIX: &str = "optscale_kompressor";

/// Cursor's spill threshold: a tool result over this many bytes is written to
/// a file and the model is handed a reference (`fileOutputThresholdBytes` on
/// every shell call; the MCP threshold measured between 32 and 164 KB).
/// Removal is counted only against what would otherwise have been inline.
pub const SPILL_BYTES: usize = 40_000;

/// The fields of an event this handler reads.
#[derive(Debug, Default)]
pub struct Event {
    pub name: String,
    pub conversation_id: Option<String>,
    pub generation_id: Option<String>,
    pub model: Option<String>,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub tool_input: Option<Value>,
    pub tool_output: Option<Value>,
    pub raw: Value,
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Parse one event, or `None` for anything that is not an object naming its
/// `hook_event_name`.
pub fn parse(input: &[u8]) -> Option<Event> {
    let raw: Value = serde_json::from_slice(input).ok()?;
    let name = raw.get("hook_event_name")?.as_str()?.to_string();
    Some(Event {
        name,
        conversation_id: text(&raw, "conversation_id"),
        generation_id: text(&raw, "generation_id"),
        model: text(&raw, "model"),
        tool_name: text(&raw, "tool_name"),
        tool_use_id: text(&raw, "tool_use_id"),
        tool_input: raw.get("tool_input").cloned(),
        tool_output: raw.get("tool_output").cloned(),
        raw,
    })
}

/// `tool_output` is a JSON-encoded *string*; decode it, accepting an object
/// too in case a build stops encoding it.
fn decoded_output(event: &Event) -> Option<Value> {
    match event.tool_output.as_ref()? {
        Value::String(encoded) => serde_json::from_str(encoded)
            .ok()
            .or_else(|| Some(Value::String(encoded.clone()))),
        other => Some(other.clone()),
    }
}

/// Answer one event. Never panics on input it does not recognise.
pub fn handle(
    input: &[u8],
    config: &Config,
    config_path: Option<&Path>,
    force_shadow: bool,
) -> Value {
    let hooks = &config.hooks.cursor;
    if !hooks.enabled {
        return nothing();
    }
    let Some(event) = parse(input) else {
        note("not a Cursor hook event; answering {}");
        return nothing();
    };
    super::touch_last_call("cursor", &event.name);
    let shadow = hooks.shadow || force_shadow;
    match event.name.as_str() {
        "sessionStart" => session_start(config, config_path, shadow),
        "postToolUse" => post_tool_use(&event, config, shadow),
        "preToolUse" if !shadow => pre_tool_use(&event, config, config_path),
        "stop" => {
            record_usage(&event, config);
            nothing()
        }
        _ => nothing(),
    }
}

/// The command a model runs to resolve a handle without the tool server.
pub fn retrieve_command(config_path: Option<&Path>) -> String {
    // Named as an allow-list can match it, like the rewrite's tail.
    let path = std::env::var_os("PATH");
    let binary = super::exe()
        .and_then(|exe| super::shell::program_word(&exe, path.as_deref()))
        .unwrap_or_else(|| "kompressor".to_string());
    match config_path.and_then(super::shell::quote) {
        Some(config) => format!("{binary} retrieve --config {config}"),
        None => format!("{binary} retrieve"),
    }
}

fn session_start(config: &Config, config_path: Option<&Path>, shadow: bool) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(path) = config_path {
        // Passed by Cursor to every later hook in the session, so they open
        // the same store this one would.
        out.insert(
            "env".into(),
            json!({ "KOMPRESSOR_CONFIG": path.display().to_string() }),
        );
    }
    // In shadow mode nothing the model sees changes, so there are no markers
    // to explain.
    if !shadow && (config.hooks.cursor.mcp.enabled || config.hooks.cursor.shell.enabled) {
        out.insert(
            "additional_context".into(),
            Value::String(kompressor_mcp::guidance::session_guidance(
                &retrieve_command(config_path),
            )),
        );
    }
    Value::Object(out)
}

/// A core over the configured store and a one-shot statistics sink.
struct Engine {
    core: AppCore,
    stats: OneShot,
}

impl Engine {
    fn open(config: &Config, need_store: bool) -> Option<Engine> {
        let store = if need_store {
            match kompressor_core::build_ref_store(config) {
                Ok(store) => store,
                Err(error) => {
                    note(&format!("the fragment store cannot be opened: {error}"));
                    return None;
                }
            }
        } else {
            let mut memory = config.clone();
            memory.refs.backend = kompressor_core::config::RefsBackend::Memory;
            kompressor_core::build_ref_store(&memory).ok()?
        };
        let stats = OneShot::open(config);
        let core = AppCore::new(config.clone(), store, stats.sink.clone());
        Some(Engine { core, stats })
    }

    fn finish(self) {
        drop(self.core);
        self.stats.finish();
    }
}

fn request<'a>(event: &'a Event) -> RequestInfo<'a> {
    RequestInfo {
        request_id: event
            .generation_id
            .as_deref()
            .or(event.tool_use_id.as_deref())
            .unwrap_or("cursor-hook"),
        api_family: None,
        model: event.model.as_deref(),
        provider: Some("cursor"),
    }
}

/// An MCP result's text items, or why it is not one to touch.
enum McpResult {
    /// The `content` array, and the indices of its text items.
    Content(Vec<Value>, Vec<usize>),
    /// A bare string.
    Text(String),
    /// Spilled to a file by Cursor: an empty text item. Nothing to compress
    /// and nothing true to replace it with.
    Spilled,
    /// Failed, `isError`, `{}`, or a shape this does not recognise.
    Untouchable,
}

fn mcp_result(decoded: Option<Value>) -> McpResult {
    match decoded {
        Some(Value::String(text)) if !text.is_empty() => McpResult::Text(text),
        Some(Value::Object(map)) => {
            if map.get("isError").and_then(Value::as_bool) == Some(true) {
                return McpResult::Untouchable;
            }
            let Some(Value::Array(items)) = map.get("content") else {
                return McpResult::Untouchable;
            };
            let mut texts = Vec::new();
            for (i, item) in items.iter().enumerate() {
                if item.get("type").and_then(Value::as_str) == Some("text") {
                    match item.get("text").and_then(Value::as_str) {
                        Some("") => return McpResult::Spilled,
                        Some(_) => texts.push(i),
                        None => return McpResult::Untouchable,
                    }
                }
            }
            if texts.is_empty() {
                McpResult::Untouchable
            } else {
                McpResult::Content(items.clone(), texts)
            }
        }
        _ => McpResult::Untouchable,
    }
}

fn post_tool_use(event: &Event, config: &Config, shadow: bool) -> Value {
    let Some(tool) = event.tool_name.as_deref() else {
        return nothing();
    };
    if let Some(mcp_tool) = tool.strip_prefix("MCP:") {
        if mcp_tool.starts_with(OWN_TOOL_PREFIX) {
            return nothing();
        }
        if shadow {
            measure_mcp(event, config, tool);
            return nothing();
        }
        let hooks = &config.hooks.cursor.mcp;
        if !hooks.enabled || hooks.skip_tools.iter().any(|skip| skip == mcp_tool) {
            return nothing();
        }
        return compress_mcp(event, config, tool);
    }
    if shadow {
        measure_other(event, config, tool);
    }
    nothing()
}

/// Compress each text item of an MCP result; reply only on a strict shrink.
fn compress_mcp(event: &Event, config: &Config, tool: &str) -> Value {
    let result = mcp_result(decoded_output(event));
    let texts: Vec<String> = match &result {
        McpResult::Content(items, indices) => indices
            .iter()
            .map(|&i| items[i]["text"].as_str().unwrap_or_default().to_string())
            .collect(),
        McpResult::Text(text) => vec![text.clone()],
        // Counted in compressing mode too, not only in shadow: the per-tool
        // count of spilled results is the list of servers worth fronting,
        // and it is in compressing mode that someone reads it.
        McpResult::Spilled => {
            unmeasured(event, config, tool, 0, "spilled");
            return nothing();
        }
        McpResult::Untouchable => return nothing(),
    };
    if texts
        .iter()
        .all(|t| t.chars().count() < config.engine.min_compress_chars)
    {
        return nothing();
    }
    let Some(engine) = Engine::open(config, true) else {
        return nothing();
    };
    let identity = Identity::anonymous();
    let info = request(event);
    let attribution = Attribution {
        tool: Some(tool),
        conversation: event.conversation_id.as_deref(),
        shadow: false,
        inline_limit_bytes: Some(crate::hook::cursor::SPILL_BYTES),
    };
    let mut before = 0usize;
    let mut after = 0usize;
    let compressed: Vec<String> = texts
        .iter()
        .map(|text| {
            let (out, _) = engine.core.compress_raw_attributed(
                &identity,
                text,
                None,
                &info,
                &config.engine,
                Surface::Hook,
                &attribution,
            );
            before += text.len();
            after += out.len();
            out
        })
        .collect();
    engine.finish();
    if after >= before {
        return nothing();
    }
    let replacement = match result {
        McpResult::Content(mut items, indices) => {
            for (slot, text) in indices.into_iter().zip(compressed) {
                items[slot]["text"] = Value::String(text);
            }
            json!({ "content": items })
        }
        McpResult::Text(_) => Value::String(compressed.into_iter().next().unwrap_or_default()),
        _ => return nothing(),
    };
    json!({ "updated_mcp_tool_output": replacement })
}

/// Shadow: what the engine would have removed from an MCP result.
fn measure_mcp(event: &Event, config: &Config, tool: &str) {
    match mcp_result(decoded_output(event)) {
        McpResult::Content(items, indices) => {
            let text: String = indices
                .iter()
                .map(|&i| items[i]["text"].as_str().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n");
            measure(event, config, tool, &text);
        }
        McpResult::Text(text) => measure(event, config, tool, &text),
        // Counted per tool: the list of servers worth fronting (design/CURSOR.md, MCP results).
        McpResult::Spilled => unmeasured(event, config, tool, 0, "spilled"),
        McpResult::Untouchable => unmeasured(event, config, tool, 0, "not_text"),
    }
}

/// Shadow for a built-in tool.
fn measure_other(event: &Event, config: &Config, tool: &str) {
    let decoded = decoded_output(event);
    match tool {
        "Shell" => {
            let command = event
                .tool_input
                .as_ref()
                .and_then(|i| i.get("command"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            // Already compressed by `hook emit`, which recorded its own row.
            if command.contains(" hook emit ") {
                return;
            }
            let output = decoded.as_ref().and_then(|d| {
                d.get("output")
                    .or_else(|| d.get("stdout"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
            match output {
                Some(text) => measure(event, config, tool, &text),
                None => unmeasured(event, config, tool, 0, "not_text"),
            }
        }
        // A `Read` carries its size, not its content: sized, never sampled.
        _ => {
            let length = decoded
                .as_ref()
                .and_then(|d| d.get("content_length"))
                .and_then(Value::as_u64);
            match length {
                Some(chars) => unmeasured(event, config, tool, chars, "builtin_tool"),
                None => {
                    if let Some(Value::String(text)) = decoded.as_ref() {
                        unmeasured(
                            event,
                            config,
                            tool,
                            text.chars().count() as u64,
                            "builtin_tool",
                        )
                    } else if let Some(other) = decoded.as_ref() {
                        let chars = other.to_string().chars().count() as u64;
                        unmeasured(event, config, tool, chars, "builtin_tool")
                    }
                }
            }
        }
    }
}

/// One shadow row from a dry engine run.
fn measure(event: &Event, config: &Config, tool: &str, text: &str) {
    let Some(engine) = Engine::open(config, false) else {
        return;
    };
    let attribution = Attribution {
        tool: Some(tool),
        conversation: event.conversation_id.as_deref(),
        shadow: true,
        inline_limit_bytes: Some(crate::hook::cursor::SPILL_BYTES),
    };
    let _ = engine.core.compress_raw_attributed(
        &Identity::anonymous(),
        text,
        None,
        &request(event),
        &config.engine,
        Surface::Hook,
        &attribution,
    );
    engine.finish();
}

/// One shadow row for output the engine cannot be shown: its size, and why.
fn unmeasured(event: &Event, config: &Config, tool: &str, chars: u64, reason: &str) {
    let stats = OneShot::open(config);
    if stats.sink.is_enabled() {
        let info = request(event);
        let mut row = OptimizationEvent::new(
            super::now_ms(),
            "",
            info.request_id,
            Surface::Hook,
            config.engine.mode,
        );
        let tokens = u32::try_from(chars.div_ceil(4)).unwrap_or(u32::MAX);
        row.model = event.model.clone();
        row.provider = Some("cursor".into());
        row.tokens_in = tokens;
        row.tokens_out = tokens;
        row.chars_in = chars;
        row.chars_out = chars;
        row.messages_total = 1;
        row.messages_skipped = 1;
        row.skipped_by_reason.insert(reason.to_string(), 1);
        row.skipped_tokens_by_reason
            .insert(reason.to_string(), tokens);
        row.outcome = Some("unreachable".into());
        row.tool = Some(tool.to_string());
        row.conversation = event.conversation_id.clone();
        row.shadow = true;
        stats.sink.send(StatsEvent::Optimization(row));
    }
    stats.finish();
}

fn pre_tool_use(event: &Event, config: &Config, config_path: Option<&Path>) -> Value {
    let shell = &config.hooks.cursor.shell;
    if !shell.enabled || event.tool_name.as_deref() != Some("Shell") {
        return nothing();
    }
    let Some(input) = event.tool_input.as_ref().and_then(Value::as_object) else {
        return nothing();
    };
    let Some(command) = input.get("command").and_then(Value::as_str) else {
        return nothing();
    };
    if let Err(reason) = super::shell::classify(command, &shell.skip) {
        tracing::debug!(reason = reason.as_str(), "shell command left alone");
        return nothing();
    }
    let Some(binary) = super::exe() else {
        return nothing();
    };
    let path = std::env::var_os("PATH");
    let Some(program) = super::shell::program_word(&binary, path.as_deref()) else {
        return nothing();
    };
    if shell.only_when_approved && !cli_approves(&program, &workspace_roots(event)) {
        return nothing();
    }
    let Some(capture) = super::create_capture(event.tool_use_id.as_deref()) else {
        note("the temporary directory is not writable; the command runs unchanged");
        return nothing();
    };
    let emit = super::shell::EmitArgs {
        config: config_path,
        request: event.generation_id.as_deref(),
        conversation: event.conversation_id.as_deref(),
        model: event.model.as_deref(),
    };
    match super::shell::rewrite_with(command, &program, &capture, &emit) {
        Some(rewritten) => json!({ "updated_input": { "command": rewritten } }),
        None => {
            let _ = std::fs::remove_file(&capture);
            nothing()
        }
    }
}

/// The workspace roots an event names.
fn workspace_roots(event: &Event) -> Vec<std::path::PathBuf> {
    event
        .raw
        .get("workspace_roots")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(std::path::PathBuf::from)
        .collect()
}

/// Whether the Cursor CLI would run a rewritten command without refusing it.
///
/// Approval is judged on the rewritten command, and measured on CLI
/// 2026.11.22 (design/CURSOR.md, Q1): an executable inside the brace group
/// that is not on the allow-list is still refused, so the rewrite never
/// approves anything the user had not — but this binary has to be approved
/// too, or *every* rewritten command is refused, including the ones the user
/// did approve. So a rewrite is offered only when the CLI runs everything
/// (`approvalMode: "unrestricted"`), or when its allow-list — the user's
/// `~/.cursor/cli-config.json`, merged with each workspace's
/// `.cursor/cli.json` — approves `program` as the rewrite writes it, and no
/// deny entry names it.
pub fn cli_approves(program: &str, workspaces: &[std::path::PathBuf]) -> bool {
    let read = |path: std::path::PathBuf| -> Option<Value> {
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    };
    let mut settings = cursor_home()
        .and_then(|home| read(home.join("cli-config.json")))
        .unwrap_or_else(|| json!({}));
    for project in workspaces
        .iter()
        .filter_map(|root| read(root.join(".cursor").join("cli.json")))
    {
        merge_like_cursor(&mut settings, project);
    }
    let entry = format!("Shell({program})");
    let listed = |key: &str| {
        settings
            .get("permissions")
            .and_then(|p| p.get(key))
            .and_then(Value::as_array)
            .is_some_and(|list| list.iter().any(|v| v.as_str() == Some(entry.as_str())))
    };
    if listed("deny") {
        return false;
    }
    settings.get("approvalMode").and_then(Value::as_str) == Some("unrestricted") || listed("allow")
}

/// Lay a workspace's `.cursor/cli.json` over the user's settings the way the
/// CLI does (its `g()` in 2026.11.22): objects merge key by key, and anything
/// else — an array included — *replaces* what was there. So a project that
/// sets `permissions.allow` replaces the user's whole allow-list rather than
/// adding to it. Measured: with `Shell(kompressor)` allow-listed globally and
/// `Shell(seq)` alone in the project file, the rewritten `seq` was refused.
fn merge_like_cursor(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(base), Value::Object(over)) => {
            for (key, value) in over {
                match base.get_mut(&key) {
                    Some(existing) if existing.is_object() && value.is_object() => {
                        merge_like_cursor(existing, value)
                    }
                    _ => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, over) => *base = over,
    }
}

/// `~/.cursor`.
pub fn cursor_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(|home| std::path::PathBuf::from(home).join(".cursor"))
}

/// The desktop `stop` event reports the turn's usage. Not a bill — there is
/// no price — but the provider-side count a hook otherwise never sees, filed
/// where the proxy files its own.
fn record_usage(event: &Event, config: &Config) {
    let raw = &event.raw;
    let count = |key: &str| {
        raw.get(key)
            .and_then(Value::as_u64)
            .map(|v| u32::try_from(v).unwrap_or(u32::MAX))
    };
    let input = count("input_tokens");
    let output = count("output_tokens");
    if input.is_none() && output.is_none() {
        return;
    }
    let stats = OneShot::open(config);
    if stats.sink.is_enabled() {
        stats.sink.send(StatsEvent::ProxyRequest(ProxyRequestEvent {
            context: None,
            timing: None,
            prefix: None,
            time_ms: super::now_ms(),
            tenant: String::new(),
            request_id: request(event).request_id.to_string(),
            api_family: "cursor".into(),
            model: event.model.clone(),
            upstream: "cursor".into(),
            status: Some(200),
            stream: false,
            strip_usage: false,
            prompt_tokens: input,
            completion_tokens: output,
            cache_read_tokens: count("cache_read_tokens"),
            cache_write_tokens: count("cache_write_tokens"),
            cache_write_1h_tokens: None,
            shadow_prompt_tokens: None,
            conversation: event.conversation_id.clone(),
            retrieve_rounds: 0,
            retrieve_tokens_injected: 0,
            upstream_ms: None,
            total_ms: None,
            error: None,
            keep_warm: false,
            output_shaping: None,
        }));
    }
    stats.finish();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(dir: &Path) -> Config {
        let mut config = Config::default();
        config.refs.path = dir.join("refs.db");
        config.stats.path = dir.join("stats.db");
        config
    }

    fn event(value: Value) -> Vec<u8> {
        serde_json::to_vec(&value).unwrap()
    }

    fn log(lines: usize) -> String {
        (0..lines)
            .map(|i| {
                format!(
                    "2026-11-25T10:00:{:02}Z INFO compiling crate-{} ok\n",
                    i % 60,
                    i % 7
                )
            })
            .collect()
    }

    fn mcp(text: &str) -> Value {
        json!({
            "hook_event_name": "postToolUse",
            "generation_id": "gen-1",
            "conversation_id": "conv-1",
            "tool_name": "MCP:big_text",
            "tool_input": {"lines": 40},
            "tool_output": serde_json::to_string(&json!({
                "content": [{"type": "text", "text": text}],
                "isError": false
            })).unwrap(),
        })
    }

    #[test]
    fn garbage_and_unknown_events_change_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        for input in [
            &b""[..],
            b"{",
            b"[1,2]",
            b"\"x\"",
            br#"{"hook_event_name":"somethingNew"}"#,
            br#"{"no":"name"}"#,
        ] {
            assert_eq!(handle(input, &config, None, false), json!({}));
        }
    }

    #[test]
    fn an_mcp_result_is_compressed_and_its_shape_mirrored() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let text = log(400);
        let reply = handle(&event(mcp(&text)), &config, None, false);
        let out = reply["updated_mcp_tool_output"]["content"][0]["text"]
            .as_str()
            .expect("a compressed text item");
        assert!(out.len() < text.len());
        assert!(out.contains("ref:k_"), "{out}");
        assert!(reply.get("permission").is_none());
    }

    #[test]
    fn a_result_that_is_not_plain_text_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        for output in [
            json!("{}"),
            json!(r#"{"content":[{"type":"text","text":""}],"isError":false}"#),
            json!(
                serde_json::to_string(
                    &json!({"content":[{"type":"text","text": log(400)}],"isError":true})
                )
                .unwrap()
            ),
            json!(r#"{"content":[{"type":"image","data":"AAAA","mimeType":"image/png"}]}"#),
        ] {
            let mut payload = mcp("x");
            payload["tool_output"] = output.clone();
            assert_eq!(
                handle(&event(payload), &config, None, false),
                json!({}),
                "{output}"
            );
        }
    }

    #[test]
    fn an_image_beside_the_text_survives_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let image = json!({"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"});
        let mut payload = mcp("");
        payload["tool_output"] = Value::String(
            serde_json::to_string(&json!({
                "content": [{"type": "text", "text": log(400)}, image.clone()],
                "isError": false
            }))
            .unwrap(),
        );
        let reply = handle(&event(payload), &config, None, false);
        assert_eq!(reply["updated_mcp_tool_output"]["content"][1], image);
    }

    #[test]
    fn our_own_tools_and_skipped_tools_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config(dir.path());
        let mut own = mcp(&log(400));
        own["tool_name"] = json!("MCP:optscale_kompressor_retrieve");
        assert_eq!(handle(&event(own), &config, None, false), json!({}));
        config.hooks.cursor.mcp.skip_tools = vec!["big_text".into()];
        assert_eq!(
            handle(&event(mcp(&log(400))), &config, None, false),
            json!({})
        );
    }

    #[test]
    fn a_small_or_incompressible_result_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        assert_eq!(
            handle(&event(mcp("small-ok")), &config, None, false),
            json!({})
        );
    }

    #[test]
    fn switched_off_it_answers_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config(dir.path());
        config.hooks.cursor.enabled = false;
        assert_eq!(
            handle(&event(mcp(&log(400))), &config, None, false),
            json!({})
        );
        assert!(!dir.path().join("stats.db").exists(), "and records nothing");
    }

    #[test]
    fn shadow_mode_records_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config(dir.path());
        config.hooks.cursor.shadow = true;
        assert_eq!(
            handle(&event(mcp(&log(400))), &config, None, false),
            json!({})
        );
        let conn = rusqlite_free_count(&dir.path().join("stats.db"));
        assert_eq!(conn, 1);
        assert!(!dir.path().join("refs.db").exists(), "nothing retained");
    }

    fn rusqlite_free_count(path: &Path) -> i64 {
        rusqlite::Connection::open(path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM optimization_events WHERE surface = 'hook' AND mode = 'off'",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn session_start_hands_over_the_config_and_the_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let reply = handle(
            &event(json!({"hook_event_name": "sessionStart"})),
            &config,
            Some(Path::new("/etc/k.toml")),
            false,
        );
        assert_eq!(reply["env"]["KOMPRESSOR_CONFIG"], "/etc/k.toml");
        let context = reply["additional_context"].as_str().unwrap();
        assert!(context.contains("optscale_kompressor_retrieve"));
        assert!(context.contains("retrieve --config '/etc/k.toml'"));
        let shadow = handle(
            &event(json!({"hook_event_name": "sessionStart"})),
            &config,
            None,
            true,
        );
        assert!(shadow.get("additional_context").is_none());
    }

    #[test]
    fn a_shell_command_is_rewritten_only_when_switched_on() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config(dir.path());
        let payload = json!({
            "hook_event_name": "preToolUse",
            "tool_name": "Shell",
            "tool_use_id": format!("t-{}", std::process::id()),
            "tool_input": {"command": "seq 1 30", "cwd": "", "timeout": 30000}
        });
        assert_eq!(
            handle(&event(payload.clone()), &config, None, false),
            json!({})
        );
        config.hooks.cursor.shell.enabled = true;
        config.hooks.cursor.shell.only_when_approved = false;
        let reply = handle(&event(payload), &config, None, false);
        if cfg!(windows) {
            assert_eq!(reply, json!({}));
            return;
        }
        let command = reply["updated_input"]["command"].as_str().unwrap();
        assert!(command.starts_with("{\nseq 1 30\n} > '"));
        assert!(command.contains(" hook emit "));
        assert!(reply.get("permission").is_none());
        // The capture file exists, owner-only, before the command runs.
        let file = command.rsplit('\'').nth(1).unwrap();
        assert!(Path::new(file).exists());
        let _ = std::fs::remove_file(file);
    }
}
