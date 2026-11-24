//! Why the engine did what it did to one payload.
//!
//! This exists because of a specific, repeated waste of operator time. A marker
//! with no handle — `[... 412 matches omitted]` rather than `[... 412 matches
//! omitted [ref:k_AbC123]]` — was read over and over as "recoverable mode is
//! off", and the investigation that followed went looking at configuration.
//! Every time, the mode was on and the elided fragment was simply over
//! `max_ref_bytes`, so there was nothing to point a handle at. Nothing in the
//! output distinguishes the two cases, and nothing could: the marker is the
//! same marker.
//!
//! So this says which one happened, along with everything else that is decided
//! and then thrown away: what the detector called the payload, which compressor
//! that selected, whether the segmenter or whole-message routing won, whether
//! the redraw fold was arbitrated and whether it won, every marker with its
//! handle, and the accounted guard's verdict.
//!
//! Everything here is reconstructed from the engine's public surface rather
//! than instrumented inside it. The pipeline stays free of a reporting mode it
//! would have to keep correct, and the two answers that cannot be observed from
//! outside — which reading won, and whether the fold won — are each derived
//! from a fact that only one branch can produce: only the segmenter reports
//! [`Kind::Mixed`], and only the folded branch emits the redraw marker.

use kompressor_engine::detect::{detect_result, strategy_for};
use kompressor_engine::markers::{has_envelope, has_marker, whitespace_only};
use kompressor_engine::mixed::has_mixed_content;
use kompressor_engine::options::EngineOptions;
use kompressor_engine::redraw::collapse_redraws;
use kompressor_engine::refs::{RefCollector, refs_in};
use kompressor_engine::text::chars_len;
use kompressor_engine::types::{Detection, ForcedType, Kind, Mode, Strategy};
use serde::Serialize;

/// The redraw marker's stem. Only the folded branch of the pipeline emits it,
/// which is what makes "did the fold win" answerable from the output alone.
const REDRAW_STEM: &str = "overwritten progress";

/// Everything the engine decided about one payload.
#[derive(Debug, Serialize)]
pub struct Explanation {
    pub chars_in: usize,
    pub chars_out: usize,
    pub saved_pct: f64,
    pub mode: String,
    pub role: String,
    /// Whether this role is withheld from compression entirely.
    pub role_protected: bool,
    /// Whether the payload reached `min_compress_chars`.
    pub size_gate_passed: bool,
    pub min_compress_chars: usize,
    pub detection: DetectionView,
    /// The compressor the detected type selects, with the enable flags applied.
    pub strategy: String,
    /// Set when the caller forced a type and detection was bypassed.
    pub forced_type: Option<String>,
    pub route: RouteView,
    pub redraw: RedrawView,
    /// The type the engine reported, or absent when the payload came back
    /// untouched.
    pub kind: Option<String>,
    /// Constituent types actually compressed, so a heterogeneous body unfolds
    /// into the real types instead of the opaque `mixed`.
    pub applied: Vec<String>,
    pub markers: Vec<MarkerView>,
    pub refs: Vec<RefView>,
    /// Present when a marker was emitted with no handle, saying which of the
    /// two causes it was.
    pub handleless_marker_reason: Option<String>,
    pub accounted: AccountedView,
}

/// What the detector saw.
#[derive(Debug, Serialize)]
pub struct DetectionView {
    pub content_type: String,
    pub confidence: f32,
    pub language: Option<String>,
    pub config_flavor: Option<String>,
    pub item_count: Option<usize>,
    /// Detection stopped at the size gate and never classified anything.
    pub skipped_size: bool,
    pub concatenated_json: bool,
    pub diagnostics: bool,
    pub untimestamped: bool,
    pub uniform_stream: bool,
    /// A uniform stream carrying a crash, which routes to the scored log path
    /// rather than the head-and-tail sampler. Surfaced here because that
    /// diversion is otherwise invisible: the type still reads `listing`.
    pub error_signal: bool,
    pub sample_truncated: bool,
}

impl DetectionView {
    /// Flatten a [`Detection`], including what the detector learned on the way.
    pub fn new(detection: &Detection) -> DetectionView {
        DetectionView {
            content_type: detection.content_type.as_str().to_string(),
            confidence: detection.confidence,
            language: detection.meta.language.map(|l| l.as_str().to_string()),
            config_flavor: detection.meta.flavor.map(|f| f.as_str().to_string()),
            item_count: detection.meta.item_count,
            skipped_size: detection.meta.skipped_size,
            concatenated_json: detection.meta.concatenated_json,
            diagnostics: detection.meta.diagnostics,
            untimestamped: detection.meta.untimestamped,
            uniform_stream: detection.meta.uniform_stream,
            error_signal: detection.meta.error_signal,
            sample_truncated: detection.meta.sample_truncated,
        }
    }
}

/// Which of the two readings of the payload won.
#[derive(Debug, Serialize)]
pub struct RouteView {
    /// Whether the segmenter was offered the payload at all.
    pub segmenter_offered: bool,
    /// `segmenter`, `whole_message`, or `none` when nothing shipped.
    pub winner: String,
    /// Why, in one sentence.
    pub reason: String,
}

/// The redraw fold: arbitrated, not applied.
#[derive(Debug, Serialize)]
pub struct RedrawView {
    /// Whether the payload holds lines with carriage returns to resolve.
    pub candidate: bool,
    /// Whether the fold was actually arbitrated — it is skipped without a
    /// collector, in lossless mode, under a forced type and below the gate.
    pub arbitrated: bool,
    /// Characters the fold would remove before routing.
    pub chars_resolved: usize,
    /// Lines the fold would rewrite.
    pub lines: usize,
    /// Whether the folded reading beat the plain one.
    pub won: bool,
}

/// One marker in the output, with the handle it carries.
#[derive(Debug, Serialize)]
pub struct MarkerView {
    pub text: String,
    /// The handle embedded in the marker, if it has one.
    pub handle: Option<String>,
}

/// One retained fragment.
#[derive(Debug, Serialize)]
pub struct RefView {
    pub handle: String,
    pub bytes: usize,
    /// Whether the output names this handle. False is an orphan: a write
    /// nothing can read.
    pub named_by_output: bool,
}

/// The guard that refuses an unexplained shrink.
#[derive(Debug, Serialize)]
pub struct AccountedView {
    pub marker_present: bool,
    pub envelope_present: bool,
    pub whitespace_only: bool,
    pub decision: String,
}

/// What the pipeline returned for one payload.
pub struct Outcome<'a> {
    pub output: &'a str,
    pub kind: Option<Kind>,
    /// Constituent types actually compressed.
    pub applied: &'a [Kind],
    pub refs: &'a RefCollector,
}

/// Assemble the explanation from the inputs and the engine's answer.
///
/// The [`Outcome`] is the pipeline's own output for this run; everything else
/// is re-derived, which costs one extra detection pass and one extra redraw
/// scan on a payload the operator is already looking at by hand.
pub fn explain(
    content: &str,
    role: &str,
    options: &EngineOptions,
    forced: Option<ForcedType>,
    outcome: Outcome<'_>,
) -> Explanation {
    let Outcome {
        output,
        kind,
        applied,
        refs,
    } = outcome;
    let chars_in = chars_len(content);
    let chars_out = chars_len(output);
    let detection = detect_result(content, role, options);
    let size_gate_passed = chars_in >= options.min_compress_chars;

    let markers = markers_in(output);
    let named = refs_in(output);
    let handleless = markers.iter().any(|m| m.handle.is_none());

    Explanation {
        chars_in,
        chars_out,
        saved_pct: if chars_in == 0 {
            0.0
        } else {
            (chars_in.saturating_sub(chars_out)) as f64 * 100.0 / chars_in as f64
        },
        mode: options.mode.to_string(),
        role: role.to_string(),
        role_protected: options.is_protected_role(role),
        size_gate_passed,
        min_compress_chars: options.min_compress_chars,
        detection: DetectionView::new(&detection),
        strategy: strategy_name(strategy_for(&detection, options)).to_string(),
        forced_type: forced.map(|f| forced_name(f).to_string()),
        route: route_view(content, options, forced, kind, size_gate_passed),
        redraw: redraw_view(content, output, options, forced, size_gate_passed),
        kind: kind.map(|k| k.as_str().to_string()),
        applied: applied.iter().map(|k| k.as_str().to_string()).collect(),
        handleless_marker_reason: handleless.then(|| handleless_reason(options)),
        markers,
        refs: refs
            .pairs()
            .iter()
            .map(|(id, fragment)| RefView {
                handle: id.to_string(),
                bytes: fragment.len(),
                named_by_output: named.contains(id),
            })
            .collect(),
        accounted: accounted_view(content, output, options, kind),
    }
}

/// Which reading of the payload won.
fn route_view(
    content: &str,
    options: &EngineOptions,
    forced: Option<ForcedType>,
    kind: Option<Kind>,
    size_gate_passed: bool,
) -> RouteView {
    let offered = forced.is_none()
        && options.enable_mixed_content
        && size_gate_passed
        && has_mixed_content(content, options);

    // Only the segmenter path reports `mixed`: whole-message routing takes its
    // kind from the detected content type, which is never `mixed`.
    let (winner, reason) = match kind {
        Some(Kind::Mixed) => (
            "segmenter",
            "the segmenter compressed the structured spans in place and earned the payload",
        ),
        Some(_) if offered => (
            "whole_message",
            "the segmenter was offered the payload and did not save enough of it, so one \
             compressor took the whole body",
        ),
        Some(_) => (
            "whole_message",
            "the payload is homogeneous, so it went straight to the compressor its type selects",
        ),
        None if offered => (
            "none",
            "neither reading shipped: the segmenter did not earn it and whole-message routing \
             declined or was refused as unaccounted",
        ),
        None => (
            "none",
            "nothing shipped: the compressor declined, or its shrink was refused as unaccounted",
        ),
    };

    RouteView {
        segmenter_offered: offered,
        winner: winner.to_string(),
        reason: reason.to_string(),
    }
}

/// The redraw arbitration, if it ran.
fn redraw_view(
    content: &str,
    output: &str,
    options: &EngineOptions,
    forced: Option<ForcedType>,
    size_gate_passed: bool,
) -> RedrawView {
    // The same conditions the pipeline applies before it computes a second
    // reading at all. A collector is required because a fold announces itself
    // with a marker, and a marker whose handle has nowhere to be stored is not
    // a trade worth making.
    let arbitrated = options.enable_redraw_fold
        && options.mode == Mode::Recoverable
        && forced.is_none()
        && size_gate_passed;

    match collapse_redraws(content) {
        Some((collapsed, frames)) => RedrawView {
            candidate: true,
            arbitrated,
            chars_resolved: chars_len(content).saturating_sub(chars_len(&collapsed)),
            lines: frames.len(),
            won: output.contains(REDRAW_STEM),
        },
        None => RedrawView {
            candidate: false,
            arbitrated,
            chars_resolved: 0,
            lines: 0,
            won: false,
        },
    }
}

/// The accounted guard's verdict, in the order the guard itself asks.
fn accounted_view(
    content: &str,
    output: &str,
    options: &EngineOptions,
    kind: Option<Kind>,
) -> AccountedView {
    let marker_present = has_marker(output);
    let envelope_present = has_envelope(output);
    let layout_only = whitespace_only(content, output);

    let decision = if kind.is_none() {
        "nothing shipped, so the guard was not reached: either every compressor declined, or a \
         shrink was refused for carrying no marker"
    } else if options.mode == Mode::Lossless {
        "accepted: lossless mode is exempt, because every transform it allows is reversible by \
         construction"
    } else if marker_present {
        "accepted: the output carries an omission marker"
    } else if envelope_present {
        "accepted: the output declares a reversible columnar envelope, reported as factored"
    } else if layout_only {
        "accepted: nothing but layout changed, and the gain paid for the rewrite; reported as \
         reformat"
    } else {
        "inconsistent: a shrink shipped with no marker, no envelope and a content change — this \
         is the case the guard exists to refuse"
    };

    AccountedView {
        marker_present,
        envelope_present,
        whitespace_only: layout_only,
        decision: decision.to_string(),
    }
}

/// Why a marker has no handle — the whole reason this command exists.
fn handleless_reason(options: &EngineOptions) -> String {
    if options.mode == Mode::Recoverable {
        format!(
            "the elided fragment was larger than max_ref_bytes ({}), so the marker announces the \
             elision with nothing to point at. Recoverable mode is on; this is a size cap, not a \
             mode.",
            options.max_ref_bytes
        )
    } else {
        format!(
            "retention is off in {} mode: only recoverable mints handles, so every marker is \
             plain.",
            options.mode
        )
    }
}

/// Top-level bracket spans that carry a marker.
///
/// Only the outermost span is reported: a marker embeds its handle in a nested
/// bracket (`[... 3 matches omitted [ref:k_AbC123] in src/app.rs]`), and the
/// handle on its own is a fragment of the marker rather than a marker of its
/// own.
fn markers_in(output: &str) -> Vec<MarkerView> {
    const MAX_MARKER_CHARS: usize = 160;
    let mut found = Vec::new();
    let bytes = output.as_bytes();
    let mut depth = 0usize;
    let mut start = 0usize;

    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'[' => {
                if depth == 0 {
                    start = index;
                }
                depth += 1;
            }
            b']' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    let span = &output[start..=index];
                    if has_marker(span) {
                        found.push(MarkerView {
                            text: truncate(span, MAX_MARKER_CHARS),
                            handle: refs_in(span).first().map(|id| id.to_string()),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    found
}

/// Clip to `max` characters, saying so.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}…")
}

/// The compressor a strategy names, in the vocabulary the docs use.
pub fn strategy_name(strategy: Strategy) -> &'static str {
    match strategy {
        Strategy::SmartCrusher => "smart_crusher",
        Strategy::Search => "search",
        Strategy::Log => "log",
        Strategy::Diff => "diff",
        Strategy::CodeAware => "code_aware",
        Strategy::Tabular => "tabular",
        Strategy::Listing => "listing",
        Strategy::Config => "config",
        Strategy::Passthrough => "passthrough",
    }
}

/// The name a forced type was given on the command line.
fn forced_name(forced: ForcedType) -> &'static str {
    match forced {
        ForcedType::Json => "json",
        ForcedType::Log => "log",
        ForcedType::Text => "text",
        ForcedType::Config => "config",
    }
}

impl Explanation {
    /// The explanation as lines a human reads top to bottom, with colour
    /// wrapped around the parts worth finding at a glance. Written through
    /// [`crate::highlight::write_stderr`], the colour is stripped whenever it is
    /// off, and what is left is the same text, byte for byte, as before colour
    /// existed.
    pub fn render(&self) -> String {
        use crate::highlight::{BAD, GOOD, HANDLE, LABEL, MARKER, VALUE, WARN, paint, sanitize};
        use std::fmt::Write as _;
        let label = |name: &str| paint(LABEL, name);
        let mut out = String::new();

        let _ = writeln!(
            out,
            "{}{} chars, role {}{}",
            label("input        "),
            self.chars_in,
            paint(VALUE, &self.role),
            if self.role_protected {
                paint(BAD, " (PROTECTED: this role is never compressed)")
            } else {
                String::new()
            }
        );
        let _ = writeln!(
            out,
            "{}{} (min_compress_chars {})",
            label("size gate    "),
            if self.size_gate_passed {
                paint(GOOD, "passed")
            } else {
                paint(
                    WARN,
                    "BELOW THE GATE: the payload was left alone without being classified",
                )
            },
            self.min_compress_chars
        );

        match &self.forced_type {
            Some(forced) => {
                let _ = writeln!(
                    out,
                    "{}{} (detection bypassed)",
                    label("forced type  "),
                    paint(VALUE, forced)
                );
            }
            None => {
                let _ = writeln!(
                    out,
                    "{}{} (confidence {:.2}){}{}",
                    label("detected     "),
                    paint(VALUE, &self.detection.content_type),
                    self.detection.confidence,
                    self.detection
                        .language
                        .as_ref()
                        .map(|l| format!(", language {}", paint(VALUE, l)))
                        .unwrap_or_default(),
                    self.detection
                        .config_flavor
                        .as_ref()
                        .map(|f| format!(", flavor {}", paint(VALUE, f)))
                        .unwrap_or_default(),
                );
            }
        }
        let _ = writeln!(
            out,
            "{}{}",
            label("strategy     "),
            paint(VALUE, &self.strategy)
        );
        let _ = writeln!(
            out,
            "{}{} — {}",
            label("route        "),
            paint(VALUE, &self.route.winner),
            self.route.reason
        );
        let _ = writeln!(
            out,
            "{}segmenter offered: {}",
            label("             "),
            self.route.segmenter_offered
        );

        if self.redraw.candidate {
            let _ = writeln!(
                out,
                "{}{} lines resolve to {} fewer chars; arbitrated: {}; {}",
                label("redraw       "),
                self.redraw.lines,
                self.redraw.chars_resolved,
                self.redraw.arbitrated,
                if self.redraw.won {
                    paint(VALUE, "the folded reading won")
                } else {
                    "the plain reading won".to_string()
                }
            );
        } else {
            let _ = writeln!(
                out,
                "{}no overwritten lines in this payload",
                label("redraw       ")
            );
        }

        let removed = format!("{:.2}% removed", self.saved_pct);
        let _ = writeln!(
            out,
            "{}{} chars ({}), reported as {}",
            label("result       "),
            self.chars_out,
            if self.saved_pct > 0.0 {
                paint(GOOD, &removed)
            } else {
                removed
            },
            paint(VALUE, self.kind.as_deref().unwrap_or("untouched"))
        );
        if !self.applied.is_empty() {
            let _ = writeln!(
                out,
                "{}{}",
                label("applied      "),
                paint(VALUE, &self.applied.join(", "))
            );
        }

        let decision = &self.accounted.decision;
        let (verdict, rest) = decision
            .split_once(':')
            .map_or((decision.as_str(), ""), |(head, _)| {
                (head, &decision[head.len()..])
            });
        let verdict_style = match verdict {
            "accepted" => GOOD,
            "inconsistent" => BAD,
            _ => WARN,
        };
        let _ = writeln!(
            out,
            "{}{}{}",
            label("accounted    "),
            paint(verdict_style, verdict),
            rest
        );

        if self.markers.is_empty() {
            let _ = writeln!(out, "{}none", label("markers      "));
        } else {
            let _ = writeln!(out, "{}{}", label("markers      "), self.markers.len());
            for marker in &self.markers {
                let _ = writeln!(
                    out,
                    "  {} {}",
                    match &marker.handle {
                        Some(handle) => paint(HANDLE, handle),
                        None => paint(WARN, "(no handle)"),
                    },
                    paint(MARKER, &sanitize(&marker.text))
                );
            }
        }
        if let Some(reason) = &self.handleless_marker_reason {
            let _ = writeln!(out, "{}{}", label("             "), paint(WARN, reason));
        }

        if self.refs.is_empty() {
            let _ = writeln!(out, "{}none retained", label("refs         "));
        } else {
            let _ = writeln!(
                out,
                "{}{} retained",
                label("refs         "),
                self.refs.len()
            );
            for reference in &self.refs {
                let _ = writeln!(
                    out,
                    "  {} {} bytes{}",
                    paint(HANDLE, &reference.handle),
                    reference.bytes,
                    if reference.named_by_output {
                        String::new()
                    } else {
                        paint(BAD, "  ORPHANED: the output never names this handle")
                    }
                );
            }
        }

        out
    }

    /// How `--show-output` should read the compressed payload: by the
    /// detector's verdict, or by the type the caller forced.
    pub fn hint(&self) -> crate::highlight::Hint {
        match &self.forced_type {
            Some(forced) => crate::highlight::Hint::from_names(forced, None, None),
            None => crate::highlight::Hint::from_names(
                &self.detection.content_type,
                self.detection.language.as_deref(),
                self.detection.config_flavor.as_deref(),
            ),
        }
    }
}
