//! `kompressor restore`: put the databases in a backup back where the
//! configuration keeps them.
//!
//! The service must be stopped: a database a process has open is refused.
//! Without `--yes` this is a rehearsal — the archive is read and checked in
//! full, the databases are checked for being free, and what would happen is
//! printed; nothing is changed. With it, each existing file is moved aside
//! (never deleted) and the restored one takes its place
//! ([`kompressor_core::backup::restore`]).

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use kompressor_core::Config;
use kompressor_core::backup::{self, RestoreReport};

use crate::error::{CliError, CliResult};

fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut value, mut unit) = (bytes as f64, 0);
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

fn summary(report: &RestoreReport, archive: &Path) -> String {
    let mut lines = vec![format!(
        "{}  written by kompressor {}",
        archive.display(),
        report.manifest.kompressor
    )];
    for database in &report.databases {
        let target = database.target.as_deref().unwrap_or("");
        lines.push(match (&database.skipped, report.applied) {
            (Some(why), _) => format!("  {:<9} left alone: {why}", database.name),
            (None, true) => format!(
                "  {:<9} restored to {target}  ({}, schema v{}){}",
                database.name,
                size(database.bytes),
                database.schema_version,
                database
                    .moved_aside
                    .as_ref()
                    .map_or(String::new(), |aside| format!(
                        "\n            was moved to {aside}"
                    ))
            ),
            (None, false) => format!(
                "  {:<9} would replace {target}  ({}, schema v{})",
                database.name,
                size(database.bytes),
                database.schema_version
            ),
        });
    }
    for note in &report.notes {
        lines.push(format!("  note: {note}"));
    }
    lines.push(if report.applied {
        "Restored. Start the service.".to_string()
    } else {
        "Checked; nothing was changed. Run again with --yes to restore.".to_string()
    });
    lines.join("\n")
}

pub fn run(
    archive: &Path,
    config: Option<&Path>,
    only: Option<&str>,
    yes: bool,
    json: bool,
) -> CliResult {
    let config = Config::load(config).map_err(|e| CliError::Usage(e.to_string()))?;
    if !archive.is_file() {
        return Err(CliError::Usage(format!(
            "{} is not a file",
            archive.display()
        )));
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0);
    let report = backup::restore(&config, archive, only, yes, now_ms)
        .map_err(|error| CliError::Runtime(format!("restore: {error}")))?;
    if json {
        println!("{}", serde_json::to_string(&report).unwrap_or_default());
    } else {
        println!("{}", summary(&report, archive));
    }
    Ok(())
}
