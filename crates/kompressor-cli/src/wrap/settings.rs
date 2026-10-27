//! Pointing an agent at the proxy by editing the settings file it reads.
//!
//! Environment variables only reach a process this command launches. An agent
//! hosted inside an editor is started by the editor, inherits the editor's
//! environment, and never sees ours — so for those the base URL has to go into
//! the settings file the agent reads for itself.
//!
//! That turns a one-line change into shared mutable state on disk, and the
//! problems are the usual ones:
//!
//! * **Two sessions, one file.** Read-modify-write from two terminals loses one
//!   of the updates. A lock serialises them.
//! * **The second session sees the first one's URL.** Whoever exits last must
//!   restore what was there *before any of us*, not what it happened to observe
//!   on the way in — otherwise the last session out writes a dead proxy's URL
//!   back into the file. So the first claimant records the original and later
//!   ones inherit it.
//! * **A session that is killed restores nothing.** Its claim would pin the file
//!   forever, so a claim is only believed while its owner is alive.
//!
//! All three are handled by one sidecar file next to the settings, recording who
//! currently owns each key and what the value was before the first of them
//! arrived.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::paths::{self, ProcessId, marker_is_stale};
use crate::error::CliError;

/// One process's claim on one settings key.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Holder {
    pub(super) pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    start_time: Option<u64>,
    /// The file this holder keeps locked while it runs; see [`paths::hold`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lock: Option<PathBuf>,
}

impl Holder {
    /// This process, holding the lock next to `file` that proves it is alive.
    pub(super) fn me(file: &Path) -> Holder {
        let id = ProcessId::current();
        let lock = held_path(file);
        Holder {
            pid: id.pid,
            start_time: id.start_time,
            lock: paths::hold(&lock).ok().map(|()| lock),
        }
    }

    /// Whether this holder still runs, where that can be proven: by its lock,
    /// or by its pid and start time. `None` otherwise — no lock or one that
    /// could not be checked, and no start time, as outside Linux. A dead
    /// holder's lock file is removed on the way; see [`paths::reap_if_dead`].
    fn proven_alive(&self) -> Option<bool> {
        if let Some(alive) = self.lock.as_deref().and_then(paths::reap_if_dead) {
            return Some(alive);
        }
        self.start_time.map(|_| self.pid_alive())
    }

    /// Whether the pid runs, which off Linux it is always taken to.
    fn pid_alive(&self) -> bool {
        ProcessId {
            pid: self.pid,
            start_time: self.start_time,
        }
        .is_alive()
    }
}

/// The lock this process holds next to `file` while it has a claim on it.
fn held_path(file: &Path) -> PathBuf {
    sibling(
        file,
        &format!(".kompressor-wrap.{}.held", std::process::id()),
    )
}

/// Give up the lock [`Holder::me`] took next to `file`.
pub(super) fn let_go(file: &Path) {
    paths::let_go(&held_path(file));
}

/// Everyone currently claiming one key, and what the key held before any of
/// them touched it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct Claim {
    /// The pre-wrap value. `None` means the key did not exist, and restoring it
    /// means removing the key rather than writing an empty string.
    #[serde(default)]
    pub(super) original: Option<String>,
    #[serde(default)]
    pub(super) holders: Vec<Holder>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct Owners {
    #[serde(flatten)]
    pub(super) keys: BTreeMap<String, Claim>,
}

/// A settings file and the JSON object inside it that holds environment values.
pub struct Target {
    /// The settings file itself.
    pub settings: PathBuf,
    /// The object the key lives in, e.g. `env`.
    pub section: &'static str,
}

impl Target {
    /// Claude Code's project-local settings, relative to `root`.
    pub fn claude_code(root: &Path) -> Target {
        Target {
            settings: root.join(".claude").join("settings.local.json"),
            section: "env",
        }
    }

    fn owners_path(&self) -> PathBuf {
        sibling(&self.settings, ".kompressor-wrap.json")
    }

    fn lock_path(&self) -> PathBuf {
        sibling(&self.settings, ".kompressor-wrap.lock")
    }
}

pub(super) fn sibling(settings: &Path, name: &str) -> PathBuf {
    settings
        .parent()
        .map(|dir| dir.join(name))
        .unwrap_or_else(|| PathBuf::from(name))
}

/// An advisory lock held for the length of a read-modify-write.
///
/// `create_new` is the whole mechanism: it is atomic, so exactly one process
/// wins the race. A lock whose owner died is broken after
/// [`LOCK_STALE_AFTER`], because the alternative is a crashed session wedging
/// every future wrap in the project.
pub(super) struct Lock {
    path: PathBuf,
    held: bool,
}

const LOCK_STALE_AFTER: Duration = Duration::from_secs(30);

impl Lock {
    pub(super) fn acquire(path: PathBuf) -> Lock {
        for _ in 0..100 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Lock { path, held: true },
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if let Ok(meta) = std::fs::metadata(&path) {
                        let stale = meta
                            .modified()
                            .ok()
                            .and_then(|m| m.elapsed().ok())
                            .map(|age| age > LOCK_STALE_AFTER)
                            .unwrap_or(false);
                        if stale {
                            let _ = std::fs::remove_file(&path);
                            continue;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                // A directory we cannot write is degraded, not unusable: the
                // edit still happens, it is just not serialised against a peer.
                Err(_) => return Lock { path, held: false },
            }
        }
        Lock { path, held: false }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        if self.held {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn read_json(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn write_json(path: &Path, object: &Map<String, Value>) -> Result<(), CliError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| CliError::runtime(&format!("could not create {}", dir.display()), e))?;
    }
    let text = serde_json::to_string_pretty(&Value::Object(object.clone()))
        .map_err(|e| CliError::runtime("could not render the settings", e))?;
    std::fs::write(path, text + "\n")
        .map_err(|e| CliError::runtime(&format!("could not write {}", path.display()), e))
}

pub(super) fn read_owners(path: &Path) -> Owners {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub(super) fn write_owners(path: &Path, owners: &Owners) -> Result<(), CliError> {
    if owners.keys.is_empty() {
        let _ = std::fs::remove_file(path);
        return Ok(());
    }
    // The settings directory may not exist yet: a project that has never been
    // configured has no `.claude/`, and the ownership record can be written
    // before anything puts one there.
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| CliError::runtime(&format!("could not create {}", dir.display()), e))?;
    }
    let text = serde_json::to_string_pretty(owners)
        .map_err(|e| CliError::runtime("could not render the ownership record", e))?;
    std::fs::write(path, text + "\n")
        .map_err(|e| CliError::runtime(&format!("could not write {}", path.display()), e))
}

/// Drop every claim whose owner is gone.
pub(super) fn sweep(claim: &mut Claim, owners_path: &Path) {
    claim.holders.retain(|holder| match holder.proven_alive() {
        Some(alive) => alive,
        // Liveness cannot be proven; fall back to age, or a holder whose lock
        // cannot be read would pin the file for good.
        None => holder.pid_alive() && !marker_is_stale(owners_path),
    });
}

/// Drop every holder, for `unwrap`, and the locks the dead ones left behind.
pub(super) fn clear(claim: &mut Claim) {
    for holder in claim.holders.drain(..) {
        if let Some(lock) = holder.lock {
            paths::reap_if_dead(&lock);
        }
    }
}

/// Point `key` at `value`, recording this process as an owner.
///
/// Returns whether the file was changed, so a caller can stay quiet when there
/// was nothing to do.
pub fn claim(target: &Target, key: &str, value: &str) -> Result<bool, CliError> {
    let _lock = Lock::acquire(target.lock_path());
    let owners_path = target.owners_path();
    let mut owners = read_owners(&owners_path);

    let mut settings = read_json(&target.settings);
    let section = settings
        .entry(target.section.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !section.is_object() {
        return Err(CliError::Runtime(format!(
            "{} has a non-object `{}` and this command will not overwrite it",
            target.settings.display(),
            target.section
        )));
    }
    let current = section.get(key).and_then(Value::as_str).map(str::to_string);

    let mut claim = owners.keys.remove(key).unwrap_or_default();
    sweep(&mut claim, &owners_path);
    // The original is recorded once, by the first live claimant. A later session
    // observing a proxy URL must not adopt it as "the original" — restoring that
    // on exit would leave the file pointing at a proxy that is gone.
    if claim.holders.is_empty() {
        claim.original = current.clone();
    }
    claim.holders.retain(|h| h.pid != std::process::id());
    claim.holders.push(Holder::me(&target.settings));
    owners.keys.insert(key.to_string(), claim);

    let changed = current.as_deref() != Some(value);
    if changed {
        section
            .as_object_mut()
            .expect("checked above")
            .insert(key.to_string(), Value::String(value.to_string()));
        write_json(&target.settings, &settings)?;
    }
    write_owners(&owners_path, &owners)?;
    Ok(changed)
}

/// What a release decided to do, so the caller can report it accurately.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Released {
    /// The key was put back the way it was found.
    Restored,
    /// Another live session still owns it, so it was left in place.
    StillInUse,
    /// Nothing here was ours.
    NotOurs,
}

/// Give up this process's claim on `key`, restoring the original if it was the
/// last one holding it.
///
/// `force` is `unwrap`: the user is asking for their settings back, so every
/// claim is dropped and the restore happens regardless of who else is running.
pub fn release(target: &Target, key: &str, force: bool) -> Result<Released, CliError> {
    let _lock = Lock::acquire(target.lock_path());
    let owners_path = target.owners_path();
    let mut owners = read_owners(&owners_path);
    // One lock covers every key this process holds in the file, so it goes
    // with the last of them — including when an `unwrap` dropped this one.
    let mine = |c: &Claim| c.holders.iter().any(|h| h.pid == std::process::id());
    let Some(mut claim) = owners.keys.remove(key) else {
        if !owners.keys.values().any(mine) {
            let_go(&target.settings);
        }
        return Ok(Released::NotOurs);
    };
    sweep(&mut claim, &owners_path);
    if force {
        clear(&mut claim);
    } else {
        claim.holders.retain(|h| h.pid != std::process::id());
    }
    if !owners.keys.values().any(mine) {
        let_go(&target.settings);
    }

    if !claim.holders.is_empty() {
        owners.keys.insert(key.to_string(), claim);
        write_owners(&owners_path, &owners)?;
        return Ok(Released::StillInUse);
    }

    let mut settings = read_json(&target.settings);
    if let Some(Value::Object(section)) = settings.get_mut(target.section) {
        match &claim.original {
            Some(original) => {
                section.insert(key.to_string(), Value::String(original.clone()));
            }
            None => {
                section.remove(key);
            }
        }
        // An `env` block we emptied is one we created; leaving `"env": {}` behind
        // is litter in a file the user owns.
        let drop_section = section.is_empty();
        if drop_section {
            settings.remove(target.section);
        }
    }
    if settings.is_empty() {
        let _ = std::fs::remove_file(&target.settings);
    } else {
        write_json(&target.settings, &settings)?;
    }
    write_owners(&owners_path, &owners)?;
    Ok(Released::Restored)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir()
                .join(format!("kompressor-settings-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
        fn target(&self) -> Target {
            Target::claude_code(&self.0)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn value_at(target: &Target, key: &str) -> Option<String> {
        read_json(&target.settings)
            .get(target.section)?
            .get(key)?
            .as_str()
            .map(str::to_string)
    }

    #[test]
    fn a_claim_writes_the_url_and_a_release_takes_it_back_out() {
        let scratch = Scratch::new("roundtrip");
        let target = scratch.target();
        assert!(claim(&target, "ANTHROPIC_BASE_URL", "http://127.0.0.1:8817").unwrap());
        assert_eq!(
            value_at(&target, "ANTHROPIC_BASE_URL").as_deref(),
            Some("http://127.0.0.1:8817")
        );
        assert_eq!(
            release(&target, "ANTHROPIC_BASE_URL", false).unwrap(),
            Released::Restored
        );
        assert_eq!(
            value_at(&target, "ANTHROPIC_BASE_URL"),
            None,
            "a key that did not exist before must not exist after"
        );
    }

    #[test]
    fn a_pre_existing_value_is_restored_exactly() {
        let scratch = Scratch::new("preexisting");
        let target = scratch.target();
        std::fs::create_dir_all(target.settings.parent().unwrap()).unwrap();
        std::fs::write(
            &target.settings,
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://my-gateway.internal"},"other":1}"#,
        )
        .unwrap();

        claim(&target, "ANTHROPIC_BASE_URL", "http://127.0.0.1:8817").unwrap();
        release(&target, "ANTHROPIC_BASE_URL", false).unwrap();

        assert_eq!(
            value_at(&target, "ANTHROPIC_BASE_URL").as_deref(),
            Some("https://my-gateway.internal")
        );
        let settings = read_json(&target.settings);
        assert_eq!(
            settings.get("other").and_then(Value::as_i64),
            Some(1),
            "unrelated settings must survive untouched"
        );
    }

    /// The bug this whole module exists to prevent: a second session records the
    /// *first session's proxy URL* as the original, and restores a dead proxy.
    #[test]
    fn a_second_claimant_inherits_the_true_original() {
        let scratch = Scratch::new("inherit");
        let target = scratch.target();
        std::fs::create_dir_all(target.settings.parent().unwrap()).unwrap();
        std::fs::write(
            &target.settings,
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://real.example"}}"#,
        )
        .unwrap();

        // Session one, still running: a live holder that is not this process.
        claim(&target, "ANTHROPIC_BASE_URL", "http://127.0.0.1:8817").unwrap();
        let owners_path = target.owners_path();
        let mut owners = read_owners(&owners_path);
        let first = owners.keys.get_mut("ANTHROPIC_BASE_URL").unwrap();
        first.holders[0].pid = 1; // pid 1 exists and is not us
        first.holders[0].start_time = None;
        write_owners(&owners_path, &owners).unwrap();

        // Session two claims a different port and then exits.
        claim(&target, "ANTHROPIC_BASE_URL", "http://127.0.0.1:8818").unwrap();
        assert_eq!(
            release(&target, "ANTHROPIC_BASE_URL", false).unwrap(),
            Released::StillInUse,
            "session one is still running, so nothing may be restored yet"
        );
        assert_eq!(
            value_at(&target, "ANTHROPIC_BASE_URL").as_deref(),
            Some("http://127.0.0.1:8818"),
            "and session two's URL stays until its own proxy is gone"
        );

        // Now session one goes too.
        assert_eq!(
            release(&target, "ANTHROPIC_BASE_URL", true).unwrap(),
            Released::Restored
        );
        assert_eq!(
            value_at(&target, "ANTHROPIC_BASE_URL").as_deref(),
            Some("https://real.example"),
            "the restored value is the pre-wrap one, not the first session's proxy"
        );
    }

    #[test]
    fn a_dead_holders_claim_does_not_pin_the_file() {
        let scratch = Scratch::new("dead");
        let target = scratch.target();
        std::fs::create_dir_all(target.settings.parent().unwrap()).unwrap();
        let owners_path = target.owners_path();
        let mut owners = Owners::default();
        owners.keys.insert(
            "ANTHROPIC_BASE_URL".into(),
            Claim {
                original: Some("https://original.example".into()),
                holders: vec![Holder {
                    pid: 0,
                    start_time: Some(1),
                    lock: None,
                }],
            },
        );
        write_owners(&owners_path, &owners).unwrap();
        std::fs::write(
            &target.settings,
            r#"{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:9999"}}"#,
        )
        .unwrap();

        if cfg!(target_os = "linux") {
            assert_eq!(
                release(&target, "ANTHROPIC_BASE_URL", false).unwrap(),
                Released::Restored,
                "a claim held only by a dead process must not block the restore"
            );
            assert_eq!(
                value_at(&target, "ANTHROPIC_BASE_URL").as_deref(),
                Some("https://original.example")
            );
        }
    }

    /// A peer at `lock`, under a pid that is certainly running (1 is init on
    /// Unix): only the lock can tell whether the session itself is.
    fn add_peer(target: &Target, lock: PathBuf) {
        let owners_path = target.owners_path();
        let mut owners = read_owners(&owners_path);
        owners
            .keys
            .get_mut("ANTHROPIC_BASE_URL")
            .unwrap()
            .holders
            .push(Holder {
                pid: 1,
                start_time: None,
                lock: Some(lock),
            });
        write_owners(&owners_path, &owners).unwrap();
    }

    fn claimed(scratch: &Scratch) -> Target {
        let target = scratch.target();
        std::fs::create_dir_all(target.settings.parent().unwrap()).unwrap();
        std::fs::write(
            &target.settings,
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://original.example"}}"#,
        )
        .unwrap();
        claim(&target, "ANTHROPIC_BASE_URL", "http://127.0.0.1:9998").unwrap();
        target
    }

    /// A session killed with its window closed leaves its lock file, held by
    /// no one. On Windows and macOS that used to read as alive for twelve
    /// hours, and the file stayed pointed at the proxy.
    #[test]
    fn a_killed_peers_claim_does_not_pin_the_file() {
        let scratch = Scratch::new("killed");
        let target = claimed(&scratch);
        let lock = sibling(&target.settings, "peer.held");
        std::fs::write(&lock, "").unwrap();
        add_peer(&target, lock.clone());

        assert_eq!(
            release(&target, "ANTHROPIC_BASE_URL", false).unwrap(),
            Released::Restored
        );
        assert_eq!(
            value_at(&target, "ANTHROPIC_BASE_URL").as_deref(),
            Some("https://original.example")
        );
        assert!(!lock.exists(), "the corpse's lock is cleared away");
    }

    #[test]
    fn a_live_peers_claim_keeps_the_file() {
        let scratch = Scratch::new("live-peer");
        let target = claimed(&scratch);
        let lock = sibling(&target.settings, "peer.held");
        paths::hold(&lock).unwrap();
        add_peer(&target, lock.clone());

        assert_eq!(
            release(&target, "ANTHROPIC_BASE_URL", false).unwrap(),
            Released::StillInUse
        );
        paths::let_go(&lock);
    }

    /// A holder whose lock cannot be read, and with no start time to go by, is
    /// believed only until the claim goes stale, as one with no lock is. Read
    /// as alive for good, it would pin the file for good.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_lock_ages_out_rather_than_pinning_the_file() {
        let scratch = Scratch::new("unreadable");
        let target = claimed(&scratch);
        // A path through a file: opening it fails, and not as missing.
        add_peer(&target, target.settings.join("held"));
        let owners_path = target.owners_path();
        let claim = read_owners(&owners_path).keys["ANTHROPIC_BASE_URL"].clone();

        let mut fresh = claim.clone();
        sweep(&mut fresh, &owners_path);
        assert_eq!(fresh.holders.len(), 2, "believed while fresh");

        let day_ago = std::time::SystemTime::now() - Duration::from_secs(24 * 3600);
        std::fs::File::options()
            .write(true)
            .open(&owners_path)
            .unwrap()
            .set_modified(day_ago)
            .unwrap();
        let mut stale = claim;
        sweep(&mut stale, &owners_path);
        assert_eq!(stale.holders.len(), 1, "only this live process is left");
        let_go(&target.settings);
    }

    #[test]
    fn releasing_something_we_never_claimed_is_not_an_error() {
        let scratch = Scratch::new("notours");
        assert_eq!(
            release(&scratch.target(), "ANTHROPIC_BASE_URL", true).unwrap(),
            Released::NotOurs
        );
    }

    #[test]
    fn a_non_object_env_block_is_refused_rather_than_overwritten() {
        let scratch = Scratch::new("hostile");
        let target = scratch.target();
        std::fs::create_dir_all(target.settings.parent().unwrap()).unwrap();
        std::fs::write(&target.settings, r#"{"env":"not an object"}"#).unwrap();
        assert!(
            claim(&target, "ANTHROPIC_BASE_URL", "http://127.0.0.1:8817").is_err(),
            "a file we do not understand is left alone"
        );
    }

    #[test]
    fn the_lock_is_exclusive_and_self_releasing() {
        let scratch = Scratch::new("lock");
        let path = scratch.0.join("l.lock");
        {
            let _held = Lock::acquire(path.clone());
            assert!(path.exists(), "the lock file is the lock");
        }
        assert!(!path.exists(), "and it is gone once dropped");
    }
}
