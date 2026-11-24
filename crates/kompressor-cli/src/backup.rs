//! `kompressor backup`: every database into one archive, from the host.
//!
//! The same archive the Maintenance page downloads
//! ([`kompressor_core::backup`]), written by a process that reads the files
//! directly. It needs no listener, no credential and no setting, and it does
//! not disturb a running service: the snapshots are taken through SQLite, as
//! one more reader. That makes it the thing to put in a scheduled job.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use kompressor_core::Config;
use kompressor_core::backup::{self, Manifest};

use crate::error::{CliError, CliResult};

/// Where the archive goes: `out` itself when it names a file, a
/// timestamped name inside it when it is a directory, and a timestamped name
/// in the current directory when nothing was said.
fn destination(out: Option<&Path>, now_ms: i64) -> PathBuf {
    match out {
        Some(path) if path.is_dir() => path.join(backup::archive_name(now_ms)),
        Some(path) => path.to_path_buf(),
        None => PathBuf::from(backup::archive_name(now_ms)),
    }
}

fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// What was written, for a person.
fn summary(manifest: &Manifest, archive: &Path, archived: u64) -> String {
    let mut lines = vec![format!("{}  {}", archive.display(), size(archived))];
    for database in &manifest.databases {
        lines.push(format!(
            "  {:<9} {:>10}  schema v{}  from {}",
            database.name,
            size(database.bytes),
            database.schema_version,
            database.source
        ));
    }
    for skipped in &manifest.skipped {
        lines.push(format!(
            "  {:<9} not included: {}",
            skipped.name, skipped.reason
        ));
    }
    lines.join("\n")
}

pub fn run(config: Option<&Path>, out: Option<&Path>, json: bool, keep: Option<u32>) -> CliResult {
    let config = Config::load(config).map_err(|e| CliError::Usage(e.to_string()))?;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0);
    let archive = destination(out, now_ms);
    if archive.exists() {
        return Err(CliError::Usage(format!(
            "{} already exists; a backup never replaces a file",
            archive.display()
        )));
    }
    if let Some(parent) = archive.parent().filter(|p| !p.as_os_str().is_empty()) {
        backup::remove_stale_scratch(parent);
    }
    let manifest = backup::write_archive(&config, &archive)
        .map_err(|error| CliError::Runtime(format!("backup: {error}")))?;
    let archived = std::fs::metadata(&archive).map_or(0, |meta| meta.len());
    // Beside it, in the format `sha256sum -c` reads; the Maintenance page's
    // Verify compares against it. The archive is good without one.
    if let Err(error) = backup::write_checksum(&archive) {
        eprintln!("warning: the archive's checksum was not written: {error}");
    }
    // On record where the service will see it. Best effort: whoever runs
    // this may not be able to write the statistics database, and a backup
    // that worked is not failed for want of a note about it.
    let recorded = config.stats.enabled
        && kompressor_stats::writer::record_backup(
            &config.stats.path,
            &kompressor_stats::events::BackupEvent {
                time_ms: now_ms,
                origin: "cli".to_string(),
                destination: std::path::absolute(&archive)
                    .unwrap_or_else(|_| archive.clone())
                    .display()
                    .to_string(),
                bytes: i64::try_from(archived).unwrap_or(i64::MAX),
                databases: manifest.databases.len() as i64,
                ok: true,
                error: None,
            },
        )
        .unwrap_or(false);
    // Only once the new archive is written and checked, and only in a
    // directory: an archive named outright is nobody's rotation.
    let removed = match (keep, archive.parent()) {
        (Some(keep), Some(dir)) if out.is_some_and(Path::is_dir) => {
            backup::prune_archives(dir, keep)
        }
        _ => Vec::new(),
    };
    if json {
        let report = serde_json::json!({
            "archive": archive.display().to_string(),
            "bytes": archived,
            "manifest": manifest,
            "removed": removed,
            "recorded": recorded,
        });
        println!("{report}");
    } else {
        println!("{}", summary(&manifest, &archive, archived));
        for name in &removed {
            println!("  removed {name}");
        }
        if !recorded {
            println!(
                "  not recorded in the statistics database (not writable from here, \
                 or older than this build): the Health page will not count this one"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_archive_goes_where_it_was_pointed() {
        let dir = tempfile::tempdir().unwrap();
        let named = dir.path().join("mine.tar.gz");
        assert_eq!(destination(Some(&named), 0), named);
        assert_eq!(
            destination(Some(dir.path()), 0),
            dir.path().join("kompressor-backup-19700101-000000.tar.gz")
        );
        assert_eq!(
            destination(None, 0),
            PathBuf::from("kompressor-backup-19700101-000000.tar.gz")
        );
    }

    #[test]
    fn sizes_read_as_a_person_would_say_them() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1_023), "1023 B");
        assert_eq!(size(1_536), "1.5 KB");
        assert_eq!(size(376_541_184), "359.1 MB");
    }
}
