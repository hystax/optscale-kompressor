//! Pointing an agent at the proxy by injecting a block into the config file it
//! reads.
//!
//! [`super::settings`] handles agents configured by a JSON settings file, and
//! environment variables handle the rest. Codex is neither: it ignores
//! `OPENAI_BASE_URL` outright — measured on 0.155.1, where setting it sent
//! every request straight to the provider and the wrapped session compressed
//! nothing — and the file it does read is TOML that the user owns and comments.
//!
//! So the edit is textual rather than parsed. A block delimited by two marker
//! comments is appended, and removing it is an exact reversal: everything
//! outside the markers is preserved byte for byte, which a TOML round-trip
//! through a value tree could not promise because it would discard the
//! comments and reorder the tables.
//!
//! # Why two blocks
//!
//! Bare TOML keys belong to the table that precedes them, so a key appended
//! after the user's `[model_providers.…]` section would silently land inside
//! it. The top-level keys therefore go at the head of the file and the table we
//! define goes at the foot, each in its own marker pair, so neither span can
//! swallow the user's content between them.
//!
//! # Concurrency
//!
//! The same problem [`super::settings`] documents, and the same answer: a
//! sidecar records who currently holds the file and what it looked like before
//! the first of them arrived, a claim counts only while its owner is alive, and
//! the last session out restores the original.

use std::path::{Path, PathBuf};

use super::settings::{
    Claim, Holder, Lock, Released, clear, let_go, read_owners, sibling, sweep, write_owners,
};
use crate::error::CliError;

/// The marker opening a block this command owns.
const BEGIN: &str = "# --- kompressor wrap: managed block, removed on exit ---";
/// The marker closing it.
const END: &str = "# --- end kompressor wrap ---";

/// What a top-level key the user set for themselves is prefixed with while
/// this command holds the file.
///
/// TOML rejects a duplicate key outright, so a key the file already assigns
/// cannot simply be overridden by appending ours — the file would stop
/// parsing and take the agent down with it. Commenting theirs out for the
/// duration is the override TOML does not offer, and it is exactly reversible:
/// the prefix is removed again on release, including from a file the user
/// edited mid-session, which the plain block-stripping path cannot do.
const DISABLED: &str = "# kompressor wrap disabled this line: ";

/// The key the ownership sidecar records this file's claim under.
///
/// One key for the whole file rather than one per TOML key: the blocks are
/// written and removed together, so a partial claim could not be restored to
/// anything coherent.
const CLAIM_KEY: &str = "\u{1}config-file";

/// A config file this command injects a block into.
pub struct Target {
    /// The file itself.
    pub path: PathBuf,
    /// Keys written at the head of the file, before any table.
    pub head: Vec<(String, String)>,
    /// Whole tables written at the foot, as rendered TOML.
    pub tables: Vec<Table>,
}

/// One `[table]` this command defines.
pub struct Table {
    /// The table's dotted name, e.g. `model_providers.kompressor`.
    pub name: String,
    /// Its keys, already rendered as TOML values.
    pub entries: Vec<(String, String)>,
}

/// How a codex install authenticates.
///
/// This decides how codex is pointed at the proxy: a ChatGPT login by a flag,
/// an API key by a block in its config file. See [`Target::codex_flags`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CodexAuth {
    /// A key in `OPENAI_API_KEY`, and no login.
    ApiKey,
    /// `codex login --with-api-key`: the key is in `auth.json`.
    StoredApiKey,
    /// A ChatGPT plan. There is no API key anywhere.
    Subscription,
}

/// Read the auth mode out of codex's `auth.json`.
///
/// Only the `auth_mode` field is read, and nothing else in that file is
/// touched, logged or copied — it holds the credential itself.
///
/// An unreadable or unfamiliar file is reported as [`CodexAuth::ApiKey`],
/// because that is the mode whose block is inert when it is wrong: a
/// subscription session that is handed the API-key block fails loudly at
/// startup, which is recoverable, while the reverse would route a key-holder's
/// traffic past the proxy in silence.
fn codex_auth(home: &Path) -> CodexAuth {
    let Ok(text) = std::fs::read_to_string(home.join("auth.json")) else {
        return CodexAuth::ApiKey;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return CodexAuth::ApiKey;
    };
    match value.get("auth_mode").and_then(serde_json::Value::as_str) {
        Some("chatgpt") => CodexAuth::Subscription,
        Some("apikey") => CodexAuth::StoredApiKey,
        _ => CodexAuth::ApiKey,
    }
}

impl Target {
    /// Codex's `config.toml`, honouring `CODEX_HOME`.
    pub fn codex(port: u16) -> Target {
        let home = codex_home();
        Target::codex_for(port, &home, codex_auth(&home))
    }

    /// The codex target in a given home: the block for an API-key login.
    ///
    /// A custom provider, because it is the only place
    /// `supports_websockets = false` can be set. That keeps the session on
    /// HTTP, where the proxy counts its tokens; a relayed WebSocket goes
    /// uncounted, which an API key, billed per token, cannot afford. A ChatGPT
    /// login is never given
    /// this block: `model_provider` is honoured under a plan, and the
    /// `env_key` it declares makes codex refuse the session with `Missing
    /// environment variable: OPENAI_API_KEY`. Measured on codex 0.155.1.
    ///
    /// `env_key` reads only the environment and `requires_openai_auth` only
    /// the stored login; given both, `env_key` wins. Measured on codex 0.164.0.
    pub fn codex_for(port: u16, home: &Path, auth: CodexAuth) -> Target {
        let base = proxy_base(port);
        let key = match auth {
            CodexAuth::StoredApiKey => ("requires_openai_auth".into(), "true".into()),
            CodexAuth::ApiKey | CodexAuth::Subscription => {
                ("env_key".into(), quoted("OPENAI_API_KEY"))
            }
        };
        Target {
            path: home.join("config.toml"),
            head: vec![
                ("model_provider".into(), quoted("kompressor")),
                ("openai_base_url".into(), quoted(&base)),
            ],
            tables: vec![Table {
                name: "model_providers.kompressor".into(),
                entries: vec![
                    ("name".into(), quoted("OpenAI via kompressor")),
                    ("base_url".into(), quoted(&base)),
                    ("wire_api".into(), quoted("responses")),
                    key,
                    ("supports_websockets".into(), "false".into()),
                ],
            }],
        }
    }

    /// Flags that point codex at the proxy instead of a block in this file,
    /// for a ChatGPT login; `None` under an API key, which keeps the file.
    ///
    /// Measured on codex 0.164.0 with a ChatGPT login: `openai_base_url` set
    /// in the config file makes codex send an API key (`sk-…`) of its own in
    /// place of the plan's token, and api.openai.com refuses that key for the
    /// Responses API — `401 … Missing scopes: api.responses.write`. The same
    /// key given as `-c` keeps the plan's token and `chatgpt-account-id`,
    /// which the proxy routes to chatgpt.com. A flag also leaves the file
    /// alone, so the Codex desktop app, which reads it too, is not redirected.
    pub fn codex_flags(port: u16) -> Option<Vec<String>> {
        codex_flags_for(port, codex_auth(&codex_home()))
    }

    fn owners_path(&self) -> PathBuf {
        sibling(&self.path, ".kompressor-wrap.json")
    }

    fn lock_path(&self) -> PathBuf {
        sibling(&self.path, ".kompressor-wrap.lock")
    }

    /// The text of the two blocks, head first, with `eol` ending each line.
    fn render(&self, eol: &str) -> (String, String) {
        let mut head = String::new();
        if !self.head.is_empty() {
            head.push_str(BEGIN);
            head.push_str(eol);
            for (key, value) in &self.head {
                head.push_str(&format!("{key} = {value}{eol}"));
            }
            head.push_str(END);
            head.push_str(eol);
        }

        let mut foot = String::new();
        if !self.tables.is_empty() {
            foot.push_str(BEGIN);
            foot.push_str(eol);
            for table in &self.tables {
                foot.push_str(&format!("[{}]{eol}", table.name));
                for (key, value) in &table.entries {
                    foot.push_str(&format!("{key} = {value}{eol}"));
                }
            }
            foot.push_str(END);
            foot.push_str(eol);
        }
        (head, foot)
    }

    /// Top-level keys we would write that the file already sets for itself.
    fn collisions(&self, content: &str) -> Vec<String> {
        let outside = strip_blocks(content);
        self.head
            .iter()
            .map(|(key, _)| key)
            .filter(|key| declares_key(&outside, key))
            .cloned()
            .collect()
    }

    /// Comment out the user's own assignment of any key we are about to write.
    ///
    /// Only the region before the first table header is touched, for the same
    /// reason [`declares_key`] only reads it: a key of the same name inside a
    /// table is a different key.
    fn disable_collisions(&self, body: &str) -> String {
        let keys: Vec<&str> = self.head.iter().map(|(key, _)| key.as_str()).collect();
        if keys.is_empty() {
            return body.to_string();
        }
        let mut out = String::with_capacity(body.len());
        let mut in_tables = false;
        // Each line with its own ending, so a CRLF file stays CRLF.
        for line in body.split_inclusive('\n') {
            let trimmed = line.trim_start();
            if trimmed.starts_with('[') {
                in_tables = true;
            }
            if !in_tables && keys.iter().any(|key| assigns(trimmed, key)) {
                out.push_str(DISABLED);
            }
            out.push_str(line);
        }
        out
    }
}

/// [`Target::codex_flags`] for a given auth mode.
fn codex_flags_for(port: u16, auth: CodexAuth) -> Option<Vec<String>> {
    (auth == CodexAuth::Subscription).then(|| {
        vec![
            "-c".into(),
            format!("openai_base_url={}", quoted(&proxy_base(port))),
        ]
    })
}

/// The proxy's OpenAI-style base URL, which codex appends `/responses` to.
fn proxy_base(port: u16) -> String {
    super::agents::Endpoint::V1.url(port)
}

/// Codex's configuration directory.
fn codex_home() -> PathBuf {
    codex_home_in(|name| std::env::var_os(name))
}

/// [`codex_home`] against a given environment, so a test can pass one without
/// mutating the process's own.
///
/// The home directory is found the way Codex finds it: `HOME` on Unix, the
/// user profile on Windows. PowerShell and cmd do not set `HOME` at all, and
/// reading only that sent `wrap codex` to `.\.codex\config.toml` in whatever
/// directory it was run from — a file Codex never reads, so the session
/// compressed nothing while reporting that the config pointed at the proxy.
/// Where Git Bash does set `HOME`, it names the profile anyway.
fn codex_home_in(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> PathBuf {
    let set = |name: &str| env(name).filter(|value| !value.is_empty());
    if let Some(explicit) = set("CODEX_HOME") {
        return PathBuf::from(explicit);
    }
    let home = if cfg!(windows) {
        set("USERPROFILE").or_else(|| set("HOME"))
    } else {
        set("HOME")
    };
    home.map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
}

/// A TOML string literal.
///
/// The values here are URLs and identifiers this command builds, so the escape
/// set is small — but a path from `CODEX_HOME` can reach a base URL, and an
/// unescaped quote there would produce a file Codex cannot read.
fn quoted(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Whether `content` assigns `key` at the top level.
///
/// Only the region before the first table header counts: `model_provider`
/// inside `[some.table]` is a different key that happens to share a name.
fn declares_key(content: &str, key: &str) -> bool {
    content
        .lines()
        .map(str::trim)
        .take_while(|line| !line.starts_with('['))
        .any(|line| assigns(line, key))
}

/// Whether one line assigns `key`.
///
/// A line already carrying the [`DISABLED`] prefix is a comment and assigns
/// nothing, which is what makes claiming the file twice idempotent.
fn assigns(line: &str, key: &str) -> bool {
    line.strip_prefix(key)
        .map(|rest| rest.trim_start().starts_with('='))
        .unwrap_or(false)
}

/// Undo [`Target::disable_collisions`], whatever else happened to the file.
fn reinstate(content: &str) -> String {
    if !content.contains(DISABLED) {
        return content.to_string();
    }
    let mut out = String::with_capacity(content.len());
    for line in content.split_inclusive('\n') {
        out.push_str(line.strip_prefix(DISABLED).unwrap_or(line));
    }
    out
}

/// The line ending `content` uses: CRLF if it has any, as a file saved on
/// Windows does, else LF. Our blocks are written with it, so the file does
/// not end up with two.
fn line_ending(content: &str) -> &'static str {
    if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// Remove every block this command owns, leaving the rest untouched.
///
/// Unmatched markers are left alone: a file holding only a `BEGIN` is one a
/// user edited by hand, and guessing where the block ended could delete their
/// content.
fn strip_blocks(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.find(BEGIN) {
        let Some(end_rel) = rest[start..].find(END) else {
            break;
        };
        let end = start + end_rel + END.len();
        out.push_str(&rest[..start]);
        let after = &rest[end..];
        rest = after
            .strip_prefix("\r\n")
            .or_else(|| after.strip_prefix('\n'))
            .unwrap_or(after);
    }
    out.push_str(rest);
    out
}

/// What [`claim`] decided to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Claimed {
    /// The block was written.
    Injected,
    /// An identical block was already there, from this session or another.
    AlreadyCurrent,
}

/// Inject the block, recording this process as an owner.
pub fn claim(target: &Target) -> Result<Claimed, CliError> {
    let _lock = Lock::acquire(target.lock_path());
    let owners_path = target.owners_path();
    let mut owners = read_owners(&owners_path);

    let current = std::fs::read_to_string(&target.path).unwrap_or_default();
    let existed = target.path.exists();

    // A key the file already sets is commented out for the duration rather
    // than refused. Refusing was the first answer and it was the wrong one: it
    // stopped the session dead over a line the user is perfectly happy for us
    // to override, and the only way forward was to hand-edit the file and
    // remember to put it back. Say what was done, and undo it on release.
    let collisions = target.collisions(&current);
    if !collisions.is_empty() {
        say!(
            "kompressor: {} already sets {} — commented out for this session, \
             restored on exit",
            target.path.display(),
            collisions.join(" and "),
        );
    }

    let mut claim = owners.keys.remove(CLAIM_KEY).unwrap_or_default();
    sweep(&mut claim, &owners_path);
    // Recorded once, by the first live claimant: a later session must restore
    // what was there before any of us, not the injected state it walked in on.
    if claim.holders.is_empty() {
        claim.original = existed.then(|| current.clone());
    }
    claim.holders.retain(|h| h.pid != std::process::id());
    claim.holders.push(Holder::me(&target.path));

    let next = injected_content(target, &current);

    let changed = next != current;
    if changed {
        if let Some(dir) = target.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| {
                CliError::runtime(&format!("could not create {}", dir.display()), e)
            })?;
        }
        std::fs::write(&target.path, &next).map_err(|e| {
            CliError::runtime(&format!("could not write {}", target.path.display()), e)
        })?;
    }
    owners.keys.insert(CLAIM_KEY.to_string(), claim);
    write_owners(&owners_path, &owners)?;
    Ok(if changed {
        Claimed::Injected
    } else {
        Claimed::AlreadyCurrent
    })
}

/// Give up this session's claim, restoring the file if it was the last holder.
///
/// `force` is `unwrap`: every claim is dropped and the file restored whoever
/// else is running, because the user is asking for their config back.
pub fn release(target: &Target, force: bool) -> Result<Released, CliError> {
    release_as(
        target,
        if force {
            Release::Unwrap
        } else {
            Release::Session
        },
    )
}

/// Remove a block that no live session holds, and leave one that is held.
///
/// For a ChatGPT login, which is pointed at the proxy with a flag and so never
/// claims the file: a block left there by a session that was killed still
/// points codex, and the Codex app, at a port nothing listens on, and a file
/// key is what makes a ChatGPT login send an API key in the first place.
pub fn clear_stale(target: &Target) -> Result<Released, CliError> {
    release_as(target, Release::Stale)
}

/// Whose claims a release drops.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Release {
    /// This session's, as it ends.
    Session,
    /// Everyone's, and a block no claim records: `unwrap`.
    Unwrap,
    /// Only the dead's, and a block no claim records.
    Stale,
}

fn release_as(target: &Target, how: Release) -> Result<Released, CliError> {
    let _lock = Lock::acquire(target.lock_path());
    let owners_path = target.owners_path();
    let mut owners = read_owners(&owners_path);
    if how != Release::Stale {
        // This process's claim ends here whichever way the release goes,
        // including when an `unwrap` has already dropped it.
        let_go(&target.path);
    }
    let Some(mut claim) = owners.keys.remove(CLAIM_KEY) else {
        // No claim recorded, but a block may still be there from a session
        // that was killed. Removing it is the repair `unwrap` exists for.
        return strip_orphan(target, how != Release::Session);
    };
    sweep(&mut claim, &owners_path);
    match how {
        Release::Session => claim.holders.retain(|h| h.pid != std::process::id()),
        Release::Unwrap => clear(&mut claim),
        Release::Stale => {}
    }
    if !claim.holders.is_empty() {
        owners.keys.insert(CLAIM_KEY.to_string(), claim);
        write_owners(&owners_path, &owners)?;
        return Ok(Released::StillInUse);
    }

    restore(target, &claim)?;
    write_owners(&owners_path, &owners)?;
    Ok(Released::Restored)
}

fn injected_content(target: &Target, current: &str) -> String {
    let eol = line_ending(current);
    let (head, foot) = target.render(eol);
    let body = target.disable_collisions(&strip_blocks(current));
    let mut next = String::new();
    next.push_str(&head);
    if !body.trim().is_empty() {
        next.push_str(body.trim_start_matches(['\r', '\n']));
        if !next.ends_with('\n') {
            next.push_str(eol);
        }
    }
    if !foot.is_empty() {
        if !next.is_empty() && !next.ends_with(&format!("{eol}{eol}")) {
            next.push_str(eol);
        }
        next.push_str(&foot);
    }

    next
}

/// Remove our settings while preserving edits made during the session.
fn restore(target: &Target, claim: &Claim) -> Result<(), CliError> {
    let current = match std::fs::read_to_string(&target.path) {
        Ok(current) => current,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(CliError::runtime(
                "could not read configuration for cleanup",
                error,
            ));
        }
    };
    let original = claim.original.as_deref().unwrap_or_default();
    let unchanged = current == injected_content(target, original);
    let cleaned = if unchanged {
        original.to_owned()
    } else {
        // The user edited the file while we held it, so their edits are kept
        // and only our changes undone — which has to include un-commenting
        // what `disable_collisions` turned off, or the key stays dead after we
        // are gone.
        reinstate(&strip_blocks(&current))
    };
    if claim.original.is_none() && unchanged {
        std::fs::remove_file(&target.path)
            .map_err(|e| CliError::runtime("could not remove managed configuration", e))
    } else {
        std::fs::write(&target.path, cleaned)
            .map_err(|e| CliError::runtime("could not clean managed configuration", e))
    }
}

/// Strip a block left behind by a session that never got to clean up.
fn strip_orphan(target: &Target, orphans: bool) -> Result<Released, CliError> {
    if !orphans {
        return Ok(Released::NotOurs);
    }
    let Ok(current) = std::fs::read_to_string(&target.path) else {
        return Ok(Released::NotOurs);
    };
    if !current.contains(BEGIN) && !current.contains(DISABLED) {
        return Ok(Released::NotOurs);
    }
    let stripped = reinstate(&strip_blocks(&current));
    std::fs::write(&target.path, &stripped)
        .map_err(|e| CliError::runtime(&format!("could not write {}", target.path.display()), e))?;
    Ok(Released::Restored)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "kompressor-configfile-{tag}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
        fn target(&self, port: u16) -> Target {
            Target::codex_for(port, &self.0, CodexAuth::ApiKey)
        }
        fn write_auth(&self, mode: &str) {
            std::fs::write(
                self.0.join("auth.json"),
                format!("{{\"auth_mode\":\"{mode}\",\"tokens\":{{}}}}"),
            )
            .unwrap();
        }
        fn write(&self, content: &str) {
            std::fs::write(self.0.join("config.toml"), content).unwrap();
        }
        fn read(&self) -> String {
            std::fs::read_to_string(self.0.join("config.toml")).unwrap_or_default()
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// An environment holding exactly `vars`.
    fn env_of(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<std::ffi::OsString> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.into())
    }

    #[test]
    fn codex_home_is_honoured_over_everything() {
        let env = env_of(&[
            ("CODEX_HOME", "/opt/codex"),
            ("HOME", "/home/u"),
            ("USERPROFILE", "/profile"),
        ]);
        assert_eq!(codex_home_in(env), PathBuf::from("/opt/codex"));
    }

    /// PowerShell and cmd leave `HOME` unset. Reading only `HOME` put the block
    /// in `.\.codex` under the working directory, which Codex never reads.
    #[cfg(windows)]
    #[test]
    fn on_windows_codex_home_is_in_the_profile_without_home() {
        let env = env_of(&[("USERPROFILE", r"C:\Users\u")]);
        assert_eq!(codex_home_in(env), PathBuf::from(r"C:\Users\u\.codex"));
        let env = env_of(&[("USERPROFILE", r"C:\Users\u"), ("HOME", "")]);
        assert_eq!(codex_home_in(env), PathBuf::from(r"C:\Users\u\.codex"));
    }

    #[cfg(unix)]
    #[test]
    fn on_unix_codex_home_is_under_home() {
        let env = env_of(&[("HOME", "/home/u"), ("USERPROFILE", "/elsewhere")]);
        assert_eq!(codex_home_in(env), PathBuf::from("/home/u/.codex"));
    }

    /// A ChatGPT login is pointed with `-c`, the config file only under a key.
    #[test]
    fn a_chatgpt_login_gets_a_flag_and_an_api_key_the_file() {
        assert_eq!(
            codex_flags_for(9131, CodexAuth::Subscription),
            Some(vec![
                "-c".to_string(),
                "openai_base_url=\"http://127.0.0.1:9131/v1\"".to_string(),
            ])
        );
        assert_eq!(codex_flags_for(9131, CodexAuth::ApiKey), None);
    }

    #[test]
    fn cleanup_preserves_edits_to_existing_settings_and_comments() {
        let scratch = Scratch::new("session-edits");
        scratch.write("# original comment\nmodel = \"old\"\n");
        let target = scratch.target(9230);
        claim(&target).unwrap();
        let edited = scratch.read().replace("\"old\"", "\"new\"") + "# added during session\n";
        scratch.write(&edited);
        release(&target, false).unwrap();
        let cleaned = scratch.read();
        assert!(cleaned.contains("model = \"new\""));
        assert!(cleaned.contains("# original comment"));
        assert!(cleaned.contains("# added during session"));
        assert!(!cleaned.contains(BEGIN));
    }

    #[test]
    fn forced_cleanup_preserves_user_settings_in_a_file_we_created() {
        let scratch = Scratch::new("new-session-edits");
        let target = scratch.target(9231);
        claim(&target).unwrap();
        scratch.write(&(scratch.read() + "[projects.example]\ntrust_level = \"trusted\"\n"));
        release(&target, true).unwrap();
        assert!(target.path.exists());
        assert!(scratch.read().contains("[projects.example]"));
        assert!(!scratch.read().contains(BEGIN));
    }

    #[test]
    fn the_injected_block_carries_what_codex_needs_to_reach_the_proxy() {
        let scratch = Scratch::new("inject");
        scratch.write("model = \"gpt-5.5\"\n");
        let target = scratch.target(9123);
        assert_eq!(claim(&target).unwrap(), Claimed::Injected);

        let text = scratch.read();
        assert!(text.contains("openai_base_url = \"http://127.0.0.1:9123/v1\""));
        assert!(text.contains("model_provider = \"kompressor\""));
        assert!(text.contains("[model_providers.kompressor]"));
        // The measured cure for a five-attempt WebSocket handshake.
        assert!(text.contains("supports_websockets = false"));
        // The user's own setting survives.
        assert!(text.contains("model = \"gpt-5.5\""));
    }

    #[test]
    fn the_top_level_keys_precede_every_table() {
        // A bare key after a table header belongs to that table, so getting
        // this wrong would define `model_provider` inside the user's section.
        let scratch = Scratch::new("order");
        scratch.write("[mcp_servers.thing]\ncommand = \"x\"\n");
        let target = scratch.target(9124);
        claim(&target).unwrap();

        let text = scratch.read();
        let key = text.find("model_provider = ").unwrap();
        let table = text.find("[mcp_servers.thing]").unwrap();
        assert!(
            key < table,
            "top-level key must come before any table:\n{text}"
        );
    }

    #[test]
    fn releasing_restores_the_file_byte_for_byte() {
        let scratch = Scratch::new("restore");
        let original = "# my notes\nmodel = \"gpt-5.5\"\n\n[mcp_servers.thing]\ncommand = \"x\"\n";
        scratch.write(original);
        let target = scratch.target(9125);
        claim(&target).unwrap();
        assert_ne!(scratch.read(), original);

        assert_eq!(release(&target, false).unwrap(), Released::Restored);
        assert_eq!(
            scratch.read(),
            original,
            "a comment or a blank line lost here is the user's file damaged"
        );
    }

    /// Whether every line in `text` ends in CRLF.
    fn all_crlf(text: &str) -> bool {
        text.matches('\n').count() == text.matches("\r\n").count()
    }

    /// A file saved on Windows. Its lines used to come back as LF while the
    /// session ran, and for good whenever codex wrote to it meanwhile.
    #[test]
    fn a_crlf_file_keeps_its_line_endings_through_a_session_codex_edits() {
        let scratch = Scratch::new("crlf");
        let original =
            "# my notes\r\nopenai_base_url = \"https://example\"\r\nmodel = \"gpt-5.5\"\r\n";
        scratch.write(original);
        let target = scratch.target(9128);
        claim(&target).unwrap();
        let injected = scratch.read();
        assert!(all_crlf(&injected), "{injected:?}");
        assert!(injected.contains(DISABLED));

        // Codex records a trusted project while the session runs.
        scratch.write(&(injected + "\r\n[projects.example]\r\ntrust_level = \"trusted\"\r\n"));
        release(&target, false).unwrap();
        let cleaned = scratch.read();
        assert!(all_crlf(&cleaned), "{cleaned:?}");
        assert!(cleaned.starts_with(original), "{cleaned:?}");
        assert!(cleaned.ends_with("[projects.example]\r\ntrust_level = \"trusted\"\r\n"));
        assert!(!cleaned.contains(BEGIN) && !cleaned.contains(DISABLED));
    }

    #[test]
    fn unwrap_keeps_a_crlf_file_crlf() {
        let scratch = Scratch::new("crlf-orphan");
        let original = "openai_base_url = \"https://example\"\r\nmodel = \"gpt-5.5\"\r\n";
        scratch.write(original);
        let target = scratch.target(9129);
        claim(&target).unwrap();
        // A session killed without cleaning up leaves no claim behind.
        let _ = std::fs::remove_file(target.owners_path());
        assert_eq!(release(&target, true).unwrap(), Released::Restored);
        let cleaned = scratch.read();
        assert!(all_crlf(&cleaned), "{cleaned:?}");
        assert_eq!(cleaned.trim_end(), original.trim_end());
    }

    /// A session killed under the file route leaves its block. A later ChatGPT
    /// session, pointed by flag, removes it; a block a live session holds stays.
    #[test]
    fn a_block_left_behind_is_cleared_and_a_held_one_kept() {
        let scratch = Scratch::new("stale");
        let original = "model = \"gpt-5.5\"\n";
        scratch.write(original);
        let target = scratch.target(9132);
        claim(&target).unwrap();
        // Held by this process, which is alive.
        assert_eq!(clear_stale(&target).unwrap(), Released::StillInUse);
        assert!(scratch.read().contains(BEGIN));

        // Killed: its claim never recorded, or recorded and then lost.
        let _ = std::fs::remove_file(target.owners_path());
        assert_eq!(clear_stale(&target).unwrap(), Released::Restored);
        assert_eq!(scratch.read().trim_end(), original.trim_end());
        assert_eq!(clear_stale(&target).unwrap(), Released::NotOurs);
        let_go(&target.path);
    }

    /// An `unwrap` from elsewhere drops this session's claim; the release that
    /// follows still takes away its lock file, which nothing else would.
    #[test]
    fn a_release_after_unwrap_leaves_no_lock_file() {
        let scratch = Scratch::new("unwrapped");
        scratch.write("model = \"gpt-5.5\"\n");
        let target = scratch.target(9133);
        claim(&target).unwrap();
        let held = sibling(
            &target.path,
            &format!(".kompressor-wrap.{}.held", std::process::id()),
        );
        assert!(held.exists());
        let _ = std::fs::remove_file(target.owners_path());
        assert_eq!(release(&target, false).unwrap(), Released::NotOurs);
        assert!(!held.exists());
    }

    #[test]
    fn a_file_we_created_is_removed_rather_than_left_empty() {
        let scratch = Scratch::new("created");
        let target = scratch.target(9126);
        claim(&target).unwrap();
        assert!(target.path.exists());

        release(&target, false).unwrap();
        assert!(
            !target.path.exists(),
            "a config directory we do not own should not keep our leftovers"
        );
    }

    #[test]
    fn claiming_twice_is_idempotent() {
        let scratch = Scratch::new("twice");
        scratch.write("model = \"gpt-5.5\"\n");
        let target = scratch.target(9127);
        claim(&target).unwrap();
        let once = scratch.read();
        assert_eq!(claim(&target).unwrap(), Claimed::AlreadyCurrent);
        assert_eq!(scratch.read(), once, "a second claim must not stack blocks");
    }

    #[test]
    fn a_key_the_user_already_sets_is_commented_out_rather_than_duplicated() {
        // TOML rejects duplicate keys, so appending ours would not win the
        // argument — it would stop Codex reading the file at all. Commenting
        // theirs out is the override TOML does not offer.
        let scratch = Scratch::new("collide");
        scratch.write("model_provider = \"mine\"\n");
        let target = scratch.target(9128);
        claim(&target).expect("a collision is not a reason to give up");

        let written = scratch.read();
        assert!(
            written.contains(&format!("{DISABLED}model_provider = \"mine\"")),
            "the user's line should have been commented, not removed: {written}"
        );
        assert_eq!(
            written
                .lines()
                .filter(|line| assigns(line.trim_start(), "model_provider"))
                .count(),
            1,
            "exactly one live assignment, or the file does not parse: {written}"
        );
    }

    #[test]
    fn the_line_it_commented_out_comes_back_on_release() {
        let scratch = Scratch::new("collide-restore");
        let original = "# my own settings\nmodel_provider = \"mine\"\nmodel = \"gpt-5\"\n";
        scratch.write(original);
        let target = scratch.target(9129);

        claim(&target).unwrap();
        assert!(scratch.read().contains(DISABLED));
        release(&target, false).unwrap();
        assert_eq!(
            scratch.read(),
            original,
            "release must put the file back exactly as it was"
        );
    }

    #[test]
    fn a_line_commented_out_by_a_session_that_died_is_repaired_by_unwrap() {
        // The session is killed before it can clean up, so the sidecar never
        // records a release and the user is left with their own setting
        // commented out by a process that no longer exists. `--unwrap` is the
        // repair, and it has to reach the commented line, not just the block.
        let scratch = Scratch::new("collide-orphan");
        scratch.write("model_provider = \"mine\"\n");
        let target = scratch.target(9130);
        claim(&target).unwrap();
        std::fs::remove_file(target.owners_path()).unwrap();

        release(&target, true).unwrap();
        let repaired = scratch.read();
        assert!(
            !repaired.contains(DISABLED),
            "the user's setting is still commented out: {repaired:?}"
        );
        assert!(
            repaired
                .lines()
                .any(|line| line.trim() == "model_provider = \"mine\""),
            "the user's setting did not come back: {repaired:?}"
        );
        // Whitespace is not asserted: `--unwrap` leaves the blank line that
        // separated the two blocks, which it did before this existed.
    }

    #[test]
    fn claiming_a_file_twice_does_not_comment_the_line_out_twice() {
        let scratch = Scratch::new("collide-twice");
        scratch.write("model_provider = \"mine\"\n");
        let target = scratch.target(9131);
        claim(&target).unwrap();
        let once = scratch.read();
        claim(&target).unwrap();
        assert_eq!(scratch.read(), once, "the second claim changed the file");
        assert_eq!(once.matches(DISABLED).count(), 1);
    }

    #[test]
    fn a_key_of_the_same_name_inside_a_table_is_left_alone() {
        // `openai_base_url` under `[model_providers.x]` is a different key that
        // happens to share a name, and commenting it out would break a provider
        // the user configured for something else entirely.
        let scratch = Scratch::new("collide-table");
        let original = "[model_providers.other]\nmodel_provider = \"theirs\"\n";
        scratch.write(original);
        let target = scratch.target(9132);
        claim(&target).unwrap();
        assert!(
            !scratch.read().contains(DISABLED),
            "a key inside a table is not a collision: {}",
            scratch.read()
        );
    }

    #[test]
    fn the_same_name_inside_a_table_is_not_a_collision() {
        let scratch = Scratch::new("nested");
        scratch.write("[profiles.work]\nmodel_provider = \"mine\"\n");
        let target = scratch.target(9129);
        assert_eq!(claim(&target).unwrap(), Claimed::Injected);
    }

    #[test]
    fn unwrap_strips_a_block_left_by_a_session_that_was_killed() {
        let scratch = Scratch::new("orphan");
        let target = scratch.target(9130);
        claim(&target).unwrap();
        // Lose the ownership record the way a `kill -9` would.
        let _ = std::fs::remove_file(target.owners_path());

        assert_eq!(release(&target, false).unwrap(), Released::NotOurs);
        assert_eq!(release(&target, true).unwrap(), Released::Restored);
        assert!(!scratch.read().contains("kompressor"));
    }

    #[test]
    fn a_marker_without_its_closing_pair_is_left_alone() {
        // Half a block means someone edited the file by hand. Guessing where
        // it ended could delete their content.
        let text = format!("{BEGIN}\nmodel_provider = \"x\"\nkeep = 1\n");
        assert_eq!(strip_blocks(&text), text);
    }

    #[test]
    fn an_api_key_install_gets_the_provider_block_and_its_websocket_setting() {
        let scratch = Scratch::new("keyauth");
        scratch.write("model = \"gpt-5.5\"\n");
        let target = scratch.target(9141);
        claim(&target).unwrap();

        let text = scratch.read();
        assert!(text.contains("model_provider = \"kompressor\""));
        assert!(text.contains("[model_providers.kompressor]"));
        // The only place this can be set; it keeps the session countable.
        assert!(text.contains("supports_websockets = false"));
    }

    #[test]
    fn the_auth_mode_comes_from_codexs_own_auth_file() {
        let scratch = Scratch::new("detect");
        scratch.write_auth("chatgpt");
        assert_eq!(codex_auth(&scratch.0), CodexAuth::Subscription);
        scratch.write_auth("apikey");
        assert_eq!(codex_auth(&scratch.0), CodexAuth::StoredApiKey);
    }

    /// Each login gets the setting that finds its key.
    #[test]
    fn the_provider_finds_the_key_where_the_login_keeps_it() {
        let stored = Target::codex_for(9142, Path::new("/x"), CodexAuth::StoredApiKey);
        let env = Target::codex_for(9142, Path::new("/x"), CodexAuth::ApiKey);
        let entry = |target: &Target, key: &str| {
            target.tables[0]
                .entries
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(
            entry(&stored, "requires_openai_auth").as_deref(),
            Some("true")
        );
        assert_eq!(entry(&stored, "env_key"), None);
        assert_eq!(
            entry(&env, "env_key").as_deref(),
            Some("\"OPENAI_API_KEY\"")
        );
        assert_eq!(entry(&env, "requires_openai_auth"), None);
    }

    #[test]
    fn an_unreadable_auth_file_is_treated_as_an_api_key() {
        // The mode whose block fails loudly when it is wrong. Guessing
        // subscription instead would route a key-holder past the proxy in
        // silence, which is this command's worst outcome.
        let scratch = Scratch::new("noauth");
        assert_eq!(codex_auth(&scratch.0), CodexAuth::ApiKey);
        std::fs::write(scratch.0.join("auth.json"), "not json at all").unwrap();
        assert_eq!(codex_auth(&scratch.0), CodexAuth::ApiKey);
    }
}
