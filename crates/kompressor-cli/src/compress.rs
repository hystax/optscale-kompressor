//! `kompressor compress` and `kompressor detect`.

use kompressor_engine::detect::{detect_result, strategy_for};
use kompressor_engine::options::EngineOptions;
use kompressor_engine::pipeline::compress_content;
use kompressor_engine::types::{Context, ForcedType, Kind, Mode};
use serde::Serialize;
use serde_json::json;

use crate::error::{CliError, CliResult};
use crate::explain::{DetectionView, Explanation, Outcome, explain, strategy_name};
use crate::input::read_input;

/// The engine options a configuration selects, so the CLI and the server make
/// the same decisions about the same payload.
fn load_engine(path: Option<&std::path::Path>) -> Result<EngineOptions, CliError> {
    Ok(kompressor_core::Config::load(path)
        .map_err(|e| CliError::Usage(e.to_string()))?
        .engine)
}

/// Arguments of `kompressor compress`.
pub struct CompressArgs {
    pub file: Option<String>,
    pub config: Option<std::path::PathBuf>,
    pub mode: Mode,
    pub role: String,
    pub forced: Option<ForcedType>,
    pub json: bool,
    pub explain: bool,
    /// Also show the compressed payload on standard error, highlighted.
    pub show_output: bool,
    /// When standard error gets colour. Standard output never does.
    pub color: crate::highlight::ColorWhen,
}

/// What a compression produced, for `--json`.
#[derive(Serialize)]
struct CompressJson {
    mode: String,
    role: String,
    kind: Option<String>,
    chars_in: usize,
    chars_out: usize,
    saved_pct: f64,
    applied: Vec<String>,
    refs: Vec<RefJson>,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    explain: Option<Explanation>,
}

#[derive(Serialize)]
struct RefJson {
    handle: String,
    bytes: usize,
}

/// Compress one payload.
///
/// Without `--json` the compressed payload is the only thing on standard
/// output, so the command composes in a pipeline; `--explain` writes its report
/// to standard error for the same reason. With `--json` everything, explanation
/// included, is one object on standard output.
pub fn compress(args: CompressArgs) -> CliResult {
    let content = read_input(args.file.as_deref())?;
    // From the configuration layer, not the compiled-in defaults: a command that
    // ignores the file and environment `check-config` prints back does not
    // compress what the server would, which makes it useless for reproducing
    // anything the server did — and silently turns an A/B of any engine flag
    // into two identical runs.
    let config = kompressor_core::Config::load(args.config.as_deref())
        .map_err(|e| CliError::Usage(e.to_string()))?;
    let options = EngineOptions {
        mode: args.mode,
        ..config.engine.clone()
    };
    options
        .validate()
        .map_err(|e| CliError::Usage(format!("engine options: {e}")))?;

    let mut refs = options.collector();
    let mut applied: Vec<Kind> = Vec::new();
    let ctx = Context::for_role(&args.role);

    // The role gate lives in the message-level API, which this command does not
    // go through; applying it here keeps `--role assistant` meaning the same
    // thing on the CLI as it does on the wire.
    let (mut output, kind) = if options.is_protected_role(&args.role) {
        (std::borrow::Cow::Borrowed(content.as_str()), None)
    } else {
        compress_content(
            &content,
            &args.role,
            &options,
            args.forced,
            &mut refs,
            &ctx,
            &mut applied,
        )
    };

    // A one-shot process cannot retain a memory store beyond its own exit.
    // Publish handles only after the configured durable store verifies them.
    if !refs.is_empty() {
        let retained = match config.refs.backend {
            kompressor_core::config::RefsBackend::Memory => false,
            kompressor_core::config::RefsBackend::Sqlite
            | kompressor_core::config::RefsBackend::Redis => {
                kompressor_core::build_ref_store(&config).is_ok_and(|store| {
                    kompressor_core::AppCore::new(
                        config.clone(),
                        store,
                        kompressor_stats::writer::null_sink(),
                    )
                    .retain_fragments(
                        &kompressor_core::Identity::anonymous(),
                        &refs,
                        "cli",
                        &options,
                    )
                })
            }
        };
        if !retained {
            eprintln!("could not durably retain fragments; returning the original payload");
            output = std::borrow::Cow::Borrowed(content.as_str());
            refs = options.collector();
            applied.clear();
        }
    }

    let chars_in = content.chars().count();
    let chars_out = output.chars().count();
    // `--show-output` reads the payload by the detector's verdict, so it needs
    // the explanation even when the report itself was not asked for.
    let explanation = (args.explain || args.show_output).then(|| {
        explain(
            &content,
            &args.role,
            &options,
            args.forced,
            Outcome {
                output: &output,
                kind,
                applied: &applied,
                refs: &refs,
            },
        )
    });

    // `--show-output` is rendered from the same explanation, then the
    // explanation is kept only if the report itself was asked for.
    let view = args.show_output.then(|| {
        crate::highlight::output_view(
            &output,
            explanation
                .as_ref()
                .map_or(crate::highlight::Hint::Plain, Explanation::hint),
        )
    });
    let explanation = explanation.filter(|_| args.explain);

    if args.json {
        let payload = CompressJson {
            mode: options.mode.to_string(),
            role: args.role.clone(),
            kind: kind.map(|k| k.as_str().to_string()),
            chars_in,
            chars_out,
            saved_pct: saved_pct(chars_in, chars_out),
            applied: applied.iter().map(|k| k.as_str().to_string()).collect(),
            refs: refs
                .pairs()
                .iter()
                .map(|(id, fragment)| RefJson {
                    handle: id.to_string(),
                    bytes: fragment.len(),
                })
                .collect(),
            content: output.into_owned(),
            explain: explanation,
        };
        print_json(&payload)?;
        // The JSON already carries the explanation; only the view is extra,
        // and it goes to standard error like every other human-facing part.
        if let Some(view) = &view {
            crate::highlight::write_stderr(args.color, view);
        }
    } else {
        print!("{output}");
        let mut report = explanation.map(|e| e.render()).unwrap_or_default();
        if let Some(view) = &view {
            report.push_str(view);
        }
        if !report.is_empty() {
            // Standard output first, and flushed, so a terminal shows the
            // payload and then the report. Each stream's bytes are unchanged.
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            crate::highlight::write_stderr(args.color, &report);
        }
    }
    Ok(())
}

/// Arguments of `kompressor detect`.
pub struct DetectArgs {
    pub file: Option<String>,
    pub config: Option<std::path::PathBuf>,
    pub role: String,
    pub json: bool,
}

/// Classify one payload without compressing it.
pub fn detect(args: DetectArgs) -> CliResult {
    let content = read_input(args.file.as_deref())?;
    let options = load_engine(args.config.as_deref())?;
    let detection = detect_result(&content, &args.role, &options);
    let strategy = strategy_name(strategy_for(&detection, &options));
    let view = DetectionView::new(&detection);

    if args.json {
        let mut value = serde_json::to_value(&view)
            .map_err(|e| CliError::runtime("rendering the detection", e))?;
        value["strategy"] = json!(strategy);
        value["chars"] = json!(content.chars().count());
        print_json(&value)?;
    } else {
        println!("content_type  {}", view.content_type);
        println!("confidence    {:.2}", view.confidence);
        println!("strategy      {strategy}");
        if let Some(language) = &view.language {
            println!("language      {language}");
        }
        if let Some(flavor) = &view.config_flavor {
            println!("config_flavor {flavor}");
        }
        if let Some(items) = view.item_count {
            println!("item_count    {items}");
        }
        if view.skipped_size {
            println!(
                "note          below min_compress_chars ({}): the payload was never classified",
                options.min_compress_chars
            );
        }
    }
    Ok(())
}

/// Percentage of `chars_in` removed.
pub fn saved_pct(chars_in: usize, chars_out: usize) -> f64 {
    if chars_in == 0 {
        0.0
    } else {
        chars_in.saturating_sub(chars_out) as f64 * 100.0 / chars_in as f64
    }
}

/// Print `value` as pretty JSON on standard output.
pub fn print_json<T: Serialize>(value: &T) -> CliResult {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| CliError::runtime("rendering the result as JSON", e))?;
    println!("{text}");
    Ok(())
}
