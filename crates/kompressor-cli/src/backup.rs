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

pub fn run(config: Option<&Path>, out: Option<&Path>, json: bool) -> CliResult {
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
    if json {
        let report = serde_json::json!({
            "archive": archive.display().to_string(),
            "bytes": archived,
            "manifest": manifest,
        });
        println!("{report}");
    } else {
        println!("{}", summary(&manifest, &archive, archived));
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
