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

    /// Every entry of an archive, by name.
    fn unpack(archive: &Path) -> Vec<(String, Vec<u8>)> {
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
