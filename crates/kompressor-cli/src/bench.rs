//! `kompressor bench replay`: the corpus, the engine, and a verdict.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use kompressor_bench::{
    Payload, Tolerance, compare_baseline, load_baseline, load_corpus, replay, save_baseline,
    write_corpus,
};
use kompressor_engine::options::EngineOptions;
use kompressor_engine::refs::ref_for;
use kompressor_engine::types::Mode;
use kompressor_store::Tenant;
use kompressor_store::sqlite::SqliteRefStore;
use serde_json::json;

use crate::compress::print_json;
use crate::error::{CliError, CliResult};

/// Arguments of `kompressor bench replay`.
pub struct ReplayArgs {
    pub corpus: PathBuf,
    pub mode: Mode,
    /// Configuration whose `[engine]` section the replay runs under.
    pub config: Option<PathBuf>,
    pub save: Option<PathBuf>,
    pub compare: Option<PathBuf>,
    pub check_determinism: bool,
    pub json: bool,
}

/// Replay a corpus, optionally recording or checking a baseline.
///
/// `--compare` exits 3 on a regression rather than 1, so a CI job can tell a
/// worse engine from a broken harness without reading the output.
pub fn replay_corpus(args: ReplayArgs) -> CliResult {
    if args.save.is_some() && args.compare.is_some() {
        return Err(CliError::Usage(
            "--save records this run and --compare checks it against a recorded one; pick one"
                .to_string(),
        ));
    }

    let payloads = load_corpus(&args.corpus).map_err(|e| CliError::Runtime(e.to_string()))?;
    // The engine section comes from the configuration layer, so a knob can be
    // A/B'd against a corpus without rebuilding. Without this the harness ran
    // the compiled-in defaults whatever the environment said, which quietly
    // makes two runs that were meant to differ identical.
    //
    // `--config` only, though: with none given this took `Config::load(None)`,
    // which falls back to discovery, so the replay ran under whatever
    // `kompressor.toml` or `~/.config/kompressor/config.toml` the machine
    // happened to have. That turns the baseline gate into a measurement of the
    // developer's config rather than of the engine. It is not hypothetical —
    // one `enable_normalised_text_fold = false` in a personal config moved the
    // `text` bucket from 61.20% to 14.15% and `make bench` reported it as a
    // regression in the code, against a baseline recorded without it. The
    // hermetic CLI tests already pin discovery at an empty directory for the
    // same reason; the gate itself had no such protection.
    //
    // So: an explicit path is read, and nothing else is. No discovery and no
    // `KOMPRESSOR_*` overlay, because `--save` writes the baseline that
    // `--compare` checks and both have to mean the same thing on every
    // machine.
    let config = match args.config.as_deref() {
        Some(path) => {
            kompressor_core::Config::load(Some(path)).map_err(|e| CliError::Usage(e.to_string()))?
        }
        None => kompressor_core::Config::default(),
    };
    let options = EngineOptions {
        mode: args.mode,
        ..config.engine
    };

    // A comparison is only worth as much as its safety checks, and determinism
    // is the one invariant that costs a second pass. Forcing it on for
    // --compare means a recorded pass always means the same thing.
    let verify = args.check_determinism || args.compare.is_some();
    let report = replay(&payloads, &options, verify);

    let comparison = match &args.compare {
        Some(path) => {
            let baseline = load_baseline(path).map_err(|e| CliError::Runtime(e.to_string()))?;
            Some(compare_baseline(&report, &baseline, Tolerance::default()))
        }
        None => None,
    };

    if args.json {
        let mut value = json!({
            "corpus": args.corpus.display().to_string(),
            "baseline": report.to_baseline(),
        });
        if let Some(comparison) = &comparison {
            value["comparison"] = serde_json::to_value(comparison)
                .map_err(|e| CliError::runtime("rendering the comparison", e))?;
        }
        print_json(&value)?;
    } else {
        print!("{}", report.render_table());
        if let Some(comparison) = &comparison {
            println!();
            println!(
                "against {}:",
                args.compare.as_ref().expect("checked").display()
            );
            print!("{}", comparison.render());
        }
    }

    if let Some(path) = &args.save {
        save_baseline(&report, path).map_err(|e| CliError::Runtime(e.to_string()))?;
        if !args.json {
            println!("recorded {}", path.display());
        }
    }

    // A dirty run fails even without a baseline: an invariant violation is not
    // a number to compare, it is a defect.
    if !report.is_clean() {
        return Err(CliError::Runtime(format!(
            "{} safety invariant violation(s) on this corpus",
            report.violations().values().sum::<u64>()
        )));
    }
    match comparison {
        Some(comparison) if !comparison.passed => Err(CliError::Regressed),
        _ => Ok(()),
    }
}

/// Arguments of `kompressor bench extract`.
pub struct ExtractArgs {
    /// Run dumps to walk, or `kompressor-refs.db` files directly.
    pub from: Vec<PathBuf>,
    pub out: PathBuf,
    /// Payloads shorter than this are dropped: below the engine's own gate
    /// they measure the gate rather than a compressor.
    pub min_bytes: usize,
    /// Substrings whose payloads are dropped, for excluding a harness's own
    /// fixtures from a corpus meant to be real traffic.
    pub exclude: Vec<String>,
    /// Recorded on every payload, for stratifying a replay.
    pub task: Option<String>,
    /// Share of control characters above which a payload is treated as binary.
    pub max_control_ratio: f64,
    pub json: bool,
}

/// Share of `content` that is control characters or replacement markers.
///
/// A corpus is agent tool *text*. An agent that `cat`s a SQLite file produces
/// a fragment that is valid UTF-8 and is not text in any sense the compressors
/// are built for, and replaying it measures the engine on input nobody meant
/// to serve it. Measured on 476 real fragments the separation is absolute —
/// two payloads at 0.51 and 0.28, every other one at exactly 0.0 — so this
/// threshold does not sit in the middle of a distribution.
///
/// `\t`, `\n` and `\r` are text: a redraw is made of them.
fn control_ratio(content: &str) -> f64 {
    if content.is_empty() {
        return 0.0;
    }
    let bad = content
        .chars()
        .filter(|ch| {
            let c = *ch as u32;
            c == 0xFFFD || (c < 32 && !matches!(c, 9 | 10 | 13))
        })
        .count();
    bad as f64 / content.chars().count() as f64
}

/// Build a corpus out of what real runs actually elided.
///
/// The corpus that ships was extracted by hand from one run, and by the time
/// this was written it was two weeks old and described a different engine:
/// `search` was 18% of everything production deferred behind a handle and the
/// corpus held ONE compressible sample of it, so the quality floor for the
/// second-largest real category rested on n=1. A corpus that cannot be rebuilt
/// is a corpus that goes stale, and a stale corpus sets floors for traffic
/// nobody sends any more.
///
/// Reads the ref store rather than a payload log, because the ref store is
/// what a run leaves behind by default. That bounds what can be recovered:
/// only content that became a HANDLE is here. A lossless reduction never mints
/// one, so `log` is under-represented by construction — the extraction is
/// honest about that rather than pretending to be a sample of all traffic.
pub fn extract_corpus(args: ExtractArgs) -> CliResult {
    let mut stores = Vec::new();
    for root in &args.from {
        collect_ref_stores(root, &mut stores);
    }
    if stores.is_empty() {
        return Err(CliError::Usage(format!(
            "no kompressor-refs.db found under: {}",
            args.from
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    // Keyed by content, not by handle: the same payload elided in two runs is
    // one corpus entry carrying the weight of both, which is what `occurrences`
    // and `requests` are for.
    let mut seen: BTreeMap<String, (u32, BTreeSet<String>)> = BTreeMap::new();
    let mut scanned = 0usize;
    let mut skipped_small = 0usize;
    let mut skipped_excluded = 0usize;
    let mut skipped_binary = 0usize;
    for path in &stores {
        let store = SqliteRefStore::open(path)
            .map_err(|e| CliError::runtime(&format!("opening {}", path.display()), e))?;
        let records = store
            .scan(&Tenant::anonymous())
            .map_err(|e| CliError::runtime(&format!("reading {}", path.display()), e))?;
        for record in records {
            scanned += 1;
            if record.fragment.len() < args.min_bytes {
                skipped_small += 1;
                continue;
            }
            if args.exclude.iter().any(|s| record.fragment.contains(s)) {
                skipped_excluded += 1;
                continue;
            }
            if control_ratio(&record.fragment) > args.max_control_ratio {
                skipped_binary += 1;
                continue;
            }
            let entry = seen
                .entry(record.fragment)
                .or_insert_with(|| (0, BTreeSet::new()));
            entry.0 += 1;
            if let Some(id) = record.request_id {
                entry.1.insert(id);
            }
        }
    }

    let payloads: Vec<Payload> = seen
        .into_iter()
        .map(|(content, (occurrences, requests))| Payload {
            content_id: ref_for(&content).body().to_string(),
            role: "tool".to_string(),
            content,
            // The ref store keeps the fragment, not the turn around it. Left
            // empty rather than invented: the replay uses it only to prove the
            // engine does NOT read it, and a fabricated value would prove that
            // against fiction.
            preceding_user_text: String::new(),
            source_tool: String::new(),
            occurrences,
            requests: requests.len() as u32,
            task: args.task.clone().unwrap_or_default(),
        })
        .collect();

    if payloads.is_empty() {
        return Err(CliError::Runtime(
            "every fragment was filtered out; lower --min-bytes or drop an --exclude".to_string(),
        ));
    }

    write_corpus(&args.out, &payloads).map_err(|e| CliError::Runtime(e.to_string()))?;

    if args.json {
        print_json(&json!({
            "out": args.out.display().to_string(),
            "stores": stores.len(),
            "scanned": scanned,
            "payloads": payloads.len(),
            "skipped_below_min_bytes": skipped_small,
            "skipped_excluded": skipped_excluded,
            "skipped_binary": skipped_binary,
        }))?;
    } else {
        println!(
            "{} payload(s) from {} fragment(s) across {} store(s) -> {}",
            payloads.len(),
            scanned,
            stores.len(),
            args.out.display()
        );
        if skipped_small > 0 {
            println!("  {skipped_small} below --min-bytes {}", args.min_bytes);
        }
        if skipped_excluded > 0 {
            println!("  {skipped_excluded} matched --exclude");
        }
        if skipped_binary > 0 {
            println!(
                "  {skipped_binary} above --max-control-ratio {} (binary, not tool text)",
                args.max_control_ratio
            );
        }
    }
    Ok(())
}

/// Find every `kompressor-refs.db` under `root`, or take `root` itself.
fn collect_ref_stores(root: &Path, out: &mut Vec<PathBuf>) {
    if root.is_file() {
        out.push(root.to_path_buf());
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_ref_stores(&path, out);
        } else if path.file_name().is_some_and(|n| n == "kompressor-refs.db") {
            out.push(path);
        }
    }
}
