//! Pointing opencode at the proxy through its own configuration.
//!
//! opencode 2.x needs this module because neither of the mechanisms the rest
//! of `wrap` relies on reaches it:
//!
//! * **Environment variables break it.** `OPENAI_BASE_URL` or
//!   `ANTHROPIC_BASE_URL` in the environment makes opencode 2.2.4 report every
//!   model of that provider as `Model unavailable` — measured against a
//!   recording listener, which received nothing. The variable the catalogue
//!   used to set for it turned a working agent into one that cannot start a
//!   turn.
//! * **The process that calls the model is not the one we launch.** `opencode`
//!   is a client of a long-lived background service (`opencode serve
//!   --service`, recorded in `~/.local/state/opencode/service.json`), and that
//!   service makes every provider request with the environment it was started
//!   with. OpenCode Desktop adopts the same service, or spawns it with its own
//!   bundled CLI.
//!
//! What does reach it is configuration: `providers.<id>.settings.baseURL`
//! steers a provider (the legacy `provider.<id>.options.baseURL` too, and the
//! v2 spelling wins when a file holds both), and two ways of delivering it fit
//! the two ways a session runs:
//!
//! * **Launched** (`kompressor wrap opencode`): the overlay goes into
//!   `OPENCODE_CONFIG_CONTENT`, which outranks every config file, and
//!   `--standalone` gives the session a private server that inherits it.
//!   Nothing is written to disk.
//! * **Attached** (`kompressor wrap opencode --attach`): the overlay is written
//!   into the global `opencode.json` and `opencode reload` makes the running
//!   service re-read it — measured: the next request went to the new URL
//!   without a restart. This is the only way in for Desktop and for a TUI
//!   already connected to the service. The file is put back on exit.
//!
//! # Whose provider is it
//!
//! A `baseURL` the user set is a decision, usually a gateway. Overriding it
//! would send the gateway's key to the vendor's own host, so a provider routed
//! elsewhere already is left alone and named, rather than claimed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value};

use super::settings::{
    Claim, Holder, Lock, Released, let_go, read_owners, sibling, sweep, write_owners,
};
use crate::error::CliError;

/// The providers this wrap routes, and the path each one's base URL takes on
/// the proxy.
///
/// The vendor SDKs append the API path themselves — `/responses`,
/// `/messages`, `/chat/completions` — so each base URL carries the version
/// segment the vendor's own default does. OpenCode Zen and Go share a host
/// and differ by prefix, which a single `/zen` mount forwards unchanged.
pub const PROVIDERS: &[(&str, &str)] = &[
    ("openai", "/v1"),
    ("anthropic", "/v1"),
    ("opencode", "/zen/v1"),
    ("opencode-go", "/zen/go/v1"),
];

/// The transport opencode is told to use for OpenAI Responses.
///
/// Left alone, opencode opens a WebSocket for the main turn. The proxy relays
/// it and compresses its frames, but a relayed turn reports no usage and runs
/// no retrieve loop, so the session would be compressed without being
/// measurable or recoverable. `http` keeps both — measured, the turn then
/// reported its prompt tokens. The key exists in 2.0.11 and 2.2.4 alike.
const OPENAI_TRANSPORT: &str = "http";

/// The variable a launched session's overlay travels in. It outranks every
/// config file opencode reads.
pub const CONTENT_VAR: &str = "OPENCODE_CONFIG_CONTENT";

/// The ownership sidecar's key for the file as a whole.
const FILE_KEY: &str = "\u{1}opencode-config";

/// The dotted path of a provider setting.
fn setting(provider: &str, name: &str) -> String {
    format!("providers.{provider}.settings.{name}")
}

/// The base URL this wrap writes for `provider`.
fn base_url(port: u16, path: &str) -> String {
    format!("http://127.0.0.1:{port}{path}")
}

/// Whether `url` is one this command wrote, on any port.
///
/// A concurrent `--attach` leaves its URL in the global file, and treating
/// that as the user's gateway would make every later launch skip every
/// provider. Only the exact shape is recognised: a different local service on
/// a loopback port is someone's decision and stays theirs.
fn is_ours(provider: &str, url: &str) -> bool {
    let Some(path) = PROVIDERS
        .iter()
        .find(|(id, _)| *id == provider)
        .map(|(_, path)| *path)
    else {
        return false;
    };
    url.strip_prefix("http://127.0.0.1:")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(port, tail)| {
            !port.is_empty()
                && port.bytes().all(|b| b.is_ascii_digit())
                && format!("/{tail}") == path
        })
}

/// Remove `//` and `/* */` comments and trailing commas, so a JSONC file can
/// be read with a strict parser.
///
/// opencode accepts both in its config files. Only ever used to *read*: a file
/// that needed it is never written back, because the comments would not
/// survive the trip.
pub fn strip_jsonc(text: &str) -> String {
    let mut bare = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            bare.push(c);
            if c == '\\' {
                if let Some(next) = chars.next() {
                    bare.push(next);
                }
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                bare.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                while chars.peek().is_some_and(|&next| next != '\n') {
                    chars.next();
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = '\0';
                for next in chars.by_ref() {
                    if previous == '*' && next == '/' {
                        break;
                    }
                    previous = next;
                }
                bare.push(' ');
            }
            _ => bare.push(c),
        }
    }

    // Trailing commas, in a second string-aware pass now that no comment can
    // sit between the comma and the bracket it precedes.
    let chars: Vec<char> = bare.chars().collect();
    let mut out = String::with_capacity(bare.len());
    let mut in_string = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 1;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if c == ',' {
            let next = chars[i + 1..].iter().find(|ch| !ch.is_whitespace());
            if !matches!(next, Some('}') | Some(']')) {
                out.push(c);
            }
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

/// Parse a config file's text, accepting what opencode accepts.
fn parse_lenient(text: &str) -> Option<Map<String, Value>> {
    if text.trim().is_empty() {
        return Some(Map::new());
    }
    serde_json::from_str::<Value>(&strip_jsonc(text))
        .ok()
        .and_then(|value| value.as_object().cloned())
}

/// Every provider a config routes somewhere, with where.
///
/// Both spellings, because a file may use either and opencode honours both.
fn routes_in(config: &Map<String, Value>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (section, inner) in [("providers", "settings"), ("provider", "options")] {
        let Some(Value::Object(providers)) = config.get(section) else {
            continue;
        };
        for (id, provider) in providers {
            if let Some(url) = provider
                .get(inner)
                .and_then(|settings| settings.get("baseURL"))
                .and_then(Value::as_str)
            {
                out.entry(id.clone()).or_insert_with(|| url.to_string());
            }
        }
    }
    out
}

/// The providers the user has routed themselves, across every source given.
fn user_routes(sources: &[Map<String, Value>]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for source in sources {
        for (id, url) in routes_in(source) {
            if !is_ours(&id, &url) {
                out.entry(id).or_insert(url);
            }
        }
    }
    out
}

/// What a wrap did with each provider, for the line it prints.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Routing {
    /// Providers now pointed at the proxy.
    pub routed: Vec<&'static str>,
    /// Providers left on a base URL the user chose, with that URL.
    pub kept: Vec<(&'static str, String)>,
}

impl Routing {
    /// Say what was done, on stderr, where the rest of `wrap` talks.
    pub fn report(&self, port: u16) {
        if !self.routed.is_empty() {
            eprintln!(
                "kompressor: opencode providers through 127.0.0.1:{port}: {}",
                self.routed.join(", ")
            );
        }
        for (id, url) in &self.kept {
            eprintln!(
                "kompressor: opencode provider {id} left on its own baseURL ({url}); \
                 it is not compressed"
            );
        }
    }
}

/// Write the overlay's settings into `config`, skipping user-routed providers.
fn apply_overlay(
    config: &mut Map<String, Value>,
    port: u16,
    taken: &BTreeMap<String, String>,
) -> Routing {
    let mut routing = Routing::default();
    for (id, path) in PROVIDERS {
        if let Some(url) = taken.get(*id) {
            routing.kept.push((id, url.clone()));
            continue;
        }
        set_path(config, &setting(id, "baseURL"), Some(base_url(port, path)));
        if *id == "openai" && get_path(config, &setting(id, "transport")).is_none() {
            set_path(
                config,
                &setting(id, "transport"),
                Some(OPENAI_TRANSPORT.to_string()),
            );
        }
        routing.routed.push(id);
    }
    routing
}

fn get_path<'a>(config: &'a Map<String, Value>, dotted: &str) -> Option<&'a Value> {
    let mut parts = dotted.split('.');
    let mut current = config.get(parts.next()?)?;
    for part in parts {
        current = current.as_object()?.get(part)?;
    }
    Some(current)
}

/// Set, or with `None` remove, the string at a dotted path.
///
/// Removal prunes every object it leaves empty, so a key this command created
/// takes its parents with it — the file then compares equal to the original
/// and is put back byte for byte.
fn set_path(config: &mut Map<String, Value>, dotted: &str, value: Option<String>) {
    let parts: Vec<&str> = dotted.split('.').collect();
    set_in(config, &parts, value);
}

fn set_in(map: &mut Map<String, Value>, parts: &[&str], value: Option<String>) {
    let [head, rest @ ..] = parts else { return };
    if rest.is_empty() {
        match value {
            Some(value) => {
                map.insert((*head).to_string(), Value::String(value));
            }
            None => {
                map.remove(*head);
            }
        }
        return;
    }
    if value.is_none() && !map.contains_key(*head) {
        return;
    }
    let child = map
        .entry((*head).to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !child.is_object() {
        // Not ours to reshape. Only reachable on removal, from a file edited
        // mid-session into something this command never wrote.
        if value.is_none() {
            return;
        }
        *child = Value::Object(Map::new());
    }
    let Some(inner) = child.as_object_mut() else {
        return;
    };
    set_in(inner, rest, value);
    if inner.is_empty() {
        map.remove(*head);
    }
}

/// opencode's global config directory, as opencode resolves it.
fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("opencode")
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The config files opencode would read for a session started in `cwd`.
///
/// Read to learn which providers the user has routed, never written. The
/// project walk stops at the repository root, where opencode's own does.
fn config_files(cwd: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let global = config_dir();
    files.push(global.join("opencode.json"));
    files.push(global.join("opencode.jsonc"));
    if let Some(dir) = std::env::var_os("OPENCODE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        let dir = PathBuf::from(dir);
        files.push(dir.join("opencode.json"));
        files.push(dir.join("opencode.jsonc"));
    }
    if let Some(file) = std::env::var_os("OPENCODE_CONFIG").filter(|f| !f.is_empty()) {
        files.push(PathBuf::from(file));
    }
    for dir in cwd.ancestors().take(64) {
        files.push(dir.join("opencode.json"));
        files.push(dir.join("opencode.jsonc"));
        files.push(dir.join(".opencode").join("opencode.json"));
        files.push(dir.join(".opencode").join("opencode.jsonc"));
        if dir.join(".git").exists() {
            break;
        }
    }
    files
}

/// `OPENCODE_CONFIG_CONTENT` for a launched session.
///
/// `existing` is the variable as the user's shell has it. It is extended, not
/// replaced: it outranks every file, so whatever the user put there is the
/// most deliberate configuration in play, and a provider it routes stays
/// routed.
pub fn launch_content(
    port: u16,
    existing: Option<&str>,
    files: &[Map<String, Value>],
) -> Result<(String, Routing), CliError> {
    let mut content = match existing {
        Some(text) => parse_lenient(text).ok_or_else(|| {
            CliError::Usage(
                "OPENCODE_CONFIG_CONTENT is set but is not a JSON object, so it cannot be \
                 extended; unset it or fix it"
                    .into(),
            )
        })?,
        None => Map::new(),
    };
    let mut sources = vec![content.clone()];
    sources.extend(files.iter().cloned());
    let taken = user_routes(&sources);
    let routing = apply_overlay(&mut content, port, &taken);
    let text = serde_json::to_string(&Value::Object(content))
        .map_err(|e| CliError::runtime("could not render the opencode overlay", e))?;
    Ok((text, routing))
}

/// The environment a launched session needs, read from this process's own.
pub fn launch_env(port: u16) -> Result<(String, Routing), CliError> {
    let cwd = std::env::current_dir()
        .map_err(|e| CliError::runtime("could not read the working directory", e))?;
    let files: Vec<Map<String, Value>> = config_files(&cwd)
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .filter_map(|text| parse_lenient(&text))
        .collect();
    let existing = std::env::var(CONTENT_VAR).ok();
    launch_content(port, existing.as_deref(), &files)
}

/// Arguments that go before the user's own.
///
/// `--standalone` is the whole point: without it the session is a client of
/// the background service, which never sees this command's environment, and
/// the wrap would route nothing while every symptom looked like success. A
/// `--server` of the user's choosing is the same trap by request, so it is
/// refused with the way that does work.
pub fn launch_args(user: &[String]) -> Result<Vec<String>, CliError> {
    let flags = user.iter().take_while(|arg| arg.as_str() != "--");
    let mut standalone = false;
    for arg in flags {
        if arg == "--server" || arg.starts_with("--server=") {
            return Err(CliError::Usage(
                "--server connects opencode to a server this command does not start, so \
                 nothing it sends would reach the proxy. Point that server's own opencode.json \
                 at the proxy, or use `kompressor wrap opencode --attach` for the local \
                 background service."
                    .into(),
            ));
        }
        standalone |= arg == "--standalone";
    }
    Ok(if standalone {
        Vec::new()
    } else {
        vec!["--standalone".to_string()]
    })
}

/// A config file this command edits for the background service.
pub struct Target {
    pub path: PathBuf,
}

impl Target {
    /// The global file the service and Desktop read.
    ///
    /// `opencode.jsonc` when it exists, because opencode reads it after
    /// `opencode.json` and lets it win: an overlay written to the loser would
    /// be shadowed by any provider the other file mentions. Measured on
    /// 2.2.4.
    pub fn global() -> Target {
        Target::in_dir(&config_dir())
    }

    pub fn in_dir(dir: &Path) -> Target {
        let jsonc = dir.join("opencode.jsonc");
        Target {
            path: if jsonc.exists() {
                jsonc
            } else {
                dir.join("opencode.json")
            },
        }
    }

    fn owners_path(&self) -> PathBuf {
        sibling(&self.path, ".kompressor-wrap.json")
    }

    fn lock_path(&self) -> PathBuf {
        sibling(&self.path, ".kompressor-wrap.lock")
    }
}

/// Read the file for editing.
///
/// Strict JSON only. A file that parses only once its comments are stripped
/// would lose them on the way back out, and a user's annotated configuration
/// is not a fair price for a session's compression.
fn read_for_edit(path: &Path) -> Result<(Option<String>, Map<String, Value>), CliError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((None, Map::new())),
        Err(e) => {
            return Err(CliError::runtime(
                &format!("could not read {}", path.display()),
                e,
            ));
        }
    };
    if text.trim().is_empty() {
        return Ok((Some(text), Map::new()));
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Ok((Some(text), map)),
        Ok(_) => Err(CliError::Usage(format!(
            "{} does not hold a JSON object; left alone",
            path.display()
        ))),
        Err(_) if parse_lenient(&text).is_some() => Err(CliError::Usage(format!(
            "{} has comments or trailing commas, which rewriting it would lose. Left \
             alone: move the comments out, or use `kompressor wrap opencode` without \
             --attach, which writes no file",
            path.display()
        ))),
        Err(e) => Err(CliError::Usage(format!(
            "{} is not valid JSON ({e}); left alone",
            path.display()
        ))),
    }
}

fn write_map(path: &Path, map: &Map<String, Value>) -> Result<(), CliError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| CliError::runtime(&format!("could not create {}", dir.display()), e))?;
    }
    let text = serde_json::to_string_pretty(&Value::Object(map.clone()))
        .map_err(|e| CliError::runtime("could not render the opencode config", e))?;
    std::fs::write(path, text + "\n")
        .map_err(|e| CliError::runtime(&format!("could not write {}", path.display()), e))
}

/// The dotted keys this command may write.
fn owned_keys() -> Vec<String> {
    let mut keys: Vec<String> = PROVIDERS
        .iter()
        .map(|(id, _)| setting(id, "baseURL"))
        .collect();
    keys.push(setting("openai", "transport"));
    keys
}

/// Whether the value now at `key` is still the one this command wrote.
fn still_ours(key: &str, value: Option<&Value>) -> bool {
    let Some(value) = value.and_then(Value::as_str) else {
        return false;
    };
    let mut parts = key.split('.');
    let provider = parts.nth(1).unwrap_or_default();
    if key.ends_with(".transport") {
        value == OPENAI_TRANSPORT
    } else {
        is_ours(provider, value)
    }
}

/// Point the file's providers at the proxy, recording what was there.
pub fn claim(target: &Target, port: u16) -> Result<Routing, CliError> {
    let _lock = Lock::acquire(target.lock_path())?;
    let owners_path = target.owners_path();
    let mut owners = read_owners(&owners_path);
    let (text, mut map) = read_for_edit(&target.path)?;

    let mut file = owners.keys.remove(FILE_KEY).unwrap_or_default();
    sweep(&mut file, &owners_path);
    // A session killed before it restored leaves its per-key originals behind,
    // and those — not the file it left pointing at a dead proxy — are what the
    // user had. Its record of the whole file is kept for the same reason.
    let leftover = owned_keys().iter().any(|key| owners.keys.contains_key(key));
    if file.holders.is_empty() && !leftover {
        file.original = text.clone();
    }
    file.holders.retain(|h| h.pid != std::process::id());
    file.holders.push(Holder::me(&target.path));

    // A key we already hold is ours whatever it says; anything else that
    // routes a provider is the user's.
    let mut visible = map.clone();
    for key in owned_keys() {
        if owners.keys.contains_key(&key) {
            set_path(&mut visible, &key, None);
        }
    }
    let taken = user_routes(&[visible]);
    for key in owned_keys() {
        let provider = key.split('.').nth(1).unwrap_or_default();
        if taken.contains_key(provider) || owners.keys.contains_key(&key) {
            continue;
        }
        let original = get_path(&map, &key)
            .and_then(Value::as_str)
            .map(str::to_string);
        // A transport the user chose is theirs to keep, and nothing of ours is
        // written over it below.
        if key.ends_with(".transport") && original.is_some() {
            continue;
        }
        owners.keys.insert(
            key,
            Claim {
                original,
                holders: Vec::new(),
            },
        );
    }

    let before = map.clone();
    let routing = apply_overlay(&mut map, port, &taken);
    if map != before {
        write_map(&target.path, &map)?;
    }
    owners.keys.insert(FILE_KEY.to_string(), file);
    write_owners(&owners_path, &owners)?;
    Ok(routing)
}

/// Give up this session's claim, restoring the file if it was the last one.
///
/// `force` is `unwrap`: every claim is dropped whoever holds it.
pub fn release(target: &Target, force: bool) -> Result<Released, CliError> {
    let _lock = Lock::acquire(target.lock_path())?;
    let owners_path = target.owners_path();
    let mut owners = read_owners(&owners_path);
    // This process's claim ends here whichever way the release goes, as in
    // `configfile::release`: the lock next to the file is what proved it alive.
    let_go(&target.path);
    let keys: Vec<String> = owned_keys()
        .into_iter()
        .filter(|key| owners.keys.contains_key(key))
        .collect();
    let file = owners.keys.remove(FILE_KEY);
    if file.is_none() && keys.is_empty() {
        return Ok(Released::NotOurs);
    }
    let mut file = file.unwrap_or_default();
    sweep(&mut file, &owners_path);
    if force {
        file.holders.clear();
    } else {
        file.holders.retain(|h| h.pid != std::process::id());
    }
    if !file.holders.is_empty() {
        owners.keys.insert(FILE_KEY.to_string(), file);
        write_owners(&owners_path, &owners)?;
        return Ok(Released::StillInUse);
    }

    let current = match std::fs::read_to_string(&target.path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(CliError::runtime(
                &format!("could not read {}", target.path.display()),
                e,
            ));
        }
    };
    if let Some(current) = current {
        let mut map = parse_lenient(&current).ok_or_else(|| {
            CliError::Usage(format!(
                "{} no longer parses, so the proxy settings were not removed; fix it \
                 and run `kompressor unwrap opencode`",
                target.path.display()
            ))
        })?;
        for key in &keys {
            // A value the user changed mid-session is theirs now.
            if still_ours(key, get_path(&map, key)) {
                let original = owners
                    .keys
                    .get(key)
                    .and_then(|claim| claim.original.clone());
                set_path(&mut map, key, original);
            }
        }
        let original = file.original.as_deref();
        match original.map(|text| (text, parse_lenient(text))) {
            // Back to exactly what was there, comments of a jsonc file and the
            // user's own indentation included.
            Some((text, Some(parsed))) if parsed == map => {
                std::fs::write(&target.path, text).map_err(|e| {
                    CliError::runtime(&format!("could not write {}", target.path.display()), e)
                })?;
            }
            // The file did not exist and nothing but ours was ever in it.
            None if map.is_empty() => {
                let _ = std::fs::remove_file(&target.path);
            }
            _ => write_map(&target.path, &map)?,
        }
    }
    for key in &keys {
        owners.keys.remove(key);
    }
    write_owners(&owners_path, &owners)?;
    Ok(Released::Restored)
}

/// What [`reload`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum Reloaded {
    /// The running service re-read its configuration.
    Running,
    /// No service was running, so there was nothing to tell; the next one to
    /// start reads the file as it now is.
    NotRunning,
}

/// Ask the running service to re-read its configuration.
///
/// The service is long-lived and reads its config once, so an edited file
/// changes nothing until this runs. `opencode reload` answered in under half a
/// second when measured; the bound is for a service that is wedged. Blocking,
/// because `unwrap` has no runtime and the launch path can spare the thread.
///
/// `opencode reload` *starts* the service when none is running — measured: a
/// restore on a machine with no service left a detached one behind. So the
/// status is asked first, and a stopped service is left stopped.
pub fn reload() -> Result<Reloaded, String> {
    let (_, status) = opencode(&["service", "status"])?;
    if status.trim() == "stopped" {
        return Ok(Reloaded::NotRunning);
    }
    opencode(&["reload"]).map(|_| Reloaded::Running)
}

/// Run one bounded opencode command, returning its exit status and stdout.
fn opencode(args: &[&str]) -> Result<(std::process::ExitStatus, String), String> {
    let shown = format!("opencode {}", args.join(" "));
    let mut child = match std::process::Command::new("opencode")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("opencode is not on PATH".into());
        }
        Err(e) => return Err(format!("could not run `{shown}`: {e}")),
    };
    let deadline = std::time::Instant::now() + RELOAD_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "`{shown}` did not answer within {}s",
                    RELOAD_TIMEOUT.as_secs()
                ));
            }
            Err(e) => return Err(format!("could not wait for `{shown}`: {e}")),
        }
    };
    let mut stdout = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = std::io::Read::read_to_string(&mut out, &mut stdout);
    }
    if status.success() {
        return Ok((status, stdout));
    }
    let mut said = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = std::io::Read::read_to_string(&mut stderr, &mut said);
    }
    let said = said
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    Err(format!("`{shown}` failed: {}", said.trim()))
}

const RELOAD_TIMEOUT: Duration = Duration::from_secs(20);

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir()
                .join(format!("kompressor-opencode-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
        fn target(&self) -> Target {
            Target::in_dir(&self.0)
        }
        fn json(&self) -> PathBuf {
            self.0.join("opencode.json")
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn object(text: &str) -> Map<String, Value> {
        serde_json::from_str::<Value>(text)
            .unwrap()
            .as_object()
            .cloned()
            .unwrap()
    }

    fn at(map: &Map<String, Value>, key: &str) -> Option<String> {
        get_path(map, key)
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    #[test]
    fn jsonc_comments_and_trailing_commas_are_stripped_but_strings_are_not() {
        let text = r#"// leading
        {
          /* block */ "a": "http://x//not-a-comment", // tail
          "b": ["/* kept */", 1,],
          "c": {"d": "\"quoted\" // still a string",},
        }"#;
        let parsed: Value = serde_json::from_str(&strip_jsonc(text)).expect("parses once stripped");
        assert_eq!(parsed["a"], "http://x//not-a-comment");
        assert_eq!(parsed["b"][0], "/* kept */");
        assert_eq!(parsed["c"]["d"], "\"quoted\" // still a string");
    }

    #[test]
    fn our_url_is_recognised_on_any_port_and_only_on_its_own_path() {
        assert!(is_ours("openai", "http://127.0.0.1:8817/v1"));
        assert!(is_ours("opencode-go", "http://127.0.0.1:9/zen/go/v1"));
        assert!(!is_ours("opencode-go", "http://127.0.0.1:8817/v1"));
        // Someone else's local gateway is not ours just for being local.
        assert!(!is_ours("openai", "http://127.0.0.1:4000/openai/v1"));
        assert!(!is_ours("openai", "http://localhost:8817/v1"));
        assert!(!is_ours("openai", "https://gateway.example/v1"));
        assert!(!is_ours("custom", "http://127.0.0.1:8817/v1"));
    }

    #[test]
    fn the_launch_overlay_routes_every_provider_and_pins_openai_to_http() {
        let (text, routing) = launch_content(8817, None, &[]).unwrap();
        let content = object(&text);
        assert_eq!(
            at(&content, "providers.openai.settings.baseURL").as_deref(),
            Some("http://127.0.0.1:8817/v1")
        );
        assert_eq!(
            at(&content, "providers.anthropic.settings.baseURL").as_deref(),
            Some("http://127.0.0.1:8817/v1")
        );
        assert_eq!(
            at(&content, "providers.opencode.settings.baseURL").as_deref(),
            Some("http://127.0.0.1:8817/zen/v1")
        );
        assert_eq!(
            at(&content, "providers.opencode-go.settings.baseURL").as_deref(),
            Some("http://127.0.0.1:8817/zen/go/v1")
        );
        // Without it the main turn goes over a WebSocket, unmeasured and with
        // no retrieve loop.
        assert_eq!(
            at(&content, "providers.openai.settings.transport").as_deref(),
            Some("http")
        );
        assert_eq!(routing.routed.len(), PROVIDERS.len());
        assert!(routing.kept.is_empty());
    }

    #[test]
    fn a_provider_the_user_routed_is_left_where_they_put_it() {
        // A gateway in either spelling. Overriding it would send the gateway's
        // key to the vendor.
        let files = [
            object(r#"{"provider":{"openai":{"options":{"baseURL":"https://gw.example/v1"}}}}"#),
            object(r#"{"providers":{"anthropic":{"settings":{"baseURL":"https://a.example"}}}}"#),
        ];
        let (text, routing) = launch_content(8817, None, &files).unwrap();
        let content = object(&text);
        assert_eq!(at(&content, "providers.openai.settings.baseURL"), None);
        assert_eq!(at(&content, "providers.openai.settings.transport"), None);
        assert_eq!(at(&content, "providers.anthropic.settings.baseURL"), None);
        assert_eq!(routing.routed, vec!["opencode", "opencode-go"]);
        assert_eq!(
            routing.kept,
            vec![
                ("openai", "https://gw.example/v1".to_string()),
                ("anthropic", "https://a.example".to_string())
            ]
        );
    }

    #[test]
    fn a_url_left_by_another_wrap_is_not_mistaken_for_a_gateway() {
        let files = [object(
            r#"{"providers":{"openai":{"settings":{"baseURL":"http://127.0.0.1:9999/v1"}}}}"#,
        )];
        let (_, routing) = launch_content(8817, None, &files).unwrap();
        assert!(routing.kept.is_empty());
        assert!(routing.routed.contains(&"openai"));
    }

    #[test]
    fn existing_config_content_is_extended_not_replaced() {
        let existing = r#"{"model":"anthropic/x", "providers":{"opencode":{"settings":{"baseURL":"https://mine/zen/v1"}}}}"#;
        let (text, routing) = launch_content(8817, Some(existing), &[]).unwrap();
        let content = object(&text);
        assert_eq!(content["model"], "anthropic/x");
        assert_eq!(
            at(&content, "providers.opencode.settings.baseURL").as_deref(),
            Some("https://mine/zen/v1")
        );
        assert!(!routing.routed.contains(&"opencode"));
        assert!(launch_content(8817, Some("not json"), &[]).is_err());
    }

    #[test]
    fn a_launch_runs_standalone_unless_told_where_the_server_is() {
        assert_eq!(launch_args(&[]).unwrap(), vec!["--standalone"]);
        let run: Vec<String> = ["run", "--format", "json", "hi"].map(String::from).to_vec();
        assert_eq!(launch_args(&run).unwrap(), vec!["--standalone"]);
        let already = vec!["--standalone".to_string()];
        assert!(launch_args(&already).unwrap().is_empty());
        for server in [
            vec!["--server".to_string(), "http://h".to_string()],
            vec!["--server=http://h".to_string()],
        ] {
            assert!(
                launch_args(&server).is_err(),
                "{server:?} bypasses the proxy"
            );
        }
        // A prompt that merely mentions the flag is not the flag.
        let prompt = vec!["run".into(), "--".into(), "--server".into()];
        assert!(launch_args(&prompt).is_ok());
    }

    #[test]
    fn an_attach_writes_the_file_and_a_release_puts_it_back_byte_for_byte() {
        let scratch = Scratch::new("roundtrip");
        let original =
            "{\n    \"$schema\": \"https://opencode.ai/config.json\",\n    \"mcp\": {}\n}\n";
        std::fs::write(scratch.json(), original).unwrap();
        let target = scratch.target();

        let routing = claim(&target, 8817).unwrap();
        assert_eq!(routing.routed.len(), PROVIDERS.len());
        let during = object(&std::fs::read_to_string(scratch.json()).unwrap());
        assert_eq!(
            at(&during, "providers.opencode-go.settings.baseURL").as_deref(),
            Some("http://127.0.0.1:8817/zen/go/v1")
        );
        assert_eq!(during["$schema"], "https://opencode.ai/config.json");

        assert_eq!(release(&target, false).unwrap(), Released::Restored);
        assert_eq!(std::fs::read_to_string(scratch.json()).unwrap(), original);
        assert!(
            !target.owners_path().exists(),
            "no sidecar outlives the session"
        );
    }

    #[test]
    fn a_file_that_did_not_exist_does_not_exist_afterwards() {
        let scratch = Scratch::new("absent");
        let target = scratch.target();
        claim(&target, 8817).unwrap();
        assert!(scratch.json().exists());
        release(&target, false).unwrap();
        assert!(!scratch.json().exists());
    }

    #[test]
    fn the_users_gateway_and_transport_survive_an_attach() {
        let scratch = Scratch::new("gateway");
        std::fs::write(
            scratch.json(),
            r#"{"provider":{"anthropic":{"options":{"baseURL":"https://gw.example/v1"}}},
                "providers":{"openai":{"settings":{"transport":"websocket"}}}}"#,
        )
        .unwrap();
        let target = scratch.target();
        let routing = claim(&target, 8817).unwrap();
        assert_eq!(routing.kept.len(), 1);
        let during = object(&std::fs::read_to_string(scratch.json()).unwrap());
        assert_eq!(at(&during, "providers.anthropic.settings.baseURL"), None);
        assert_eq!(
            at(&during, "providers.openai.settings.transport").as_deref(),
            Some("websocket"),
            "a transport the user chose is not overridden"
        );
        assert_eq!(
            at(&during, "providers.openai.settings.baseURL").as_deref(),
            Some("http://127.0.0.1:8817/v1")
        );
        release(&target, false).unwrap();
        let after = object(&std::fs::read_to_string(scratch.json()).unwrap());
        assert_eq!(at(&after, "providers.openai.settings.baseURL"), None);
        assert_eq!(
            at(&after, "providers.openai.settings.transport").as_deref(),
            Some("websocket")
        );
        assert_eq!(
            at(&after, "provider.anthropic.options.baseURL").as_deref(),
            Some("https://gw.example/v1")
        );
    }

    #[test]
    fn an_edit_made_during_the_session_is_kept_when_ours_are_removed() {
        let scratch = Scratch::new("midsession");
        std::fs::write(scratch.json(), r#"{"theme":"dark"}"#).unwrap();
        let target = scratch.target();
        claim(&target, 8817).unwrap();
        let mut during = object(&std::fs::read_to_string(scratch.json()).unwrap());
        during.insert("theme".into(), Value::String("light".into()));
        // And one of ours, repointed by the user mid-session: theirs now.
        set_path(
            &mut during,
            "providers.anthropic.settings.baseURL",
            Some("https://picked.example/v1".into()),
        );
        write_map(&scratch.json(), &during).unwrap();

        release(&target, false).unwrap();
        let after = object(&std::fs::read_to_string(scratch.json()).unwrap());
        assert_eq!(after["theme"], "light");
        assert_eq!(
            at(&after, "providers.anthropic.settings.baseURL").as_deref(),
            Some("https://picked.example/v1")
        );
        assert_eq!(at(&after, "providers.openai.settings.baseURL"), None);
        assert_eq!(at(&after, "providers.opencode-go.settings.baseURL"), None);
    }

    #[test]
    fn a_commented_file_is_refused_rather_than_stripped() {
        let scratch = Scratch::new("jsonc");
        let path = scratch.0.join("opencode.jsonc");
        let text = "// my notes\n{\"theme\": \"dark\",}\n";
        std::fs::write(&path, text).unwrap();
        let target = scratch.target();
        assert_eq!(
            target.path, path,
            "the jsonc file wins, so it is the one to edit"
        );
        assert!(claim(&target, 8817).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn a_second_session_keeps_the_file_until_the_last_one_leaves() {
        let scratch = Scratch::new("second");
        let original = r#"{"theme":"dark"}"#;
        std::fs::write(scratch.json(), original).unwrap();
        let target = scratch.target();

        claim(&target, 8817).unwrap();
        // Session one is someone else, still alive.
        let owners_path = target.owners_path();
        let mut owners = read_owners(&owners_path);
        let file = owners.keys.get_mut(FILE_KEY).unwrap();
        // pid 1 exists and is not us; no start time, so liveness is by pid.
        file.holders[0] = serde_json::from_str(r#"{"pid":1,"port":8817}"#).unwrap();
        write_owners(&owners_path, &owners).unwrap();

        claim(&target, 8818).unwrap();
        assert_eq!(release(&target, false).unwrap(), Released::StillInUse);
        let during = object(&std::fs::read_to_string(scratch.json()).unwrap());
        assert!(at(&during, "providers.openai.settings.baseURL").is_some());

        assert_eq!(release(&target, true).unwrap(), Released::Restored);
        assert_eq!(
            std::fs::read_to_string(scratch.json()).unwrap(),
            original,
            "the original is the pre-wrap file, not the first session's"
        );
    }

    #[test]
    fn a_killed_sessions_leftovers_are_restored_to_the_true_original() {
        let scratch = Scratch::new("killed");
        let original = r#"{"theme":"dark"}"#;
        std::fs::write(scratch.json(), original).unwrap();
        let target = scratch.target();
        claim(&target, 8817).unwrap();
        // Killed: its holder is a process that no longer exists.
        let owners_path = target.owners_path();
        let mut owners = read_owners(&owners_path);
        owners.keys.get_mut(FILE_KEY).unwrap().holders[0].pid = u32::MAX - 1;
        write_owners(&owners_path, &owners).unwrap();

        // A new session walks in on the dead one's file and leaves cleanly.
        claim(&target, 8818).unwrap();
        release(&target, false).unwrap();
        assert_eq!(std::fs::read_to_string(scratch.json()).unwrap(), original);
    }

    #[test]
    fn releasing_what_was_never_claimed_is_not_an_error() {
        let scratch = Scratch::new("never");
        assert_eq!(release(&scratch.target(), true).unwrap(), Released::NotOurs);
    }
}
