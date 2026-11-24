//! `kompressor db`: what the Maintenance page shows and does about the
//! databases, from the host.
//!
//! `status` describes each database as SQLite sees it, `check` runs a full
//! integrity check, `checkpoint` folds the write-ahead log into the file.
//! Like `kompressor backup`, these read the files directly, as whoever runs
//! them: no listener, no credential, no setting, and the service keeps
//! running. `check` exits 1 when a database is damaged, so a scheduled job
//! can alarm on it.

use std::path::{Path, PathBuf};

use kompressor_core::Config;
use kompressor_core::config::RefsBackend;
use kompressor_store::sqlite;
use serde_json::{Value, json};

use crate::error::{CliError, CliResult};

/// A database by name, the schema version this build expects of it, and
/// where it is — or why it has no file.
type Database = (&'static str, u32, Result<PathBuf, String>);

/// The on-disk databases `config` names, or why one has no file.
fn databases(config: &Config) -> Vec<Database> {
    let on_disk = |path: &Path| {
        if path.is_file() {
            Ok(path.to_path_buf())
        } else {
            Err(format!("{} does not exist", path.display()))
        }
    };
    vec![
        (
            "stats.db",
            kompressor_stats::schema::latest_version(),
            if config.stats.enabled {
                on_disk(&config.stats.path)
            } else {
                Err("statistics are off".to_string())
            },
        ),
        (
            "refs.db",
            sqlite::latest_version(),
            match config.refs.backend {
                RefsBackend::Sqlite => on_disk(&config.refs.path),
                RefsBackend::Memory => Err("the ref store is in memory".to_string()),
            },
        ),
    ]
}

/// Those of them `only` names, or all; an unknown name is a usage error.
fn chosen(config: &Config, only: Option<&str>) -> Result<Vec<Database>, CliError> {
    let all = databases(config);
    match only {
        None => Ok(all),
        Some(name) => {
            let found: Vec<_> = all
                .into_iter()
                .filter(|(known, ..)| *known == name)
                .collect();
            if found.is_empty() {
                return Err(CliError::Usage(format!(
                    "no database called {name:?}; there are stats.db and refs.db"
                )));
            }
            Ok(found)
        }
    }
}

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

fn wal_bytes(path: &Path) -> u64 {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    std::fs::metadata(PathBuf::from(wal)).map_or(0, |meta| meta.len())
}

fn load(config: Option<&Path>) -> Result<Config, CliError> {
    Config::load(config).map_err(|e| CliError::Usage(e.to_string()))
}

fn print(json: bool, report: &[Value], line: impl Fn(&Value) -> String) {
    if json {
        println!("{}", Value::Array(report.to_vec()));
    } else {
        for entry in report {
            println!("{}", line(entry));
        }
    }
}

pub fn status(config: Option<&Path>, only: Option<&str>, as_json: bool) -> CliResult {
    let config = load(config)?;
    let report: Vec<Value> = chosen(&config, only)?
        .into_iter()
        .map(|(name, expected, path)| match path {
            Err(reason) => json!({"name": name, "present": false, "reason": reason}),
            Ok(path) => {
                let bytes = std::fs::metadata(&path).map_or(0, |meta| meta.len());
                let mut entry = json!({
                    "name": name,
                    "present": true,
                    "path": path.display().to_string(),
                    "bytes": bytes,
                    "wal_bytes": wal_bytes(&path),
                    "expected_schema_version": expected,
                });
                match sqlite::facts(&path) {
                    Ok(facts) => {
                        entry["schema_version"] = json!(facts.schema_version);
                        entry["reclaimable_bytes"] = json!(facts.reclaimable_bytes());
                    }
                    Err(error) => entry["error"] = json!(error.to_string()),
                }
                if name == "stats.db"
                    && let Ok(Some((oldest, newest))) = kompressor_stats::schema::time_span(&path)
                {
                    entry["oldest_ms"] = json!(oldest);
                    entry["newest_ms"] = json!(newest);
                }
                entry
            }
        })
        .collect();
    print(as_json, &report, |entry| {
        let name = entry["name"].as_str().unwrap_or_default();
        if entry["present"] == false {
            return format!("{name:<9} none: {}", entry["reason"].as_str().unwrap_or(""));
        }
        if let Some(error) = entry["error"].as_str() {
            return format!("{name:<9} UNREADABLE: {error}");
        }
        let (held, expected) = (
            entry["schema_version"].as_u64().unwrap_or(0),
            entry["expected_schema_version"].as_u64().unwrap_or(0),
        );
        let schema = if held == expected {
            format!("schema v{held}")
        } else {
            format!("schema v{held} (this build: v{expected})")
        };
        format!(
            "{name:<9} {:>10}  log {:>9}  reclaimable {:>9}  {schema}  {}",
            size(entry["bytes"].as_u64().unwrap_or(0)),
            size(entry["wal_bytes"].as_u64().unwrap_or(0)),
            size(entry["reclaimable_bytes"].as_u64().unwrap_or(0)),
            entry["path"].as_str().unwrap_or("")
        )
    });
    Ok(())
}

pub fn check(config: Option<&Path>, only: Option<&str>, as_json: bool) -> CliResult {
    let config = load(config)?;
    let mut damaged = Vec::new();
    let report: Vec<Value> = chosen(&config, only)?
        .into_iter()
        .map(|(name, _, path)| match path {
            Err(reason) => json!({"name": name, "present": false, "reason": reason}),
            Ok(path) => {
                let started = std::time::Instant::now();
                // A file SQLite cannot read at all is a failed check.
                let problems =
                    sqlite::integrity_check(&path).unwrap_or_else(|error| vec![error.to_string()]);
                if !problems.is_empty() {
                    damaged.push(name);
                }
                json!({
                    "name": name,
                    "present": true,
                    "ok": problems.is_empty(),
                    "problems": problems,
                    "took_ms": started.elapsed().as_millis() as u64,
                })
            }
        })
        .collect();
    print(as_json, &report, |entry| {
        let name = entry["name"].as_str().unwrap_or_default();
        if entry["present"] == false {
            return format!("{name:<9} none: {}", entry["reason"].as_str().unwrap_or(""));
        }
        if entry["ok"] == true {
            return format!(
                "{name:<9} ok  ({:.1} s)",
                entry["took_ms"].as_u64().unwrap_or(0) as f64 / 1000.0
            );
        }
        let problems: Vec<&str> = entry["problems"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        format!("{name:<9} DAMAGED\n  {}", problems.join("\n  "))
    });
    if damaged.is_empty() {
        Ok(())
    } else {
        Err(CliError::Runtime(format!(
            "integrity check failed: {}",
            damaged.join(", ")
        )))
    }
}

pub fn checkpoint(config: Option<&Path>, only: Option<&str>, as_json: bool) -> CliResult {
    let config = load(config)?;
    let mut failed = Vec::new();
    let report: Vec<Value> = chosen(&config, only)?
        .into_iter()
        .map(|(name, _, path)| match path {
            Err(reason) => json!({"name": name, "present": false, "reason": reason}),
            Ok(path) => {
                let before = wal_bytes(&path);
                match sqlite::checkpoint(&path) {
                    Ok(done) => json!({
                        "name": name,
                        "present": true,
                        "complete": done.complete,
                        "wal_bytes_before": before,
                        "wal_bytes_after": wal_bytes(&path),
                    }),
                    Err(error) => {
                        failed.push(name);
                        json!({"name": name, "present": true, "error": error.to_string()})
                    }
                }
            }
        })
        .collect();
    print(as_json, &report, |entry| {
        let name = entry["name"].as_str().unwrap_or_default();
        if entry["present"] == false {
            return format!("{name:<9} none: {}", entry["reason"].as_str().unwrap_or(""));
        }
        if let Some(error) = entry["error"].as_str() {
            return format!("{name:<9} FAILED: {error}");
        }
        let sizes = format!(
            "log {} -> {}",
            size(entry["wal_bytes_before"].as_u64().unwrap_or(0)),
            size(entry["wal_bytes_after"].as_u64().unwrap_or(0))
        );
        if entry["complete"] == true {
            format!("{name:<9} {sizes}")
        } else {
            format!("{name:<9} {sizes}  (a reader was in the way; the rest folds in next time)")
        }
    });
    if failed.is_empty() {
        Ok(())
    } else {
        Err(CliError::Runtime(format!(
            "checkpoint failed: {}",
            failed.join(", ")
        )))
    }
}
