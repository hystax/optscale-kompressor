//! `kompressor hook install | uninstall | status cursor`, and the plugin
//! directory `wrap cursor-agent` hands one session.
//!
//! The editor is not launched by `wrap`, so there is no command line to put a
//! flag on: the registration has to be durable. It is made as small and as
//! owned as it can be (docs/design/CURSOR.md, "Desktop"):
//!
//! * `~/.cursor/hooks.json` gains four entries — `sessionStart`,
//!   `preToolUse` on `Shell`, `postToolUse` on `MCP:` (every tool in shadow
//!   mode) and `stop`;
//! * `~/.cursor/mcp.json` gains one server, `optscale_kompressor`, unless
//!   shadowing, when there are no markers to resolve.
//!
//! Ownership is a sidecar under `$KOMPRESSOR_HOME/installed/` listing exactly
//! the entries written, the bytes each file held before, and the bytes this
//! wrote. Uninstall puts a file back byte for byte when nobody has touched it
//! since; otherwise it removes exactly the entries it owns, and reports and
//! leaves any the user has edited. Unlike the claims in `wrap/settings.rs` an
//! install has no owning process — it is meant to outlive this one.

use std::path::{Path, PathBuf};

use kompressor_core::config::Config;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::error::{CliError, CliResult};

/// The hook timeout written into `hooks.json`, in seconds. The handler's own
/// deadline (`hooks.cursor.deadline`, 2 s by default) stays below it.
pub const TIMEOUT_S: u64 = 5;

/// The events registered, with the matcher each carries.
fn events(shadow: bool) -> Vec<(&'static str, Option<&'static str>)> {
    vec![
        ("sessionStart", None),
        ("preToolUse", Some("^Shell$")),
        // Every tool in shadow mode, which is what sizes Read, Grep and Shell.
        ("postToolUse", Some(if shadow { "" } else { "^MCP:" })),
        ("stop", None),
    ]
}

/// Arguments of `hook install cursor`.
pub struct InstallArgs {
    pub project: bool,
    pub print: bool,
    pub shadow: bool,
    pub force: bool,
    pub allow_list: bool,
    pub config: Option<PathBuf>,
}

/// Where one install lives.
struct Scope {
    /// `~/.cursor`, or `<workspace>/.cursor`.
    cursor_dir: PathBuf,
    /// The sidecar recording what was written.
    sidecar: PathBuf,
    /// Where a pinned configuration is written when one has to be.
    pinned_config: PathBuf,
    label: String,
}

fn scope(project: bool) -> Result<Scope, CliError> {
    let state = super::state_dir();
    if project {
        let workspace = std::env::current_dir()
            .map_err(|e| CliError::runtime("could not read the working directory", e))?;
        let key = fnv(&workspace.display().to_string());
        Ok(Scope {
            cursor_dir: workspace.join(".cursor"),
            sidecar: state.join(format!("cursor-project-{key}.json")),
            pinned_config: state.join(format!("cursor-project-{key}.toml")),
            label: format!("project {}", workspace.display()),
        })
    } else {
        let cursor_dir = super::cursor::cursor_home().ok_or_else(|| {
            CliError::Usage(
                "no home directory: HOME is not set, so ~/.cursor cannot be found".into(),
            )
        })?;
        Ok(Scope {
            cursor_dir,
            sidecar: state.join("cursor.json"),
            pinned_config: state.join("cursor.toml"),
            label: "user".into(),
        })
    }
}

fn fnv(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// What an install wrote, so that uninstall removes exactly that.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Sidecar {
    scope: String,
    installed_ms: i64,
    files: Vec<FileClaim>,
    /// The configuration the hooks run under.
    config: Option<PathBuf>,
    /// Whether that configuration was written by the install.
    config_written: bool,
    /// The CLI configuration `Shell(kompressor)` was added to, if it was.
    allow_list: Option<PathBuf>,
    /// The entries added to it, removed again on uninstall.
    #[serde(default)]
    allow_entries: Vec<String>,
    shadow: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct FileClaim {
    path: PathBuf,
    /// The file as it was; `None` when it did not exist.
    original: Option<String>,
    /// The file as this wrote it.
    written: String,
    entries: Vec<Owned>,
}

/// One entry, by where it lives: an element of `hooks.<event>`, or the value
/// of `mcpServers.<name>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Owned {
    /// `hooks` or `mcpServers`.
    section: String,
    /// The event or the server name.
    key: String,
    value: Value,
}

/// Quote a path for the command line Cursor runs a hook with.
fn shell_word(path: &Path) -> String {
    let text = path.display().to_string();
    if cfg!(windows) {
        format!("\"{text}\"")
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// The hook command for an install.
pub fn hook_command(exe: &Path, config: Option<&Path>, shadow: bool) -> String {
    let mut command = format!("{} hook cursor", shell_word(exe));
    if let Some(config) = config {
        command.push_str(&format!(" --config {}", shell_word(config)));
    }
    if shadow {
        command.push_str(" --shadow");
    }
    command
}

/// The `hooks` object an install adds.
pub fn hook_entries(command: &str, shadow: bool) -> Vec<(String, Value)> {
    events(shadow)
        .into_iter()
        .map(|(event, matcher)| {
            let mut entry = json!({ "command": command, "timeout": TIMEOUT_S });
            if let Some(matcher) = matcher {
                entry["matcher"] = Value::String(matcher.to_string());
            }
            (event.to_string(), entry)
        })
        .collect()
}

/// The `optscale_kompressor` server registration.
pub fn mcp_entry(exe: &Path, config: Option<&Path>) -> Value {
    let mut args = vec![json!("mcp"), json!("serve")];
    if let Some(config) = config {
        args.push(json!("--config"));
        args.push(json!(config.display().to_string()));
    }
    // `env` names nothing: a credential copied here would be a second copy
    // on disk.
    json!({ "command": exe.display().to_string(), "args": args, "env": {} })
}

/// Resolve the configuration the hooks will run under, and whether it has to
/// be written as a pinned copy.
///
/// The hook, `emit` and the tool server are three processes started from three
/// working directories, so the store has to be named absolutely. A file whose
/// `refs.path` and `stats.path` are already absolute is referenced where it
/// is — edits to it then reach every later hook. Otherwise the configuration
/// is written out with both paths made absolute: against the working
/// directory when a file named them, against `$KOMPRESSOR_HOME` when there was
/// no file at all.
fn resolve_config(explicit: Option<&Path>) -> Result<(Config, Option<PathBuf>, bool), CliError> {
    let config = Config::load(explicit).map_err(|e| CliError::Usage(e.to_string()))?;
    let absolute = config.refs.path.is_absolute() && config.stats.path.is_absolute();
    if absolute && let Some(source) = config.source.clone() {
        return Ok((config, Some(source), false));
    }
    let base = if config.source.is_some() {
        std::env::current_dir()
            .map_err(|e| CliError::runtime("could not read the working directory", e))?
    } else {
        crate::wrap::paths::Paths::from_env()
            .run_dir()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
    };
    let mut pinned = config.clone();
    if pinned.refs.path.is_relative() {
        pinned.refs.path = base.join(&pinned.refs.path);
    }
    if pinned.stats.path.is_relative() {
        pinned.stats.path = base.join(&pinned.stats.path);
    }
    Ok((pinned, None, true))
}

/// A configuration for one session or one install, with its paths pinned.
pub fn write_pinned(config: &Config, path: &Path) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CliError::runtime(&format!("could not create {}", parent.display()), e))?;
    }
    let text = toml::to_string_pretty(config)
        .map_err(|e| CliError::runtime("could not serialise the configuration", e))?;
    crate::wrap::paths::write_private(path, &text)
        .map_err(|e| CliError::runtime(&format!("could not write {}", path.display()), e))
}

fn read_text(path: &Path) -> Result<Option<String>, CliError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(CliError::runtime(
            &format!("could not read {}", path.display()),
            e,
        )),
    }
}

fn parse_object(path: &Path, text: Option<&str>) -> Result<Map<String, Value>, CliError> {
    match text {
        None => Ok(Map::new()),
        Some(t) if t.trim().is_empty() => Ok(Map::new()),
        Some(t) => match serde_json::from_str::<Value>(t) {
            Ok(Value::Object(map)) => Ok(map),
            Ok(_) => Err(CliError::Usage(format!(
                "{} is not a JSON object; not touching it",
                path.display()
            ))),
            Err(e) => Err(CliError::Usage(format!(
                "{} is not valid JSON ({e}); not touching it",
                path.display()
            ))),
        },
    }
}

fn render(map: &Map<String, Value>) -> String {
    serde_json::to_string_pretty(&Value::Object(map.clone())).unwrap_or_default() + "\n"
}

/// Whether a hook entry is one of ours: it runs `hook cursor`.
fn is_ours(entry: &Value) -> bool {
    entry
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| c.ends_with(" hook cursor") || c.contains(" hook cursor --"))
}

/// Whether a `preToolUse` entry would see `Shell` calls. A matcher is a
/// regular expression over the tool name; an absent or empty one matches
/// every tool.
fn matches_shell(entry: &Value) -> bool {
    match entry.get("matcher").and_then(Value::as_str) {
        None | Some("") => true,
        Some(pattern) => regex::Regex::new(pattern)
            .map(|re| re.is_match("Shell"))
            .unwrap_or_else(|_| pattern.contains("Shell")),
    }
}

/// Other tools' hooks on the events this registers, as `(event, command)`.
fn others(hooks: &Map<String, Value>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (event, _) in events(true) {
        if let Some(Value::Array(entries)) = hooks.get(event) {
            for entry in entries.iter().filter(|e| !is_ours(e)) {
                let command = entry
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("(no command)")
                    .to_string();
                out.push((event.to_string(), command));
            }
        }
    }
    out
}

/// Run `kompressor hook install cursor`.
pub fn install(args: InstallArgs) -> CliResult {
    let scope = scope(args.project)?;
    let exe =
        super::exe().ok_or_else(|| CliError::Runtime("could not find this executable".into()))?;
    let (config, source, needs_copy) = resolve_config(args.config.as_deref())?;

    if args.print {
        // Nothing written. A configuration that would need pinning is named
        // as the file it came from, or left out.
        let config_path = source.as_deref().or(config.source.as_deref());
        let command = hook_command(&exe, config_path, args.shadow);
        let hooks: Map<String, Value> = hook_entries(&command, args.shadow)
            .into_iter()
            .map(|(event, entry)| (event, Value::Array(vec![entry])))
            .collect();
        let mut out = json!({ "hooks.json": { "version": 1, "hooks": hooks } });
        if !args.shadow {
            out["mcp.json"] = json!({ "mcpServers": {
                crate::wrap::mcp::SERVER_NAME: mcp_entry(&exe, config_path)
            }});
        }
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return Ok(());
    }

    let hooks_path = scope.cursor_dir.join("hooks.json");
    let mcp_path = scope.cursor_dir.join("mcp.json");

    // A previous install of ours is replaced, not stacked.
    if scope.sidecar.exists() {
        remove(&scope, true)?;
    }

    // Check everything before writing anything.
    let hooks_text = read_text(&hooks_path)?;
    let mut hooks_doc = parse_object(&hooks_path, hooks_text.as_deref())?;
    let existing_hooks = hooks_doc
        .get("hooks")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if !args.force {
        // Our own hooks in another file Cursor reads: both sets would run on
        // every event. Measured on CLI 2026.11.22 with a user and a project
        // install at once — each MCP result was recorded twice, and the
        // losing shell rewrite left its capture file behind.
        let elsewhere: Vec<String> = other_hook_files(&hooks_path)
            .into_iter()
            .filter(|path| hook_entries_in(path).iter().any(|(_, e)| is_ours(e)))
            .map(|path| path.display().to_string())
            .collect();
        if !elsewhere.is_empty() {
            return Err(CliError::Usage(format!(
                "Kompressor's hooks are already installed in {}. Cursor runs every hooks file \
                 it finds, so both sets would answer each event and every result would be \
                 counted twice; nothing was written. Uninstall that one first \
                 (`kompressor hook uninstall cursor`, with --project for a workspace), or \
                 pass --force.",
                elsewhere.join(", ")
            )));
        }
        let mut rival: Vec<String> = existing_hooks
            .get("preToolUse")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|e| !is_ours(e) && matches_shell(e))
            .map(|e| {
                e.get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string()
            })
            .collect();
        // The other files Cursor reads count as well: a rewriter in the
        // workspace or the enterprise file collides just the same.
        for path in other_hook_files(&hooks_path) {
            for (event, entry) in hook_entries_in(&path) {
                if event == "preToolUse" && !is_ours(&entry) && matches_shell(&entry) {
                    let command = entry.get("command").and_then(Value::as_str).unwrap_or("?");
                    rival.push(format!("{command} (in {})", path.display()));
                }
            }
        }
        if !rival.is_empty() {
            return Err(CliError::Usage(format!(
                "{} already has a preToolUse hook on Shell: {}. Two hooks rewriting one \
                 command overwrite each other (the last reply wins), so nothing was written. \
                 Pass --force to install beside it anyway.",
                hooks_path.display(),
                rival.join(", ")
            )));
        }
    }
    let mcp_text = read_text(&mcp_path)?;
    let mut mcp_doc = parse_object(&mcp_path, mcp_text.as_deref())?;
    let server = mcp_entry(
        &exe,
        if needs_copy {
            Some(scope.pinned_config.as_path())
        } else {
            source.as_deref()
        },
    );
    if !args.shadow
        && let Some(present) = mcp_doc
            .get("mcpServers")
            .and_then(|s| s.get(crate::wrap::mcp::SERVER_NAME))
        && present != &server
    {
        return Err(CliError::Usage(format!(
            "{} already registers a server named {} that this did not write; nothing was \
             written. Remove or rename it first.",
            mcp_path.display(),
            crate::wrap::mcp::SERVER_NAME
        )));
    }

    // Now write. The statistics database first: see `prepare_stats`.
    super::prepare_stats(&config);
    let config_path = if needs_copy {
        write_pinned(&config, &scope.pinned_config)?;
        Some(scope.pinned_config.clone())
    } else {
        source.clone()
    };
    std::fs::create_dir_all(&scope.cursor_dir).map_err(|e| {
        CliError::runtime(
            &format!("could not create {}", scope.cursor_dir.display()),
            e,
        )
    })?;

    let command = hook_command(&exe, config_path.as_deref(), args.shadow);
    let mut owned_hooks = Vec::new();
    hooks_doc.entry("version").or_insert(json!(1));
    let hooks = hooks_doc
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(hooks) = hooks else {
        return Err(CliError::Usage(format!(
            "{}: `hooks` is not an object; not touching it",
            hooks_path.display()
        )));
    };
    for (event, entry) in hook_entries(&command, args.shadow) {
        let list = hooks.entry(event.clone()).or_insert_with(|| json!([]));
        let Value::Array(list) = list else {
            return Err(CliError::Usage(format!(
                "{}: hooks.{event} is not an array; not touching it",
                hooks_path.display()
            )));
        };
        list.push(entry.clone());
        owned_hooks.push(Owned {
            section: "hooks".into(),
            key: event,
            value: entry,
        });
    }
    let hooks_written = render(&hooks_doc);

    let mut sidecar = Sidecar {
        scope: scope.label.clone(),
        installed_ms: super::now_ms(),
        files: Vec::new(),
        config: config_path.clone(),
        config_written: needs_copy,
        allow_list: None,
        allow_entries: Vec::new(),
        shadow: args.shadow,
    };

    write_file(&hooks_path, &hooks_written)?;
    sidecar.files.push(FileClaim {
        path: hooks_path.clone(),
        original: hooks_text,
        written: hooks_written,
        entries: owned_hooks,
    });

    if !args.shadow {
        let server = mcp_entry(&exe, config_path.as_deref());
        let servers = mcp_doc
            .entry("mcpServers")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(servers) = servers {
            servers.insert(crate::wrap::mcp::SERVER_NAME.into(), server.clone());
        }
        let written = render(&mcp_doc);
        write_file(&mcp_path, &written)?;
        sidecar.files.push(FileClaim {
            path: mcp_path.clone(),
            original: mcp_text,
            written,
            entries: vec![Owned {
                section: "mcpServers".into(),
                key: crate::wrap::mcp::SERVER_NAME.into(),
                value: server,
            }],
        });
    }

    if args.allow_list
        && let Some(home) = super::cursor::cursor_home()
    {
        let cli = home.join("cli-config.json");
        let added = add_allow(&cli, &allow_entries(&exe))?;
        if !added.is_empty() {
            eprintln!(
                "  allow-list: added {} to {}",
                added.join(", "),
                cli.display()
            );
            sidecar.allow_list = Some(cli);
            sidecar.allow_entries = added;
        }
    }

    save_sidecar(&scope.sidecar, &sidecar)?;

    eprintln!(
        "kompressor: Cursor hooks installed ({}{})",
        scope.label,
        if args.shadow {
            ", shadow: measuring only"
        } else {
            ""
        }
    );
    eprintln!("  {}", hooks_path.display());
    if !args.shadow {
        eprintln!(
            "  {}  (server {})",
            mcp_path.display(),
            crate::wrap::mcp::SERVER_NAME
        );
    }
    if let Some(config) = &config_path {
        eprintln!("  configuration: {}", config.display());
    }
    if config.hooks.cursor.shell.enabled
        && config.hooks.cursor.shell.only_when_approved
        && !shell_would_run(&exe)
    {
        eprintln!(
            "  shell output: idle until the Cursor CLI would run the rewritten command. Its \
             allow-list judges the command word literally: add {} to it, or rerun with \
             --allow-list to have them added (or set approvalMode to unrestricted). A \
             workspace .cursor/cli.json that sets permissions.allow replaces the global \
             list, so the entries belong there too.",
            allow_entries(&exe).join(", ")
        );
    }
    eprintln!("  `kompressor hook status cursor` shows when a hook last ran.");
    Ok(())
}

fn write_file(path: &Path, text: &str) -> CliResult {
    std::fs::write(path, text)
        .map_err(|e| CliError::runtime(&format!("could not write {}", path.display()), e))
}

fn save_sidecar(path: &Path, sidecar: &Sidecar) -> CliResult {
    let text = serde_json::to_string_pretty(sidecar)
        .map_err(|e| CliError::runtime("could not render the install record", e))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CliError::runtime(&format!("could not create {}", parent.display()), e))?;
    }
    // Owner-only: it holds a copy of files from ~/.cursor.
    crate::wrap::paths::write_private(path, &text)
        .map_err(|e| CliError::runtime(&format!("could not write {}", path.display()), e))
}

fn load_sidecar(path: &Path) -> Result<Option<Sidecar>, CliError> {
    match read_text(path)? {
        None => Ok(None),
        Some(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| CliError::Runtime(format!("{} is damaged: {e}", path.display()))),
    }
}

/// Whether the CLI, from this directory, would approve a rewrite naming this
/// binary as a hook started with this process's `PATH` would name it.
fn shell_would_run(exe: &Path) -> bool {
    let path = std::env::var_os("PATH");
    let here: Vec<PathBuf> = std::env::current_dir().into_iter().collect();
    super::shell::program_word(exe, path.as_deref())
        .is_some_and(|word| super::cursor::cli_approves(&word, &here))
}

/// The allow-list entries that approve this binary as a rewrite names it.
///
/// Cursor matches the command word literally (`shell::program_word`): the
/// bare name when `PATH` finds this binary, the unquoted absolute path
/// otherwise. Which of the two a hook writes depends on the `PATH` Cursor
/// gives it, which the desktop and the CLI need not share, so both are added.
///
/// And the retrieval tool, `Mcp(*:optscale_kompressor_retrieve)`: under an
/// allow-list every MCP tool call needs approval too, and a refused retrieval
/// leaves the model a marker it cannot resolve. `*` because the server is
/// `optscale_kompressor` when installed and `plugin-kompressor-…` under
/// `wrap`. It reads back fragments and nothing else.
fn allow_entries(exe: &Path) -> Vec<String> {
    let mut entries = vec![
        ALLOW_ENTRY.to_string(),
        "Mcp(*:optscale_kompressor_retrieve)".to_string(),
    ];
    if let Some(word) = super::shell::program_word(exe, None)
        && !word.starts_with('\'')
    {
        entries.push(format!("Shell({word})"));
    }
    entries
}

/// Add `entries` to the CLI's allow-list; the ones that were not there yet.
fn add_allow(path: &Path, entries: &[String]) -> Result<Vec<String>, CliError> {
    let text = read_text(path)?;
    let mut doc = parse_object(path, text.as_deref())?;
    let permissions = doc
        .entry("permissions")
        .or_insert_with(|| json!({ "allow": [], "deny": [] }));
    let Some(allow) = permissions
        .as_object_mut()
        .map(|p| p.entry("allow").or_insert_with(|| json!([])))
        .and_then(Value::as_array_mut)
    else {
        return Ok(Vec::new());
    };
    let added: Vec<String> = entries
        .iter()
        .filter(|entry| !allow.iter().any(|v| v == entry.as_str()))
        .cloned()
        .collect();
    if added.is_empty() {
        return Ok(added);
    }
    allow.extend(added.iter().map(|entry| json!(entry)));
    write_file(path, &render(&doc))?;
    Ok(added)
}

const ALLOW_ENTRY: &str = "Shell(kompressor)";

/// Remove exactly the entries an install added.
fn remove_allow(path: &Path, entries: &[String]) {
    let Ok(Some(text)) = read_text(path) else {
        return;
    };
    let Ok(mut doc) = parse_object(path, Some(&text)) else {
        return;
    };
    if let Some(allow) = doc
        .get_mut("permissions")
        .and_then(|p| p.get_mut("allow"))
        .and_then(Value::as_array_mut)
    {
        let before = allow.len();
        allow.retain(|v| !entries.iter().any(|entry| v == entry.as_str()));
        if allow.len() != before {
            let _ = write_file(path, &render(&doc));
        }
    }
}

/// Run `kompressor hook uninstall cursor`.
pub fn uninstall(project: bool) -> CliResult {
    let scope = scope(project)?;
    if !scope.sidecar.exists() {
        eprintln!(
            "kompressor: no Cursor install of ours ({}); nothing to remove",
            scope.label
        );
        return Ok(());
    }
    remove(&scope, false)
}

/// Undo what the sidecar records. `quiet` for a reinstall.
fn remove(scope: &Scope, quiet: bool) -> CliResult {
    let Some(sidecar) = load_sidecar(&scope.sidecar)? else {
        return Ok(());
    };
    let mut left = Vec::new();
    for claim in &sidecar.files {
        let current = read_text(&claim.path)?;
        match current {
            Some(ref text) if *text == claim.written => match &claim.original {
                Some(original) => write_file(&claim.path, original)?,
                None => {
                    let _ = std::fs::remove_file(&claim.path);
                }
            },
            None => {}
            Some(text) => {
                // Edited since: remove exactly our entries.
                let mut doc = parse_object(&claim.path, Some(&text))?;
                for owned in &claim.entries {
                    if !remove_entry(&mut doc, owned) {
                        left.push(format!(
                            "{}: {}.{}",
                            claim.path.display(),
                            owned.section,
                            owned.key
                        ));
                    }
                }
                write_file(&claim.path, &render(&doc))?;
            }
        }
    }
    if let Some(cli) = &sidecar.allow_list {
        // A record from before the entries were listed added the one.
        let entries = if sidecar.allow_entries.is_empty() {
            vec![ALLOW_ENTRY.to_string()]
        } else {
            sidecar.allow_entries.clone()
        };
        remove_allow(cli, &entries);
    }
    if sidecar.config_written
        && let Some(config) = &sidecar.config
    {
        let _ = std::fs::remove_file(config);
    }
    let _ = std::fs::remove_file(&scope.sidecar);
    if !quiet {
        eprintln!("kompressor: Cursor hooks removed ({})", scope.label);
    }
    for entry in &left {
        eprintln!(
            "kompressor: left in place, edited since it was installed: {entry}. Remove it by hand \
             if it is no longer wanted."
        );
    }
    Ok(())
}

/// Remove one owned entry if it is still exactly as written.
fn remove_entry(doc: &mut Map<String, Value>, owned: &Owned) -> bool {
    let Some(section) = doc.get_mut(&owned.section).and_then(Value::as_object_mut) else {
        return false;
    };
    match owned.section.as_str() {
        "hooks" => {
            let Some(list) = section.get_mut(&owned.key).and_then(Value::as_array_mut) else {
                return false;
            };
            let Some(at) = list.iter().position(|v| *v == owned.value) else {
                return false;
            };
            list.remove(at);
            if list.is_empty() {
                section.remove(&owned.key);
            }
            true
        }
        _ => {
            if section.get(&owned.key) == Some(&owned.value) {
                section.remove(&owned.key);
                true
            } else {
                false
            }
        }
    }
}

/// The other hooks files Cursor reads besides `own`: the user's, the
/// current workspace's and the enterprise one, whichever of them `own` is not.
fn other_hook_files(own: &Path) -> Vec<PathBuf> {
    // The same file named two ways — `~/.cursor/hooks.json` is also the
    // workspace file when the workspace is the home directory. Compared by
    // directory, because the file itself may not exist yet.
    let same = |a: &Path, b: &Path| {
        let dir = |p: &Path| p.parent().and_then(|d| std::fs::canonicalize(d).ok());
        a == b || (a.file_name() == b.file_name() && dir(a).is_some() && dir(a) == dir(b))
    };
    let mut files = Vec::new();
    if let Some(home) = super::cursor::cursor_home() {
        files.push(home.join("hooks.json"));
    }
    if let Ok(here) = std::env::current_dir() {
        files.push(here.join(".cursor").join("hooks.json"));
    }
    files.push(PathBuf::from("/etc/cursor/hooks.json"));
    let mut out: Vec<PathBuf> = Vec::new();
    for file in files {
        if !same(&file, own) && !out.iter().any(|seen| same(seen, &file)) {
            out.push(file);
        }
    }
    out
}

/// Every hook entry in a hooks file, with its event. Empty when the file is
/// absent or unreadable.
fn hook_entries_in(path: &Path) -> Vec<(String, Value)> {
    read_text(path)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|doc| doc.get("hooks").and_then(Value::as_object).cloned())
        .map(|hooks| {
            hooks
                .into_iter()
                .filter_map(|(event, list)| Some((event, list.as_array()?.clone())))
                .flat_map(|(event, list)| list.into_iter().map(move |e| (event.clone(), e)))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether hooks of ours are registered where a session started here would
/// read them — at user level or in this workspace, by an install or by hand:
/// a `wrap` session would then get every hook twice.
pub fn installed_for_user() -> bool {
    if let Ok(here) = std::env::current_dir()
        && hook_entries_in(&here.join(".cursor").join("hooks.json"))
            .iter()
            .any(|(_, e)| is_ours(e))
    {
        return true;
    }
    let Ok(scope) = scope(false) else {
        return false;
    };
    if scope.sidecar.exists() {
        return true;
    }
    read_text(&scope.cursor_dir.join("hooks.json"))
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|doc| doc.get("hooks").cloned())
        .and_then(|hooks| hooks.as_object().cloned())
        .is_some_and(|hooks| {
            hooks
                .values()
                .filter_map(Value::as_array)
                .flatten()
                .any(is_ours)
        })
}

/// Run `kompressor hook status cursor`.
pub fn status(project: bool, config: Option<&Path>) -> CliResult {
    let scope = scope(project)?;
    let sidecar = load_sidecar(&scope.sidecar)?;
    println!(
        "Cursor hooks ({}, {})",
        scope.label,
        scope.cursor_dir.display()
    );
    match &sidecar {
        None => println!("  installed     no"),
        Some(s) => println!(
            "  installed     yes, {}{}",
            super::ago(s.installed_ms),
            if s.shadow {
                " (shadow: measuring only)"
            } else {
                ""
            }
        ),
    }
    if let Some(sidecar) = &sidecar {
        for claim in &sidecar.files {
            let doc = read_text(&claim.path)?
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            let states: Vec<String> = claim
                .entries
                .iter()
                .map(|owned| {
                    let present = match owned.section.as_str() {
                        "hooks" => doc
                            .get("hooks")
                            .and_then(|h| h.get(&owned.key))
                            .and_then(Value::as_array)
                            .is_some_and(|l| l.contains(&owned.value)),
                        _ => {
                            doc.get(&owned.section).and_then(|s| s.get(&owned.key))
                                == Some(&owned.value)
                        }
                    };
                    format!(
                        "{} {}",
                        owned.key,
                        if present { "ok" } else { "MISSING or edited" }
                    )
                })
                .collect();
            println!("  {:<13} {}", file_name(&claim.path), claim.path.display());
            println!("                {}", states.join(", "));
        }
    }

    let own_file = scope.cursor_dir.join("hooks.json");
    for path in other_hook_files(&own_file) {
        if hook_entries_in(&path).iter().any(|(_, e)| is_ours(e)) {
            println!(
                "  ALSO HERE     {}: Kompressor's hooks are installed there too, so each \
                 event runs both and every result is counted twice",
                path.display()
            );
        }
    }
    // What else answers the same events. The last reply wins (design/CURSOR.md, "Hooks").
    let mut sources = vec![
        scope.cursor_dir.join("hooks.json"),
        PathBuf::from("/etc/cursor/hooks.json"),
    ];
    if !project && let Ok(here) = std::env::current_dir() {
        sources.push(here.join(".cursor").join("hooks.json"));
    }
    let mut rivals = Vec::new();
    for source in sources {
        if let Ok(Some(text)) = read_text(&source)
            && let Ok(doc) = serde_json::from_str::<Value>(&text)
            && let Some(hooks) = doc.get("hooks").and_then(Value::as_object)
        {
            for (event, command) in others(hooks) {
                rivals.push(format!("{}: {event}: {command}", source.display()));
            }
        }
    }
    if rivals.is_empty() {
        println!("  other hooks   none on sessionStart, preToolUse, postToolUse or stop");
    } else {
        println!("  other hooks   on the same events (the last reply wins):");
        for rival in rivals {
            println!("                {rival}");
        }
    }

    let config_path = config
        .map(Path::to_path_buf)
        .or_else(|| sidecar.as_ref().and_then(|s| s.config.clone()));
    match Config::load(config_path.as_deref()) {
        Err(error) => println!("  config        cannot be loaded: {error}"),
        Ok(config) => {
            let hooks = &config.hooks.cursor;
            println!(
                "  config        {}",
                config_path
                    .as_deref()
                    .or(config.source.as_deref())
                    .map_or("(built-in defaults)".to_string(), |p| p
                        .display()
                        .to_string())
            );
            println!(
                "  compression   {}; MCP results {}; shell output {}",
                if !hooks.enabled {
                    "OFF (hooks.cursor.enabled = false)"
                } else if hooks.shadow || sidecar.as_ref().is_some_and(|s| s.shadow) {
                    "shadow (measuring, nothing changed)"
                } else {
                    "on"
                },
                if hooks.mcp.enabled { "on" } else { "off" },
                if !hooks.shell.enabled {
                    "off (hooks.cursor.shell.enabled)"
                } else if hooks.shell.only_when_approved
                    && !super::exe().is_some_and(|exe| shell_would_run(&exe))
                {
                    "on, but idle: the CLI would refuse the rewrite (add Shell(kompressor) to \
                     its allow-list, or `hook install cursor --allow-list`)"
                } else {
                    "on"
                }
            );
            match kompressor_core::build_ref_store(&config) {
                Ok(_) => println!("  store         {} (reachable)", config.refs.path.display()),
                Err(error) => println!(
                    "  store         {} NOT reachable: {error}",
                    config.refs.path.display()
                ),
            }
            println!("  statistics    {}", config.stats.path.display());
        }
    }
    let last = std::fs::read_to_string(super::last_call_file("cursor"))
        .ok()
        .and_then(|t| {
            let mut parts = t.split_whitespace();
            Some((
                parts.next()?.parse::<i64>().ok()?,
                parts.next()?.to_string(),
            ))
        });
    match last {
        Some((ms, event)) => println!("  last call     {} ({event})", super::ago(ms)),
        None => println!(
            "  last call     never. If hooks are installed and a session has run, Cursor is \
             not calling them: a team policy may disable user hooks."
        ),
    }
    Ok(())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Write the plugin directory `wrap cursor-agent` passes with `--plugin-dir`.
///
/// ```text
/// <dir>/.cursor-plugin/plugin.json   name, version, "mcpServers": "./mcp.json"
/// <dir>/hooks/hooks.json             sessionStart, preToolUse, postToolUse, stop
/// <dir>/mcp.json                     optscale_kompressor -> kompressor mcp serve
/// ```
///
/// The plugin's name is part of what the model sees — its server is offered as
/// `plugin-<name>-<server>` — so it is kept short.
pub fn write_plugin(dir: &Path, exe: &Path, config: &Path, shadow: bool) -> std::io::Result<()> {
    std::fs::create_dir_all(dir.join(".cursor-plugin"))?;
    std::fs::create_dir_all(dir.join("hooks"))?;
    let mut manifest = json!({
        "name": "kompressor",
        "displayName": "kompressor",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "OptScale Kompressor: compresses tool output for this session",
    });
    if !shadow {
        manifest["mcpServers"] = json!("./mcp.json");
    }
    let pretty = |v: &Value| serde_json::to_string_pretty(v).unwrap_or_default() + "\n";
    std::fs::write(
        dir.join(".cursor-plugin").join("plugin.json"),
        pretty(&manifest),
    )?;
    let command = hook_command(exe, Some(config), shadow);
    let hooks: Map<String, Value> = hook_entries(&command, shadow)
        .into_iter()
        .map(|(event, entry)| (event, Value::Array(vec![entry])))
        .collect();
    std::fs::write(
        dir.join("hooks").join("hooks.json"),
        pretty(&json!({ "version": 1, "hooks": hooks })),
    )?;
    if !shadow {
        std::fs::write(
            dir.join("mcp.json"),
            pretty(&json!({ "mcpServers": {
                crate::wrap::mcp::SERVER_NAME: mcp_entry(exe, Some(config))
            }})),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_matcher_reads_as_a_regular_expression() {
        assert!(matches_shell(&json!({"command": "x"})));
        assert!(matches_shell(&json!({"command": "x", "matcher": ""})));
        assert!(matches_shell(
            &json!({"command": "x", "matcher": "^Shell$"})
        ));
        assert!(matches_shell(
            &json!({"command": "x", "matcher": "Shell|Read"})
        ));
        assert!(!matches_shell(&json!({"command": "x", "matcher": "^MCP:"})));
        assert!(!matches_shell(&json!({"command": "x", "matcher": "Read"})));
    }

    #[test]
    fn shadow_mode_watches_every_tool_and_compressing_mode_only_mcp() {
        let shadow = hook_entries("k hook cursor", true);
        let post = shadow.iter().find(|(e, _)| e == "postToolUse").unwrap();
        assert_eq!(post.1["matcher"], "");
        let live = hook_entries("k hook cursor", false);
        let post = live.iter().find(|(e, _)| e == "postToolUse").unwrap();
        assert_eq!(post.1["matcher"], "^MCP:");
        for (_, entry) in live {
            assert_eq!(entry["timeout"], TIMEOUT_S);
            assert!(entry.get("failClosed").is_none(), "never fail closed");
        }
    }

    #[test]
    fn the_command_names_the_binary_and_its_configuration_absolutely() {
        let command = hook_command(Path::new("/opt/k"), Some(Path::new("/etc/c.toml")), false);
        if cfg!(windows) {
            assert_eq!(command, "\"/opt/k\" hook cursor --config \"/etc/c.toml\"");
        } else {
            assert_eq!(command, "'/opt/k' hook cursor --config '/etc/c.toml'");
        }
    }

    #[test]
    fn an_entry_edited_since_install_is_not_removed() {
        let owned = Owned {
            section: "hooks".into(),
            key: "stop".into(),
            value: json!({"command": "a hook cursor", "timeout": 5}),
        };
        let mut doc: Map<String, Value> = serde_json::from_value(json!({
            "hooks": {"stop": [{"command": "a hook cursor", "timeout": 9}]}
        }))
        .unwrap();
        assert!(!remove_entry(&mut doc, &owned));
        assert_eq!(doc["hooks"]["stop"][0]["timeout"], 9);
    }
}
