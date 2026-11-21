//! One archive holding every database this deployment keeps.
//!
//! Two files carry state that cannot be recomputed: `stats.db`, the record of
//! every request and what compression did to it, and `refs.db`, the fragments
//! recoverable compression elided. Losing the first loses the history the
//! dashboards are drawn from; losing the second loses the content behind every
//! handle still in a conversation. A backup is both, taken together.
//!
//! # What is in the archive
//!
//! A gzipped tar of:
//!
//! - `manifest.json` — what was written and what was not, and why
//!   ([`Manifest`]). First, so it can be read without unpacking the rest.
//! - `stats.db` — a snapshot of the statistics database.
//! - `refs.db` — a snapshot of the ref store, when it is on disk.
//!
//! Each snapshot is one self-contained SQLite file: taken through SQLite
//! (`VACUUM INTO`) rather than copied, so it is consistent however busy the
//! service is, has its write-ahead log folded in, and has passed
//! `PRAGMA quick_check`. The service is neither stopped nor paused.
//!
//! # What is not
//!
//! The configuration, which holds API keys and the shared secret, and the
//! interception CA, which holds a private key. Neither is state this process
//! produced, and an archive that is safe to hand to whoever asked for the
//! statistics should not carry either. An in-memory ref store has no file and
//! is listed in the manifest as skipped.
//!
//! # Restoring
//!
//! Stop the service, put `stats.db` and `refs.db` where `stats.path` and
//! `refs.path` point, remove any `-wal` and `-shm` files beside them, and
//! start it. A database older than the binary is migrated on start, as any
//! other is; one newer than the binary is not something to restore.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::{Config, RefsBackend};
use crate::error::CoreError;

/// The archive layout this build writes. Raised when a reader of an older
/// archive would be misled by a newer one.
pub const FORMAT: u32 = 1;

/// Prefix of every scratch directory a backup creates beside its output, so
/// one a crashed process left behind can be recognised and removed.
const SCRATCH_PREFIX: &str = ".kompressor-backup-";

/// One database in the archive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackedUp {
    /// Its name in the archive: `stats.db` or `refs.db`.
    pub name: String,
    /// Where it was read from.
    pub source: String,
    /// Size of the snapshot, uncompressed.
    pub bytes: u64,
    /// The schema version (`PRAGMA user_version`) the snapshot holds.
    pub schema_version: u32,
}

/// A database that is not in the archive, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    pub name: String,
    pub reason: String,
}

/// What an archive holds. Written into it as `manifest.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    /// The version of the binary that wrote the archive.
    pub kompressor: String,
    pub created_at_ms: i64,
    pub databases: Vec<BackedUp>,
    pub skipped: Vec<Skipped>,
}

/// A name for an archive written now: `kompressor-backup-20261120-071530.tar.gz`,
/// in UTC.
pub fn archive_name(now_ms: i64) -> String {
    let seconds = now_ms.div_euclid(1_000);
    let (days, of_day) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // Civil date from days since the epoch (Howard Hinnant's algorithm), so
    // naming a file does not need a date library.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "kompressor-backup-{year:04}{month:02}{day:02}-{:02}{:02}{:02}.tar.gz",
        of_day / 3_600,
        of_day % 3_600 / 60,
        of_day % 60
    )
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0)
}

/// A scratch directory that removes itself, snapshots and all.
struct Scratch(PathBuf);

impl Scratch {
    /// A new, empty, private directory beside `output`: on the same
    /// filesystem as the archive, so nothing is copied across one, and never
    /// in a `/tmp` that may be memory.
    fn beside(output: &Path) -> std::io::Result<Scratch> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = match output.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        let path = parent.join(format!(
            "{SCRATCH_PREFIX}{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            // The snapshots hold every tenant's data; the directory they
            // wait in is this process's alone.
            builder.mode(0o700);
        }
        builder.create(&path)?;
        Ok(Scratch(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Remove scratch directories a backup that was killed left in `dir`.
///
/// Only ones old enough that no running backup can own them: an hour, against
/// a snapshot that takes seconds.
pub fn remove_stale_scratch(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(SCRATCH_PREFIX) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|at| at.elapsed().ok())
            .is_some_and(|age| age > std::time::Duration::from_secs(3_600));
        if old {
            let path = entry.path();
            let _ = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
        }
    }
}

/// One archive in a backup directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Archive {
    pub name: String,
    pub bytes: u64,
    /// When it was taken, from its name: the name is written once and a copy
    /// keeps it, where a file's modification time does not survive either.
    pub created_at_ms: i64,
    /// Its SHA-256, from the `.sha256` file written beside it; absent for
    /// one taken before those were.
    pub sha256: Option<String>,
}

/// When the archive called `name` was taken, if `name` is one
/// [`archive_name`] writes — exactly, so nothing else in a directory is ever
/// taken for a backup.
pub fn archive_time(name: &str) -> Option<i64> {
    let stamp = name
        .strip_prefix("kompressor-backup-")?
        .strip_suffix(".tar.gz")?;
    let bytes = stamp.as_bytes();
    if bytes.len() != 15 || bytes[8] != b'-' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = &stamp[range];
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse().ok())
            .flatten()
    };
    let (year, month, day) = (number(0..4)?, number(4..6)?, number(6..8)?);
    let (hour, minute, second) = (number(9..11)?, number(11..13)?, number(13..15)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    // Days from the civil date: the inverse of the conversion `archive_name`
    // makes.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let ms = (days * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000;
    // A date that does not exist (the 31st of a 30-day month) does not come
    // back as itself.
    (archive_name(ms) == name).then_some(ms)
}

/// The archives in `dir`, newest first. A directory that is missing or
/// unreadable holds none.
pub fn list_archives(dir: &Path) -> Vec<Archive> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut archives: Vec<Archive> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let created_at_ms = archive_time(&name)?;
            let meta = entry.metadata().ok()?;
            meta.is_file().then(|| Archive {
                sha256: read_checksum(&entry.path()),
                name,
                bytes: meta.len(),
                created_at_ms,
            })
        })
        .collect();
    archives.sort_by_key(|archive| std::cmp::Reverse(archive.created_at_ms));
    archives
}

/// Remove all but the `keep` newest archives in `dir`, and return the names
/// removed. Zero keeps them all. Nothing but an archive is ever touched.
pub fn prune_archives(dir: &Path, keep: u32) -> Vec<String> {
    if keep == 0 {
        return Vec::new();
    }
    list_archives(dir)
        .into_iter()
        .skip(keep as usize)
        .filter(|archive| std::fs::remove_file(dir.join(&archive.name)).is_ok())
        .inspect(|archive| {
            let _ = std::fs::remove_file(checksum_path(&dir.join(&archive.name)));
        })
        .map(|archive| archive.name)
        .collect()
}

/// When the next scheduled backup is due: `every` after the newest archive in
/// `dir`, or now when there is none. Read from the directory, so the schedule
/// is the same after a restart.
pub fn next_due_ms(dir: &Path, every: std::time::Duration, now_ms: i64) -> i64 {
    match list_archives(dir).first() {
        // An archive dated in the future — a clock that was wrong — must not
        // put the next one off for ever.
        Some(newest) if newest.created_at_ms <= now_ms => {
            newest.created_at_ms + i64::try_from(every.as_millis()).unwrap_or(i64::MAX / 2)
        }
        _ => now_ms,
    }
}

/// What a scheduled backup did.
#[derive(Clone, Debug)]
pub struct Scheduled {
    pub archive: PathBuf,
    pub bytes: u64,
    pub manifest: Manifest,
    /// Older archives removed to keep `maintenance.backup_keep`.
    pub removed: Vec<String>,
    /// The archive's SHA-256, also written beside it; absent when that file
    /// could not be written.
    pub sha256: Option<String>,
}

/// Take the scheduled backup if one is due at `now_ms`: write an archive
/// into `maintenance.backup_dir` and remove the ones past
/// `maintenance.backup_keep`. `Ok(None)` when there is no schedule or it is
/// not time.
///
/// Old archives are removed only after the new one is written and has been
/// checked, so a backup that fails never costs one that worked.
pub fn run_scheduled(config: &Config, now_ms: i64) -> Result<Option<Scheduled>, CoreError> {
    let Some((dir, every)) = config.maintenance.schedule() else {
        return Ok(None);
    };
    if next_due_ms(dir, every, now_ms) > now_ms {
        return Ok(None);
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)?;
    remove_stale_scratch(dir);
    let archive = dir.join(archive_name(now_ms));
    let manifest = write_archive(config, &archive)?;
    let bytes = std::fs::metadata(&archive)?.len();
    // The archive is good without it: a checksum that cannot be written is a
    // warning, not a failed backup.
    let sha256 = write_checksum(&archive)
        .map_err(|error| tracing::warn!(%error, "the archive's checksum was not written"))
        .ok();
    let removed = prune_archives(dir, config.maintenance.backup_keep);
    Ok(Some(Scheduled {
        archive,
        bytes,
        manifest,
        removed,
        sha256,
    }))
}

/// Where the checksum of `archive` is kept: beside it, as `<name>.sha256`.
pub fn checksum_path(archive: &Path) -> PathBuf {
    let mut path = archive.as_os_str().to_owned();
    path.push(".sha256");
    PathBuf::from(path)
}

/// The SHA-256 of the file at `path`, as lowercase hex. Read a buffer at a
/// time: an archive can be larger than memory.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = std::io::Read::read(&mut file, &mut buffer)?;
        if read == 0 {
            break;
        }
        context.update(&buffer[..read]);
    }
    Ok(context
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Write the SHA-256 of `archive` beside it, in the format `sha256sum`
/// writes — `<hex>  <name>` — so `sha256sum -c` checks a copy anywhere.
/// Returns the checksum.
pub fn write_checksum(archive: &Path) -> Result<String, CoreError> {
    let sum = sha256_file(archive)?;
    let name = archive
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(checksum_path(archive))?;
    writeln!(file, "{sum}  {name}")?;
    Ok(sum)
}

/// The checksum written beside `archive`, when there is a readable one.
pub fn read_checksum(archive: &Path) -> Option<String> {
    let text = std::fs::read_to_string(checksum_path(archive)).ok()?;
    let sum = text.split_whitespace().next()?.to_ascii_lowercase();
    (sum.len() == 64 && sum.bytes().all(|b| b.is_ascii_hexdigit())).then_some(sum)
}

/// Whether an archive matches the checksum written when it was taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumMatch {
    Matches,
    /// The bytes have changed since: a damaged or replaced file.
    Differs,
    /// None was written: an archive from before they were, or a copy.
    Absent,
}

/// One database in an archive, as [`verify_archive`] found it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedDatabase {
    pub name: String,
    pub bytes: u64,
    pub schema_version: u32,
    /// What SQLite's full integrity check found; empty when it is sound.
    pub problems: Vec<String>,
}

/// What [`verify_archive`] found: whether `kompressor restore` would accept
/// the archive, without stopping anything or touching a database.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verified {
    pub ok: bool,
    pub bytes: u64,
    pub sha256: String,
    pub checksum: ChecksumMatch,
    /// Absent when there was none to read.
    pub manifest: Option<Manifest>,
    pub databases: Vec<VerifiedDatabase>,
    /// Everything wrong with it, in words; empty when `ok`.
    pub problems: Vec<String>,
}

/// Check the archive at `archive` as a restore would, and say what is wrong
/// with it: the checksum written beside it, the manifest, and every database
/// in it unpacked and put through a full integrity check against what the
/// manifest says it holds.
///
/// Unpacked into a private directory beside the archive, removed afterwards,
/// so it needs the archive's uncompressed size free there for as long as it
/// runs. An archive that is damaged is a verdict, not an error: `Err` is for
/// one that cannot be read at all.
pub fn verify_archive(archive: &Path) -> Result<Verified, CoreError> {
    let bytes = std::fs::metadata(archive)?.len();
    let sha256 = sha256_file(archive)?;
    let checksum = match read_checksum(archive) {
        None => ChecksumMatch::Absent,
        Some(written) if written == sha256 => ChecksumMatch::Matches,
        Some(_) => ChecksumMatch::Differs,
    };
    let mut verified = Verified {
        ok: false,
        bytes,
        sha256,
        checksum,
        manifest: None,
        databases: Vec::new(),
        problems: Vec::new(),
    };
    if checksum == ChecksumMatch::Differs {
        verified
            .problems
            .push("its bytes differ from the checksum written when it was taken".to_string());
    }
    let scratch = Scratch::beside(archive)?;
    let mut unpacked: Vec<(String, PathBuf)> = Vec::new();
    let unpacking = (|| -> Result<(), String> {
        let file = File::open(archive).map_err(|error| error.to_string())?;
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
        for entry in tar
            .entries()
            .map_err(|error| format!("not a gzipped tar ({error})"))?
        {
            let mut entry = entry.map_err(|error| format!("damaged ({error})"))?;
            let name = entry
                .path()
                .map_err(|error| error.to_string())?
                .to_string_lossy()
                .into_owned();
            // Only the names a backup writes, as plain files: nothing in an
            // archive chooses where it lands.
            if !entry.header().entry_type().is_file() {
                continue;
            }
            if name == "manifest.json" {
                let mut text = String::new();
                std::io::Read::read_to_string(
                    &mut std::io::Read::take(&mut entry, 1 << 20),
                    &mut text,
                )
                .map_err(|error| format!("damaged ({error})"))?;
                verified.manifest = Some(
                    serde_json::from_str(&text)
                        .map_err(|error| format!("unreadable manifest ({error})"))?,
                );
                continue;
            }
            if !matches!(name.as_str(), "stats.db" | "refs.db") {
                continue;
            }
            if unpacked.iter().any(|(seen, _)| *seen == name) {
                return Err(format!("{name} is in it twice"));
            }
            let path = scratch.0.join(&name);
            let mut out = File::create(&path).map_err(|error| error.to_string())?;
            std::io::copy(&mut entry, &mut out).map_err(|error| format!("damaged ({error})"))?;
            unpacked.push((name, path));
        }
        Ok(())
    })();
    if let Err(problem) = unpacking {
        verified.problems.push(problem);
    }
    match &verified.manifest {
        None => verified
            .problems
            .push("no manifest.json: not a kompressor backup".to_string()),
        Some(manifest) if manifest.format > FORMAT => verified.problems.push(format!(
            "written in format {} by kompressor {}; this build reads up to {FORMAT}",
            manifest.format, manifest.kompressor
        )),
        Some(_) => {}
    }
    let listed = verified
        .manifest
        .as_ref()
        .map(|manifest| manifest.databases.clone())
        .unwrap_or_default();
    for expected in &listed {
        if !unpacked.iter().any(|(name, _)| *name == expected.name) {
            verified.problems.push(format!(
                "the manifest lists {} and it is not there",
                expected.name
            ));
        }
    }
    for (name, path) in &unpacked {
        let size = std::fs::metadata(path).map_or(0, |meta| meta.len());
        let mut problems = kompressor_store::sqlite::integrity_check(path)
            .unwrap_or_else(|error| vec![error.to_string()]);
        let schema_version =
            kompressor_store::sqlite::facts(path).map_or(0, |facts| facts.schema_version);
        match listed.iter().find(|database| database.name == *name) {
            None => problems.push("it is not in the manifest".to_string()),
            Some(expected) => {
                if expected.bytes != size {
                    problems.push(format!(
                        "{size} bytes where the manifest says {}",
                        expected.bytes
                    ));
                }
                if expected.schema_version != schema_version {
                    problems.push(format!(
                        "schema v{schema_version} where the manifest says v{}",
                        expected.schema_version
                    ));
                }
            }
        }
        for problem in problems.iter().take(3) {
            verified.problems.push(format!("{name}: {problem}"));
        }
        verified.databases.push(VerifiedDatabase {
            name: name.clone(),
            bytes: size,
            schema_version,
            problems,
        });
    }
    if verified.manifest.is_some() && verified.databases.is_empty() && verified.problems.is_empty()
    {
        verified.problems.push("it holds no database".to_string());
    }
    verified.ok = verified.problems.is_empty();
    drop(scratch);
    Ok(verified)
}

/// What became of one database in a restore.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Restored {
    pub name: String,
    /// Where it goes: `stats.path` or `refs.path`.
    pub target: Option<String>,
    /// Its size and schema version in the archive.
    pub bytes: u64,
    pub schema_version: u32,
    /// Whether it was put in place — or, without `apply`, would be.
    pub restored: bool,
    /// Where the file that was there has been moved; nothing is deleted.
    pub moved_aside: Option<String>,
    /// Why it was left alone, when it was.
    pub skipped: Option<String>,
}

/// What a restore did, or would do.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RestoreReport {
    pub manifest: Manifest,
    /// False for a rehearsal: everything was checked and nothing changed.
    pub applied: bool,
    pub databases: Vec<Restored>,
    /// Things worth knowing that did not stop it.
    pub notes: Vec<String>,
}

/// The databases an archive may hold, where `config` keeps each, and the
/// schema version this build brings one to.
fn restore_targets(config: &Config) -> [(&'static str, Result<PathBuf, &'static str>, u32); 2] {
    [
        (
            "stats.db",
            if config.stats.enabled {
                Ok(config.stats.path.clone())
            } else {
                Err("statistics are off (stats.enabled = false)")
            },
            kompressor_stats::schema::latest_version(),
        ),
        (
            "refs.db",
            match config.refs.backend {
                RefsBackend::Sqlite => Ok(config.refs.path.clone()),
                RefsBackend::Memory => {
                    Err("the ref store is in memory (refs.backend = \"memory\")")
                }
            },
            kompressor_store::sqlite::latest_version(),
        ),
    ]
}

/// `path` with `suffix` appended to its file name: `stats.db` + `-wal`.
fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Give the restored file the owner and mode of the one it replaces — or,
/// where there was none, the owner of the directory it is in. A restore is
/// run by whoever can stop the service, often root, and a database only root
/// can open is a service that does not start.
#[cfg(unix)]
fn adopt_ownership(restored: &Path, replaced: Option<&Path>, notes: &mut Vec<String>) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let like = replaced
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|meta| (meta.uid(), meta.gid(), Some(meta.mode() & 0o777)))
        .or_else(|| {
            let parent = restored.parent()?;
            let meta = std::fs::metadata(parent).ok()?;
            Some((meta.uid(), meta.gid(), None))
        });
    let Some((uid, gid, mode)) = like else { return };
    if let Some(mode) = mode {
        let _ = std::fs::set_permissions(restored, std::fs::Permissions::from_mode(mode));
    }
    let own = std::fs::metadata(restored).map(|meta| (meta.uid(), meta.gid()));
    if own.is_ok_and(|own| own != (uid, gid))
        && std::os::unix::fs::chown(restored, Some(uid), Some(gid)).is_err()
    {
        notes.push(format!(
            "{} is owned by whoever ran this, not by the owner of the file it replaced \
             (uid {uid}): make sure the service can open it",
            restored.display()
        ));
    }
}

#[cfg(not(unix))]
fn adopt_ownership(_: &Path, _: Option<&Path>, _: &mut Vec<String>) {}

/// Put the databases in `archive` back where `config` keeps them.
///
/// Everything is checked before anything is touched: the archive is one this
/// build can read, each database in it passes a full integrity check and is
/// no newer than this build's schema, and nothing has the databases it would
/// replace open. Only then, and only with `apply`, is each existing file
/// moved aside — to `<name>.before-restore-<UTC>`, with its write-ahead log;
/// nothing is deleted — and the restored one put in its place. Without
/// `apply` it is a rehearsal that reports what would happen.
///
/// `only` restores one database and leaves the other alone. An older
/// database is migrated when the service next starts, as any other is.
///
/// **The service must be stopped.** A database a process has open is refused
/// ([`kompressor_store::sqlite::in_use`]); replacing a file under a running
/// writer loses whatever it writes next and can corrupt both.
pub fn restore(
    config: &Config,
    archive: &Path,
    only: Option<&str>,
    apply: bool,
    now_ms: i64,
) -> Result<RestoreReport, CoreError> {
    let invalid = |why: String| CoreError::BadRequest(format!("{}: {why}", archive.display()));
    let targets = restore_targets(config);
    if let Some(name) = only
        && !targets.iter().any(|(known, ..)| *known == name)
    {
        return Err(CoreError::BadRequest(format!(
            "no database called {name:?}; an archive holds stats.db and refs.db"
        )));
    }

    // Unpack beside each target, so putting a file in place is a rename.
    let mut scratches: Vec<Scratch> = Vec::new();
    let mut unpacked: Vec<(usize, PathBuf)> = Vec::new();
    let mut manifest: Option<Manifest> = None;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(File::open(archive)?));
    for entry in tar
        .entries()
        .map_err(|error| invalid(format!("not a gzipped tar ({error})")))?
    {
        let mut entry = entry.map_err(|error| invalid(format!("damaged ({error})")))?;
        let name = entry
            .path()
            .map_err(|error| invalid(error.to_string()))?
            .to_string_lossy()
            .into_owned();
        // Only the names a backup writes, as plain files: nothing in an
        // archive chooses where it lands.
        if !entry.header().entry_type().is_file() {
            continue;
        }
        if name == "manifest.json" {
            let mut text = String::new();
            std::io::Read::read_to_string(
                &mut std::io::Read::take(&mut entry, 1 << 20),
                &mut text,
            )?;
            manifest = Some(
                serde_json::from_str(&text)
                    .map_err(|error| invalid(format!("unreadable manifest ({error})")))?,
            );
            continue;
        }
        let Some(index) = targets.iter().position(|(known, ..)| *known == name) else {
            continue;
        };
        if unpacked.iter().any(|(seen, _)| *seen == index) {
            return Err(invalid(format!("{name} is in it twice")));
        }
        let (_, Ok(target), _) = &targets[index] else {
            continue;
        };
        if only.is_some_and(|only| only != name) {
            continue;
        }
        if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let scratch = Scratch::beside(target)?;
        let path = scratch.0.join(&name);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        std::io::copy(&mut entry, &mut file)?;
        file.sync_all()?;
        scratches.push(scratch);
        unpacked.push((index, path));
    }
    let manifest =
        manifest.ok_or_else(|| invalid("no manifest.json: not a kompressor backup".into()))?;
    if manifest.format > FORMAT {
        return Err(invalid(format!(
            "written in format {} by kompressor {}; this build reads up to {FORMAT}",
            manifest.format, manifest.kompressor
        )));
    }

    // Check everything, change nothing.
    let mut report = RestoreReport {
        manifest,
        applied: apply,
        databases: Vec::new(),
        notes: Vec::new(),
    };
    let mut ready: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (index, (name, target, expected)) in targets.iter().enumerate() {
        let mut entry = Restored {
            name: name.to_string(),
            target: target.as_ref().ok().map(|path| path.display().to_string()),
            bytes: 0,
            schema_version: 0,
            restored: false,
            moved_aside: None,
            skipped: None,
        };
        let in_archive = report.manifest.databases.iter().any(|d| d.name == *name);
        let copy = unpacked
            .iter()
            .find(|(seen, _)| *seen == index)
            .map(|(_, p)| p);
        match (target, copy) {
            (_, None) if only.is_some_and(|only| only != *name) => {
                entry.skipped = Some("not asked for".to_string());
            }
            (Err(reason), _) => entry.skipped = Some(reason.to_string()),
            (Ok(_), None) if in_archive => {
                return Err(invalid(format!(
                    "the manifest lists {name} and it is not there"
                )));
            }
            (Ok(_), None) => entry.skipped = Some("not in the archive".to_string()),
            (Ok(target), Some(copy)) => {
                let problems = kompressor_store::sqlite::integrity_check(copy)
                    .unwrap_or_else(|error| vec![error.to_string()]);
                if let Some(first) = problems.first() {
                    return Err(invalid(format!("{name} in it is damaged: {first}")));
                }
                let facts = kompressor_store::sqlite::facts(copy)?;
                if facts.schema_version > *expected {
                    return Err(invalid(format!(
                        "{name} in it has schema v{}, newer than this build's v{expected} \
                         (written by kompressor {}): restore with that version or later",
                        facts.schema_version, report.manifest.kompressor
                    )));
                }
                if kompressor_store::sqlite::in_use(target) {
                    return Err(CoreError::BadRequest(format!(
                        "{} is open in another process: stop the service first",
                        target.display()
                    )));
                }
                entry.bytes = std::fs::metadata(copy)?.len();
                entry.schema_version = facts.schema_version;
                entry.restored = true;
                if facts.schema_version < *expected {
                    report.notes.push(format!(
                        "{name} holds schema v{}; it is migrated to v{expected} when the service starts",
                        facts.schema_version
                    ));
                }
                ready.push((copy.clone(), target.clone()));
            }
        }
        report.databases.push(entry);
    }
    if ready.is_empty() {
        let reasons: Vec<String> = report
            .databases
            .iter()
            .filter_map(|d| d.skipped.as_ref().map(|why| format!("{}: {why}", d.name)))
            .collect();
        return Err(invalid(format!(
            "nothing in it to restore here ({})",
            reasons.join("; ")
        )));
    }
    if !apply {
        return Ok(report);
    }

    // Now, and only now, the files move.
    let stamp = archive_name(now_ms);
    let stamp = stamp
        .trim_start_matches("kompressor-backup-")
        .trim_end_matches(".tar.gz");
    for (copy, target) in &ready {
        let entry = report
            .databases
            .iter_mut()
            .find(|d| d.target.as_deref() == Some(&target.display().to_string()))
            .expect("every ready database has its entry");
        let aside = beside(target, &format!(".before-restore-{stamp}"));
        let replaced = target.is_file();
        if replaced {
            std::fs::rename(target, &aside)?;
            entry.moved_aside = Some(aside.display().to_string());
        }
        // The old log and index go with the old file: left behind, SQLite
        // would replay them into the restored database.
        for suffix in ["-wal", "-shm", "-journal"] {
            let stale = beside(target, suffix);
            if stale.exists() {
                if replaced {
                    std::fs::rename(&stale, beside(&aside, suffix))?;
                } else {
                    std::fs::remove_file(&stale)?;
                }
            }
        }
        std::fs::rename(copy, target)?;
        adopt_ownership(
            target,
            replaced.then_some(aside.as_path()),
            &mut report.notes,
        );
    }
    drop(scratches);
    Ok(report)
}

/// Snapshot one database into `scratch`, or say why it is not there to take.
fn take<E>(
    name: &str,
    source: &Path,
    scratch: &Path,
    snapshot: impl FnOnce(&Path, &Path) -> Result<u32, E>,
) -> Result<Result<(BackedUp, PathBuf), Skipped>, CoreError>
where
    CoreError: From<E>,
{
    if !source.is_file() {
        return Ok(Err(Skipped {
            name: name.to_string(),
            reason: format!("{} does not exist", source.display()),
        }));
    }
    let copy = scratch.join(name);
    let schema_version = snapshot(source, &copy)?;
    let bytes = std::fs::metadata(&copy)?.len();
    Ok(Ok((
        BackedUp {
            name: name.to_string(),
            source: source.display().to_string(),
            bytes,
            schema_version,
        },
        copy,
    )))
}

/// Write an archive of every database `config` names to `output`.
///
/// `output` is created, and must not exist: a backup never replaces a file.
/// Nothing is left behind on failure — the partial archive and the snapshots
/// are removed. An archive with no database in it is an error, not an empty
/// success: `stats.enabled = false` with an in-memory ref store has nothing
/// to back up, and saying so is better than a file that looks like a backup.
pub fn write_archive(config: &Config, output: &Path) -> Result<Manifest, CoreError> {
    let scratch = Scratch::beside(output)?;
    let mut manifest = Manifest {
        format: FORMAT,
        kompressor: env!("CARGO_PKG_VERSION").to_string(),
        created_at_ms: now_ms(),
        databases: Vec::new(),
        skipped: Vec::new(),
    };
    let mut copies = Vec::new();
    let mut record = |taken: Result<(BackedUp, PathBuf), Skipped>| match taken {
        Ok((database, copy)) => {
            manifest.databases.push(database);
            copies.push(copy);
        }
        Err(skipped) => manifest.skipped.push(skipped),
    };

    if config.stats.enabled {
        record(take(
            "stats.db",
            &config.stats.path,
            &scratch.0,
            kompressor_stats::schema::snapshot,
        )?);
    } else {
        record(Err(Skipped {
            name: "stats.db".to_string(),
            reason: "statistics are off (stats.enabled = false)".to_string(),
        }));
    }
    match config.refs.backend {
        RefsBackend::Sqlite => record(take(
            "refs.db",
            &config.refs.path,
            &scratch.0,
            kompressor_store::sqlite::snapshot,
        )?),
        RefsBackend::Memory => record(Err(Skipped {
            name: "refs.db".to_string(),
            reason: "the ref store is in memory (refs.backend = \"memory\")".to_string(),
        })),
    }
    if manifest.databases.is_empty() {
        let reasons: Vec<&str> = manifest
            .skipped
            .iter()
            .map(|skipped| skipped.reason.as_str())
            .collect();
        return Err(CoreError::BadRequest(format!(
            "there is no database to back up: {}",
            reasons.join("; ")
        )));
    }

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Every tenant's fragments are in here: not world-readable, whatever
        // the umask.
        options.mode(0o600);
    }
    // Opened here, apart from the writing: a file that was already there
    // fails this and is left alone, while one this call created is removed
    // if writing it fails.
    let file = options.open(output)?;
    let written = pack(&manifest, &copies, file);
    if written.is_err() {
        let _ = std::fs::remove_file(output);
    }
    written?;
    Ok(manifest)
}

/// Tar and gzip the manifest and the snapshots into `file`.
fn pack(manifest: &Manifest, copies: &[PathBuf], file: File) -> Result<(), CoreError> {
    let gzip = flate2::write::GzEncoder::new(BufWriter::new(file), flate2::Compression::default());
    let mut archive = tar::Builder::new(gzip);
    let seconds = (manifest.created_at_ms / 1_000).max(0) as u64;

    let listing = serde_json::to_vec_pretty(manifest)
        .map_err(|error| CoreError::Io(std::io::Error::other(error)))?;
    let mut header = tar::Header::new_gnu();
    header.set_size(listing.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(seconds);
    header.set_cksum();
    archive.append_data(&mut header, "manifest.json", listing.as_slice())?;

    for (database, copy) in manifest.databases.iter().zip(copies) {
        let mut header = tar::Header::new_gnu();
        header.set_size(database.bytes);
        header.set_mode(0o600);
        header.set_mtime(seconds);
        header.set_cksum();
        archive.append_data(&mut header, &database.name, File::open(copy)?)?;
    }
    // Each layer is finished by hand: dropped, a failure to write the last
    // block would go unreported and leave an archive that ends early.
    let mut file = archive.into_inner()?.finish()?;
    file.flush()?;
    file.get_ref().sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// A configuration over two real, populated databases in `dir`.
    fn deployment(dir: &Path) -> Config {
        let mut config = Config::default();
        config.stats.path = dir.join("stats.db");
        config.refs.path = dir.join("refs.db");
        let stats = kompressor_stats::schema::open_read_write(&config.stats.path).unwrap();
        stats
            .execute(
                "INSERT INTO store_samples (time_ms, live_refs, live_bytes) VALUES (1, 2, 3)",
                [],
            )
            .unwrap();
        drop(stats);
        let store = kompressor_store::SqliteRefStore::open(&config.refs.path).unwrap();
        drop(store);
        config
    }

    /// An archive's entries: name and bytes, in order.
    type Entries = Vec<(String, Vec<u8>)>;

    /// Every entry of an archive, by name.
    fn unpack(archive: &Path) -> Entries {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(File::open(archive).unwrap()));
        tar.entries()
            .unwrap()
            .map(|entry| {
                let mut entry = entry.unwrap();
                let name = entry.path().unwrap().to_string_lossy().into_owned();
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).unwrap();
                (name, bytes)
            })
            .collect()
    }

    #[test]
    fn an_archive_holds_the_manifest_and_a_usable_copy_of_each_database() {
        let dir = tempfile::tempdir().unwrap();
        let config = deployment(dir.path());
        let output = dir.path().join("out").join("backup.tar.gz");
        std::fs::create_dir(output.parent().unwrap()).unwrap();
        let manifest = write_archive(&config, &output).unwrap();

        assert_eq!(manifest.format, FORMAT);
        assert_eq!(manifest.kompressor, env!("CARGO_PKG_VERSION"));
        assert!(manifest.skipped.is_empty());
        let names: Vec<&str> = manifest.databases.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["stats.db", "refs.db"]);
        assert_eq!(
            manifest.databases[0].schema_version,
            kompressor_stats::schema::latest_version()
        );

        let entries = unpack(&output);
        let listed: Vec<&str> = entries.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(listed, ["manifest.json", "stats.db", "refs.db"]);
        let inside: Manifest = serde_json::from_slice(&entries[0].1).unwrap();
        assert_eq!(
            inside, manifest,
            "the manifest in the archive is the one returned"
        );
        for (database, (_, bytes)) in manifest.databases.iter().zip(&entries[1..]) {
            assert_eq!(bytes.len() as u64, database.bytes);
            assert!(bytes.starts_with(b"SQLite format 3\0"), "{}", database.name);
        }

        // Restored, the statistics open and hold the row that was there.
        let restored = dir.path().join("restored.db");
        std::fs::write(&restored, &entries[1].1).unwrap();
        let conn = kompressor_stats::schema::open_read_write(&restored).unwrap();
        let rows: i64 = conn
            .query_row("SELECT live_bytes FROM store_samples", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 3);

        // Nothing but the archive is left beside it.
        let left: Vec<_> = std::fs::read_dir(output.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, ["backup.tar.gz"]);
    }

    /// Rows still in the write-ahead log are in the snapshot: it is taken
    /// through SQLite, not by copying the main file.
    #[test]
    fn rows_not_yet_checkpointed_are_in_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let config = deployment(dir.path());
        let writer = kompressor_stats::schema::open_read_write(&config.stats.path).unwrap();
        writer
            .execute_batch("PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        for sample in 10..200 {
            writer
                .execute(
                    "INSERT INTO store_samples (time_ms, live_refs, live_bytes) VALUES (?1, 0, 0)",
                    [sample],
                )
                .unwrap();
        }
        assert!(
            std::fs::metadata(dir.path().join("stats.db-wal"))
                .unwrap()
                .len()
                > 0,
            "the rows are in the log, with the writer still open"
        );
        let output = dir.path().join("backup.tar.gz");
        write_archive(&config, &output).unwrap();
        let restored = dir.path().join("restored.db");
        std::fs::write(&restored, &unpack(&output)[1].1).unwrap();
        let conn = rusqlite_free_count(&restored);
        assert_eq!(conn, 191);
        drop(writer);
    }

    fn rusqlite_free_count(path: &Path) -> i64 {
        kompressor_stats::schema::open_read_write(path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM store_samples", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn what_has_no_file_is_listed_as_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = deployment(dir.path());
        config.refs.backend = RefsBackend::Memory;
        let output = dir.path().join("backup.tar.gz");
        let manifest = write_archive(&config, &output).unwrap();
        assert_eq!(manifest.databases.len(), 1);
        assert_eq!(manifest.skipped.len(), 1);
        assert_eq!(manifest.skipped[0].name, "refs.db");
        assert!(manifest.skipped[0].reason.contains("in memory"));
        assert_eq!(unpack(&output).len(), 2);

        // A store configured on disk that has never been written.
        config.refs.backend = RefsBackend::Sqlite;
        config.refs.path = dir.path().join("never-created.db");
        let manifest = write_archive(&config, &dir.path().join("second.tar.gz")).unwrap();
        assert!(manifest.skipped[0].reason.ends_with("does not exist"));
        assert!(
            !config.refs.path.exists(),
            "backing up does not create what it was asked to copy"
        );
    }

    #[test]
    fn nothing_to_back_up_is_an_error_and_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.stats.enabled = false;
        config.refs.backend = RefsBackend::Memory;
        let output = dir.path().join("backup.tar.gz");
        let error = write_archive(&config, &output).unwrap_err().to_string();
        assert!(error.contains("no database to back up"), "{error}");
        assert!(error.contains("stats.enabled = false"), "{error}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn an_existing_file_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let config = deployment(dir.path());
        let output = dir.path().join("backup.tar.gz");
        std::fs::write(&output, b"last week's").unwrap();
        assert!(write_archive(&config, &output).is_err());
        assert_eq!(std::fs::read(&output).unwrap(), b"last week's");
        let scratch = std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with('.')
            })
            .count();
        assert_eq!(scratch, 0, "the snapshots are removed all the same");
    }

    #[cfg(unix)]
    #[test]
    fn the_archive_is_readable_by_its_owner_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let config = deployment(dir.path());
        let output = dir.path().join("backup.tar.gz");
        write_archive(&config, &output).unwrap();
        let mode = std::fs::metadata(&output).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn archives_are_named_for_the_utc_second_they_were_taken() {
        assert_eq!(archive_name(0), "kompressor-backup-19700101-000000.tar.gz");
        // 2026-11-20T07:15:30Z, and a leap day.
        assert_eq!(
            archive_name(1_795_158_930_000),
            "kompressor-backup-20261120-071530.tar.gz"
        );
        assert_eq!(
            archive_name(1_709_251_199_000),
            "kompressor-backup-20240229-235959.tar.gz"
        );
    }

    #[test]
    fn an_archive_name_reads_back_as_the_time_it_was_written_for() {
        for ms in [0, 1_795_158_930_000, 1_709_251_199_000, 951_782_400_000] {
            assert_eq!(archive_time(&archive_name(ms)), Some(ms));
        }
        for not_ours in [
            "kompressor-backup-20261120-071530.tar",
            "kompressor-backup-20261120-071530.tar.gz.part",
            "kompressor-backup-2026112-0071530.tar.gz",
            "kompressor-backup-20261131-071530.tar.gz",
            "kompressor-backup-20261120-246060.tar.gz",
            "kompressor-backup-2026112a-071530.tar.gz",
            "../kompressor-backup-20261120-071530.tar.gz",
            "stats.db",
        ] {
            assert_eq!(archive_time(not_ours), None, "{not_ours}");
        }
    }

    const HOUR: i64 = 3_600_000;

    /// A backup directory with an archive taken at each of `hours`, and two
    /// files that are not archives.
    fn backup_dir(dir: &Path, hours: &[i64]) -> PathBuf {
        let backups = dir.join("backups");
        std::fs::create_dir_all(&backups).unwrap();
        for hour in hours {
            std::fs::write(backups.join(archive_name(hour * HOUR)), b"archive").unwrap();
        }
        std::fs::write(backups.join("notes.txt"), b"mine").unwrap();
        std::fs::write(backups.join("kompressor-backup-old.tar.gz"), b"mine").unwrap();
        backups
    }

    #[test]
    fn pruning_keeps_the_newest_and_touches_nothing_that_is_not_an_archive() {
        let dir = tempfile::tempdir().unwrap();
        let backups = backup_dir(dir.path(), &[5, 1, 9, 3]);
        let listed: Vec<i64> = list_archives(&backups)
            .iter()
            .map(|archive| archive.created_at_ms / HOUR)
            .collect();
        assert_eq!(listed, [9, 5, 3, 1], "newest first, archives only");
        assert_eq!(
            prune_archives(&backups, 0),
            Vec::<String>::new(),
            "zero keeps all"
        );
        assert_eq!(list_archives(&backups).len(), 4);

        let removed = prune_archives(&backups, 2);
        assert_eq!(removed, [archive_name(3 * HOUR), archive_name(HOUR)]);
        let left: Vec<i64> = list_archives(&backups)
            .iter()
            .map(|archive| archive.created_at_ms / HOUR)
            .collect();
        assert_eq!(left, [9, 5]);
        assert!(backups.join("notes.txt").exists());
        assert!(backups.join("kompressor-backup-old.tar.gz").exists());
        assert!(list_archives(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn a_backup_is_due_an_interval_after_the_newest_archive() {
        let dir = tempfile::tempdir().unwrap();
        let every = std::time::Duration::from_secs(6 * 3_600);
        let empty = dir.path().join("none");
        assert_eq!(
            next_due_ms(&empty, every, 50 * HOUR),
            50 * HOUR,
            "none yet: now"
        );
        let backups = backup_dir(dir.path(), &[10, 4]);
        assert_eq!(next_due_ms(&backups, every, 11 * HOUR), 16 * HOUR);
        // Taken by a clock that ran ahead: not a reason to wait a year.
        std::fs::write(backups.join(archive_name(9_000 * HOUR)), b"archive").unwrap();
        assert_eq!(next_due_ms(&backups, every, 20 * HOUR), 20 * HOUR);
    }

    #[test]
    fn a_scheduled_backup_is_taken_when_due_and_old_ones_are_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = deployment(dir.path());
        assert!(
            run_scheduled(&config, 100 * HOUR).unwrap().is_none(),
            "no schedule"
        );
        let backups = dir.path().join("deep").join("backups");
        config.maintenance.backup_dir = backups.clone();
        config.maintenance.backup_every = std::time::Duration::from_secs(6 * 3_600);
        config.maintenance.backup_keep = 2;

        let first = run_scheduled(&config, 100 * HOUR)
            .unwrap()
            .expect("none yet: due");
        assert_eq!(first.archive, backups.join(archive_name(100 * HOUR)));
        assert_eq!(first.manifest.databases.len(), 2);
        assert_eq!(
            first.bytes,
            std::fs::metadata(&first.archive).unwrap().len()
        );
        assert!(first.removed.is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&backups).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "the directory it made is its owner's alone");
        }

        assert!(
            run_scheduled(&config, 103 * HOUR).unwrap().is_none(),
            "not yet"
        );
        assert!(run_scheduled(&config, 106 * HOUR).unwrap().is_some());
        let third = run_scheduled(&config, 112 * HOUR).unwrap().unwrap();
        assert_eq!(third.removed, [archive_name(100 * HOUR)]);
        let kept: Vec<i64> = list_archives(&backups)
            .iter()
            .map(|archive| archive.created_at_ms / HOUR)
            .collect();
        assert_eq!(kept, [112, 106]);
        assert_eq!(
            std::fs::read_dir(&backups).unwrap().count(),
            4,
            "two archives and their checksums; no scratch left, and the pruned one's checksum \
             went with it"
        );
        let newest = &list_archives(&backups)[0];
        assert_eq!(newest.sha256.as_deref(), third.sha256.as_deref());
        let line = std::fs::read_to_string(checksum_path(&third.archive)).unwrap();
        assert_eq!(
            line,
            format!("{}  {}\n", third.sha256.unwrap(), archive_name(112 * HOUR)),
            "the format sha256sum -c reads"
        );
    }

    /// A good archive passes, and each way one can go bad is named.
    #[test]
    fn an_archive_is_verified_without_restoring_it() {
        let dir = tempfile::tempdir().unwrap();
        let config = deployment(dir.path());
        let good = dir.path().join("good.tar.gz");
        write_archive(&config, &good).unwrap();

        let verified = verify_archive(&good).unwrap();
        assert!(verified.ok, "{verified:?}");
        assert_eq!(verified.checksum, ChecksumMatch::Absent, "none written yet");
        assert_eq!(verified.databases.len(), 2);
        assert!(verified.databases.iter().all(|d| d.problems.is_empty()));
        assert_eq!(verified.sha256, sha256_file(&good).unwrap());

        let sum = write_checksum(&good).unwrap();
        assert_eq!(read_checksum(&good).as_deref(), Some(sum.as_str()));
        assert_eq!(
            verify_archive(&good).unwrap().checksum,
            ChecksumMatch::Matches
        );
        let leftovers = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(SCRATCH_PREFIX))
            .count();
        assert_eq!(leftovers, 0, "nothing unpacked is left behind");

        // The same bytes and one more: the checksum notices.
        let grown = dir.path().join("grown.tar.gz");
        let mut bytes = std::fs::read(&good).unwrap();
        bytes.push(0);
        std::fs::write(&grown, &bytes).unwrap();
        std::fs::copy(checksum_path(&good), checksum_path(&grown)).unwrap();
        let differs = verify_archive(&grown).unwrap();
        assert_eq!(differs.checksum, ChecksumMatch::Differs);
        assert!(!differs.ok);

        // Damage in the middle of the compressed stream.
        let damaged = dir.path().join("damaged.tar.gz");
        let mut bytes = std::fs::read(&good).unwrap();
        let middle = bytes.len() / 2;
        for byte in &mut bytes[middle..middle + 64] {
            *byte ^= 0x5A;
        }
        std::fs::write(&damaged, &bytes).unwrap();
        let found = verify_archive(&damaged).unwrap();
        assert!(!found.ok);
        assert!(!found.problems.is_empty(), "{found:?}");

        // A tar of a database and nothing else is not one of ours.
        let bare = dir.path().join("bare.tar.gz");
        let gzip = flate2::write::GzEncoder::new(
            File::create(&bare).unwrap(),
            flate2::Compression::fast(),
        );
        let mut tar = tar::Builder::new(gzip);
        tar.append_path_with_name(&config.refs.path, "refs.db")
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        let foreign = verify_archive(&bare).unwrap();
        assert!(!foreign.ok);
        assert!(
            foreign
                .problems
                .iter()
                .any(|p| p.contains("not a kompressor backup")),
            "{foreign:?}"
        );

        assert!(verify_archive(&dir.path().join("absent.tar.gz")).is_err());
    }

    /// A scheduled backup that cannot be written removes nothing: the
    /// archives that worked are still there.
    #[test]
    fn a_failed_scheduled_backup_prunes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = deployment(dir.path());
        let backups = backup_dir(dir.path(), &[1, 2, 3]);
        config.maintenance.backup_dir = backups.clone();
        config.maintenance.backup_every = std::time::Duration::from_secs(3_600);
        config.maintenance.backup_keep = 1;
        // Nothing to back up: both databases gone.
        config.stats.enabled = false;
        config.refs.backend = RefsBackend::Memory;
        assert!(run_scheduled(&config, 50 * HOUR).is_err());
        assert_eq!(list_archives(&backups).len(), 3);
    }

    /// How many samples the statistics database at `path` holds.
    fn samples(path: &Path) -> i64 {
        kompressor_stats::schema::open_read_write(path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM store_samples", [], |row| row.get(0))
            .unwrap()
    }

    fn add_sample(config: &Config, time_ms: i64) {
        kompressor_stats::schema::open_read_write(&config.stats.path)
            .unwrap()
            .execute(
                "INSERT INTO store_samples (time_ms, live_refs, live_bytes) VALUES (?1, 0, 0)",
                [time_ms],
            )
            .unwrap();
    }

    const STAMP: i64 = 1_795_158_930_000;

    #[test]
    fn a_restore_puts_the_archive_back_and_moves_what_was_there_aside() {
        let dir = tempfile::tempdir().unwrap();
        let config = deployment(dir.path());
        let archive = dir.path().join("backup.tar.gz");
        write_archive(&config, &archive).unwrap();
        // Life goes on after the backup: these rows are not in it.
        add_sample(&config, 50);
        add_sample(&config, 51);
        assert_eq!(samples(&config.stats.path), 3);

        // A rehearsal changes nothing and says what it would do.
        let rehearsal = restore(&config, &archive, None, false, STAMP).unwrap();
        assert!(!rehearsal.applied);
        assert!(
            rehearsal
                .databases
                .iter()
                .all(|d| d.restored && d.moved_aside.is_none())
        );
        assert_eq!(samples(&config.stats.path), 3);
        let before: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert!(
            before
                .iter()
                .all(|name| !name.to_string_lossy().contains("before-restore"))
        );
        assert!(
            before
                .iter()
                .all(|name| !name.to_string_lossy().starts_with('.')),
            "no scratch left: {before:?}"
        );

        let done = restore(&config, &archive, None, true, STAMP).unwrap();
        assert!(done.applied);
        assert_eq!(samples(&config.stats.path), 1, "as it was when backed up");
        let stats = &done.databases[0];
        assert_eq!((stats.name.as_str(), stats.restored), ("stats.db", true));
        let aside = PathBuf::from(stats.moved_aside.as_ref().expect("moved, not deleted"));
        assert_eq!(
            aside.file_name().unwrap().to_string_lossy(),
            "stats.db.before-restore-20261120-071530"
        );
        assert_eq!(samples(&aside), 3, "what was there is whole, beside it");
        assert!(done.databases[1].moved_aside.is_some());
        // Both open, and the store is a store.
        kompressor_store::SqliteRefStore::open(&config.refs.path).unwrap();
        assert!(done.notes.is_empty(), "{:?}", done.notes);
    }

    #[cfg(unix)]
    #[test]
    fn a_restored_file_has_the_mode_of_the_one_it_replaced() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let config = deployment(dir.path());
        let archive = dir.path().join("backup.tar.gz");
        write_archive(&config, &archive).unwrap();
        std::fs::set_permissions(&config.stats.path, std::fs::Permissions::from_mode(0o644))
            .unwrap();
        restore(&config, &archive, None, true, STAMP).unwrap();
        let mode = std::fs::metadata(&config.stats.path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o644,
            "the service that could read the old one can read this"
        );
    }

    #[test]
    fn one_database_can_be_restored_and_the_other_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let config = deployment(dir.path());
        let archive = dir.path().join("backup.tar.gz");
        write_archive(&config, &archive).unwrap();
        add_sample(&config, 50);
        let refs_before = std::fs::read(&config.refs.path).unwrap();
        // The store is in use; the statistics are not, and only they are asked for.
        let store = kompressor_store::SqliteRefStore::open(&config.refs.path).unwrap();
        let done = restore(&config, &archive, Some("stats.db"), true, STAMP).unwrap();
        assert_eq!(samples(&config.stats.path), 1);
        assert_eq!(done.databases[1].skipped.as_deref(), Some("not asked for"));
        assert!(!done.databases[1].restored);
        drop(store);
        assert_eq!(
            std::fs::read(&config.refs.path).unwrap()[..100],
            refs_before[..100]
        );
        let unknown = restore(&config, &archive, Some("config.toml"), true, STAMP).unwrap_err();
        assert!(
            unknown.to_string().contains("stats.db and refs.db"),
            "{unknown}"
        );
    }

    /// A database a process has open is not replaced under it.
    #[test]
    fn a_database_in_use_is_refused_and_nothing_is_touched() {
        let dir = tempfile::tempdir().unwrap();
        let config = deployment(dir.path());
        let archive = dir.path().join("backup.tar.gz");
        write_archive(&config, &archive).unwrap();
        add_sample(&config, 50);
        let service = kompressor_store::SqliteRefStore::open(&config.refs.path).unwrap();
        for apply in [false, true] {
            let refused = restore(&config, &archive, None, apply, STAMP)
                .unwrap_err()
                .to_string();
            assert!(refused.contains("stop the service first"), "{refused}");
            assert!(refused.contains("refs.db"), "{refused}");
        }
        // Not half done: the statistics, which were free, were not replaced.
        assert_eq!(samples(&config.stats.path), 2);
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("before-restore") || name.starts_with('.'))
            .collect();
        assert!(left.is_empty(), "{left:?}");
        drop(service);
        restore(&config, &archive, None, true, STAMP).unwrap();
    }

    /// Onto a host with no databases yet — a new machine — and the old log
    /// of a database that is being replaced does not survive to be replayed.
    #[test]
    fn a_restore_onto_nothing_creates_the_databases() {
        let dir = tempfile::tempdir().unwrap();
        let config = deployment(dir.path());
        let archive = dir.path().join("backup.tar.gz");
        write_archive(&config, &archive).unwrap();
        let fresh = tempfile::tempdir().unwrap();
        let mut elsewhere = config.clone();
        elsewhere.stats.path = fresh.path().join("data").join("stats.db");
        elsewhere.refs.path = fresh.path().join("data").join("refs.db");
        // A stray log from some earlier life.
        std::fs::create_dir_all(fresh.path().join("data")).unwrap();
        std::fs::write(fresh.path().join("data").join("stats.db-wal"), b"stale").unwrap();
        let done = restore(&elsewhere, &archive, None, true, STAMP).unwrap();
        assert!(
            done.databases
                .iter()
                .all(|d| d.restored && d.moved_aside.is_none())
        );
        assert_eq!(samples(&elsewhere.stats.path), 1);

        // With the store in memory there, only the statistics go back.
        let mut partly = elsewhere.clone();
        partly.refs.backend = RefsBackend::Memory;
        let done = restore(&partly, &archive, None, true, STAMP + 1_000).unwrap();
        assert!(done.databases[0].restored);
        assert!(
            done.databases[1]
                .skipped
                .as_deref()
                .unwrap()
                .contains("in memory")
        );
        // And with nowhere to put either, it is an error, not a quiet success.
        partly.stats.enabled = false;
        let nothing = restore(&partly, &archive, None, true, STAMP)
            .unwrap_err()
            .to_string();
        assert!(
            nothing.contains("nothing in it to restore here"),
            "{nothing}"
        );
    }

    /// What is not a sound backup this build can read is refused before
    /// anything moves.
    #[test]
    fn an_archive_that_cannot_be_trusted_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let config = deployment(dir.path());
        let good = dir.path().join("good.tar.gz");
        write_archive(&config, &good).unwrap();
        let entries = unpack(&good);
        // Rebuild the archive with `change` applied to its entries.
        let rebuilt = |name: &str, change: &dyn Fn(&mut Entries)| {
            let mut entries = entries.clone();
            change(&mut entries);
            let path = dir.path().join(name);
            let gzip = flate2::write::GzEncoder::new(
                File::create(&path).unwrap(),
                flate2::Compression::fast(),
            );
            let mut archive = tar::Builder::new(gzip);
            for (name, bytes) in &entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o600);
                // The name written raw: the builder refuses a path with
                // `..` in it, and a hostile archive is not built by it.
                header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
                header.set_cksum();
                archive.append(&header, bytes.as_slice()).unwrap();
            }
            archive.into_inner().unwrap().finish().unwrap();
            path
        };
        let refused = |path: &Path| {
            restore(&config, path, None, true, STAMP)
                .unwrap_err()
                .to_string()
        };

        let junk = dir.path().join("junk.tar.gz");
        std::fs::write(&junk, b"not an archive at all").unwrap();
        assert!(refused(&junk).contains("not a gzipped tar") || refused(&junk).contains("damaged"));

        let bare = rebuilt("bare.tar.gz", &|entries| {
            entries.retain(|(name, _)| name != "manifest.json")
        });
        assert!(refused(&bare).contains("not a kompressor backup"));

        let future = rebuilt("future.tar.gz", &|entries| {
            let mut manifest: Manifest = serde_json::from_slice(&entries[0].1).unwrap();
            manifest.format = FORMAT + 1;
            entries[0].1 = serde_json::to_vec(&manifest).unwrap();
        });
        assert!(refused(&future).contains("this build reads up to"));

        let damaged = rebuilt("damaged.tar.gz", &|entries| {
            let bytes = &mut entries[1].1;
            let middle = bytes.len() / 2;
            for byte in &mut bytes[middle..middle + 4_096] {
                *byte = 0xAB;
            }
        });
        assert!(
            refused(&damaged).contains("stats.db in it is damaged"),
            "{}",
            refused(&damaged)
        );

        let missing = rebuilt("missing.tar.gz", &|entries| {
            entries.retain(|(name, _)| name != "refs.db")
        });
        assert!(refused(&missing).contains("lists refs.db and it is not there"));

        // A database from a build newer than this one.
        let newer = rebuilt("newer.tar.gz", &|entries| {
            let path = dir.path().join("bump.db");
            std::fs::write(&path, &entries[1].1).unwrap();
            let conn = kompressor_stats::schema::open_read_write(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA user_version = 9999;")
                .unwrap();
            drop(conn);
            entries[1].1 = std::fs::read(&path).unwrap();
        });
        let said = refused(&newer);
        assert!(said.contains("newer than this build"), "{said}");

        // An entry that would climb out of the directory is not one of ours,
        // and is not written anywhere.
        let sneaky = rebuilt("sneaky.tar.gz", &|entries| {
            entries.push(("evil/../../escaped.db".to_string(), b"x".to_vec()));
        });
        restore(&config, &sneaky, None, false, STAMP).unwrap();
        assert!(!dir.path().parent().unwrap().join("escaped.db").exists());

        // Through all of that, the live databases were never moved.
        assert_eq!(samples(&config.stats.path), 1);
        let moved = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("before-restore"))
            .count();
        assert_eq!(moved, 0);
    }

    /// An archive from an older schema restores, with a note that the
    /// service migrates it on start.
    #[test]
    fn an_older_database_restores_and_says_it_will_be_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let config = deployment(dir.path());
        kompressor_stats::schema::open_read_write(&config.stats.path)
            .unwrap()
            .execute_batch(
                "DROP TABLE backups; DROP TABLE maintenance_events; PRAGMA user_version = 15;",
            )
            .unwrap();
        let archive = dir.path().join("old.tar.gz");
        let manifest = write_archive(&config, &archive).unwrap();
        assert_eq!(manifest.databases[0].schema_version, 15);
        let done = restore(&config, &archive, None, true, STAMP).unwrap();
        assert_eq!(done.databases[0].schema_version, 15);
        assert!(
            done.notes.iter().any(|note| note.contains("migrated to v")),
            "{:?}",
            done.notes
        );
        // Opening it as the service does brings it up.
        let conn = kompressor_stats::schema::open_read_write(&config.stats.path).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version as u32, kompressor_stats::schema::latest_version());
    }

    #[test]
    fn only_old_scratch_directories_are_swept() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join(format!("{SCRATCH_PREFIX}1-0"));
        std::fs::create_dir(&fresh).unwrap();
        std::fs::write(dir.path().join("stats.db"), b"keep").unwrap();
        remove_stale_scratch(dir.path());
        assert!(fresh.exists(), "one a backup may still be using");
        assert!(dir.path().join("stats.db").exists());
        remove_stale_scratch(&dir.path().join("missing"));
    }
}
