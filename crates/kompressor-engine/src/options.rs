//! Engine knobs, their defaults, and the per-request overlay.
//!
//! Almost every default here was measured into place against a corpus of real
//! agent tool output rather than chosen. The comments say what each one is
//! protecting; changing one is a corpus-replay change, not a taste change.

use crate::types::Mode;
use serde::{Deserialize, Serialize};

/// How the never-larger guarantee is measured.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TokenGuard {
    /// Compare code points.
    #[default]
    Chars,
    /// Also require the estimated token count to fall.
    ///
    /// Roughly a tenth of compressed payloads lose characters without losing
    /// tokens — compression strips cheap letters and spaces while keeping
    /// expensive digits and separators, so the remainder is denser than what it
    /// replaced. Opt-in until a corpus A/B says it should be the default.
    Tokens,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineOptions {
    pub mode: Mode,

    /// Payloads shorter than this are left alone entirely.
    ///
    /// 512 characters, about 128 tokens. It was 2048 until two benchmark runs
    /// showed agent tool results landing at 400-2,200 characters, so most were
    /// being skipped; the gate alone cost about 15 points of savings on one run.
    pub min_compress_chars: usize,

    /// Arrays longer than this are sampled rather than kept whole.
    pub max_array_items: usize,

    /// String values longer than this are routed to a nested compressor, or
    /// clipped if nothing claims them.
    pub max_field_chars: usize,

    /// Trailing lines kept verbatim by the log compressor: recent context is
    /// disproportionately load-bearing.
    pub keep_tail_lines: usize,

    /// Idle window for a retained fragment.
    pub ref_ttl_seconds: u64,

    /// Absolute lifetime cap, as a multiple of the idle window, so constant
    /// access cannot pin a fragment forever.
    pub ref_max_lifetime_multiplier: u32,

    /// Largest fragment worth retaining, in UTF-8 bytes.
    ///
    /// In `recoverable` mode this is a hard gate rather than a hint: an elision
    /// whose fragment does not fit is refused and the payload is returned
    /// whole, because the mode's whole promise is that what it removed can be
    /// fetched back.
    ///
    /// One mebibyte, not the 64 KiB this used to be. The smaller cap bit
    /// exactly the payloads compression exists for: measured at 65536, a
    /// 200 KB single-line payload came back at **0.00%** where a larger cap
    /// takes 99.72% of it, and a 266 KB repetitive log likewise returned
    /// untouched. `docs/CODEX.md` was already telling operators to raise it,
    /// which is the sign of a default in the wrong place.
    pub max_ref_bytes: usize,

    /// In `recoverable` mode, a payload whose compression leaves a handle but
    /// saves fewer characters than this ships whole instead. 0 turns it off.
    ///
    /// A handle is an invitation to fetch, and a fetch is a whole extra model
    /// round. Where re-sent content is billed as cache reads, a small elision
    /// saves less than one fetch costs. On Claude Opus 5.5 across ten
    /// recoverable Harbor runs, tool results that saved under 4,000 characters
    /// were fetched back 30% of the time: 402 of 535 handle-bearing results
    /// accounted for 20% of what compression saved and 114 of 265 fetch rounds.
    /// Shipping those whole came out about $1.3 ahead across the ten runs at
    /// $0.03 a round, and stayed ahead at $0.02. Markers without a handle are
    /// not gated: nothing can be fetched through them.
    pub min_elision_saving_chars: usize,

    pub enable_smart_crusher: bool,
    pub enable_search_compressor: bool,
    pub enable_log_compressor: bool,
    pub enable_diff_compressor: bool,
    pub enable_tabular_compressor: bool,
    pub enable_listing_compressor: bool,
    pub enable_code_aware: bool,
    pub enable_mixed_content: bool,
    pub enable_config_compressor: bool,
    /// Let the segmenter recognise a run of configuration inside a
    /// heterogeneous body.
    ///
    /// Without it the segmenter's vocabulary is diffs, logs, grep hits,
    /// tables and listings — all line-shaped. Configuration is not, so a
    /// tool result holding a source file and then a `.cfg` presented as
    /// one unclassified run, went whole to the detector's single winner,
    /// and compressed by nothing at all.
    pub enable_config_sections: bool,

    /// Re-detect and route long string values instead of blind-clipping them.
    ///
    /// Structured payloads routinely arrive wrapped: a build log in a `stdout`
    /// field, double-encoded JSON in a `body` field, a source file in a `file`
    /// field. A blind clip keeps the first `max_field_chars` and silently drops
    /// the rest, including a mid-stream error the log compressor would keep.
    pub enable_nested_routing: bool,

    /// Resolve the carriage returns a terminal progress meter leaves behind.
    pub enable_redraw_fold: bool,

    /// Classify a line-numbered payload by what is under the numbers.
    ///
    /// `27:#include <stdio.h>` is a mapping key to the config classifier, so
    /// `grep -n` over a C file is "yaml" with 0.80 confidence, the config
    /// compressor declines, and routing has already happened so the gutter lift
    /// never gets a turn. On seven Harbor runs that was 33 payloads returned
    /// whole; classifying the stripped body instead takes 53.7% off them.
    pub enable_gutter_aware_detection: bool,

    /// Let build output fold lines that differ only in a name.
    ///
    /// Build transcripts repeat a handful of line shapes once per artefact —
    /// `Setting up <package> (<version>) ...` — and the varying field is a
    /// name, not a number, so the ordinary value-normalised identity sees them
    /// all as distinct. Off, build output folds like any other log; on, it
    /// folds by name as well, with diagnostics excluded so two errors in
    /// different files can never collapse into one.
    pub enable_build_templates: bool,

    /// Mint a handle for a fold whose occurrences were byte-identical.
    ///
    /// Off by default, and the default is the point. `line [×47]` says the line
    /// occurred forty-seven times and that the occurrences were the same bytes,
    /// which is the entire fragment — a handle beside it offers a round trip
    /// for content the marker already carries. It is not free: in one measured
    /// Harbor run **241 of 275 stored fragments** were such repeats, 88% of
    /// every marker the model saw, averaging 64 bytes behind each, and not one
    /// was ever fetched. Markers the model learns to ignore are the reason the
    /// ones worth fetching go unfetched too.
    ///
    /// On, every fold keeps a handle, which is what an audit trail wanting one
    /// fetchable fragment per elision would ask for. A fold whose occurrences
    /// *differ* — the normalised identities, where the count cannot give the
    /// values back — keeps its handle either way.
    pub fold_handles_identical_repeats: bool,

    /// Say `similar` in a fold marker whose occurrences differ.
    ///
    /// `[×N ref:…]` is documented to a model as "a line that repeated", so over
    /// lines that merely shared a template it describes the handle as holding
    /// copies of something already visible — and a model with no reason to
    /// fetch does not fetch. Measured on eight attachment headings folded to
    /// one: the seven variants were retained and retrievable, and the task
    /// still lost the instruction they carried.
    ///
    /// On, such a fold reads `[×N similar ref:…]`. The handle, the fragment and
    /// the compression are identical either way; only what the model is told
    /// changes. Off restores the older wording, which is what the golden
    /// vectors were recorded under.
    pub fold_marks_variants: bool,

    /// Compress HTML by folding its repeated lines.
    ///
    /// HTML had no compressor at all: `strategy_for` had no arm for it, so it
    /// fell to `Passthrough` and every HTML payload was returned whole. That is
    /// not a small gap on agent traffic — a 40 KB page fetched by a tool call
    /// is generated markup, which means the same element repeated with a
    /// different number in it, and that is precisely what the log fold is for.
    /// Measured on a live codex session, one such page went from **0.00% to
    /// 94.46%**.
    ///
    /// There is no HTML *parser* here and this does not pretend to be one: the
    /// payload goes through the ordinary log route, so what it folds is
    /// repeated lines, and a page written as one long line is left alone the
    /// same way any other single-line payload is. In recoverable mode the
    /// folded occurrences keep a handle, so the differences between two nearly
    /// identical elements stay fetchable — which matters, because the one
    /// workload where those differences are the whole point is a set of XSS
    /// test vectors.
    pub enable_html_compressor: bool,

    /// Let a plain-text fold normalise values as well as matching lines exactly.
    ///
    /// Prose folds on byte-identical lines and nothing else: normalising its
    /// numbers would merge sentences differing only in one, and that has always
    /// been the reading applied here. It is the wrong reading for *generated*
    /// text, which is the other thing that arrives classified as plain — a
    /// markdown guide of 80 near-identical fenced blocks came back at 7.15%
    /// exact and 97.10% normalised, and a 700-section INI that its own
    /// compressor declined came back at 32.96% and 98.39%.
    ///
    /// On, both readings are computed and the shorter ships, with the exact one
    /// winning ties — so a payload whose lines repeat verbatim is untouched by
    /// this, and the normalised reading can only win where the lines actually
    /// differ. Corpus coverage 64.31% -> 68.72%, almost all of it the `text`
    /// bucket going from 16.63% to 62.71%: payloads the detector calls plain
    /// that are really source or data dumps.
    ///
    /// Off restores the exact-only fold. What the normalised reading removes
    /// stays behind a handle in recoverable mode, which is what makes it
    /// acceptable on anything whose values carry meaning.
    pub enable_normalised_text_fold: bool,

    /// Require a majority of lines to be table rows before calling a payload
    /// tabular.
    ///
    /// The pipe-table rule had a count and no ratio, alone among its
    /// neighbours: three rows anywhere claimed the whole payload. On the
    /// shipped corpus that was nine payloads with 6% to 23% of their lines
    /// matching — dataset cards and READMEs with a small table in them, curl
    /// progress, rustc errors — every one handed to a compressor that declines
    /// prose, and every one compressing at roughly nothing.
    ///
    /// On, those nine reclassify (all of them to `text`, which is the
    /// measurement rather than the guess) and corpus coverage goes 68.78% ->
    /// 69.95%, the `tabular` bucket from 15 payloads at 28.12% to 6 at 76.96%.
    /// No payload loses ground. Off restores the count-only rule.
    pub enable_tabular_ratio: bool,

    /// Run reversible key factoring in the lossy modes too.
    ///
    /// The transform removes repeated key names and is reversible from its own
    /// envelope, so it is safe in any mode. Confining it to the lossless path —
    /// the obvious reading of "reversible transforms only" — leaves map-shaped
    /// JSON compressing at roughly nothing in `recoverable`, which is the mode
    /// most deployments actually run.
    pub enable_lossy_key_factoring: bool,

    /// Replace a file Read that a later edit in the same request invalidated.
    pub enable_stale_read: bool,

    /// Classify a payload by its body, with the harness frame held out.
    ///
    /// An agent harness wraps every tool result in a few lines of `Chunk ID:` /
    /// `Wall time:` / `Process exited with code N` / `Output:`. Those lines are
    /// not part of what the payload *is*, but detection counts them, and every
    /// routing rule is a ratio over lines — so on a short payload the frame
    /// alone can push a real grep dump below its threshold and send it to plain
    /// text. That reasoning is sound and the change still **loses**, which is
    /// why it ships off.
    ///
    /// Measured over 1,336 eligible payloads of real agent traffic: it moves 34
    /// payloads to a different type, almost all of them text to tabular, and
    /// corpus coverage falls from 57.27% to 56.97%. Reclassifying is not the
    /// same as compressing better — the frame-free type is more *accurate*, and
    /// the compressor it selects does a worse job than the mixed segmenter that
    /// had the payload before. Left here, off, because it is a real effect worth
    /// re-testing on traffic whose payloads are shorter, where the frame is a
    /// larger share of every ratio.
    pub enable_preamble_aware_detection: bool,

    /// Lift a `cat -n` / `grep -n` line-number column off a payload before
    /// classifying it, and number the surviving lines again afterwards.
    ///
    /// The column is four to seven characters wide and it defeats the whole
    /// detector chain, because every rule in that chain is anchored at the start
    /// of a line. See [`crate::gutter`].
    pub enable_line_gutter: bool,

    /// Fold spans a *previous* message in the same conversation already showed.
    ///
    /// The one redundancy no per-payload compressor can see. Measured over
    /// 316,526 tool outputs from 3,283 terminal-bench trials: 16.4% of all
    /// bytes in lines of 40 characters or more had already been shown by an
    /// earlier output in the same trial. See [`crate::crossturn`].
    pub enable_cross_turn_dedup: bool,

    /// Clip lines longer than this, as a last resort, when nothing else could
    /// compress the payload at all.
    ///
    /// Every compressor here selects or folds *lines*, so a payload whose
    /// content lives inside one enormous line defeats all of them at once. See
    /// [`crate::longline`]. `0` disables it.
    pub max_line_chars: usize,

    /// Fall back to duplicate folding when the chosen compressor declines a
    /// payload that is mostly repeated lines.
    ///
    /// Without it, a payload some compressor claimed and then could not help is
    /// returned whole even when folding would have taken almost all of it. On a
    /// corpus of real agent traffic that was 117,608 characters across 14
    /// payloads — warning spew shaped like YAML, repeated compiler invocations
    /// shaped like source.
    pub enable_fold_fallback: bool,

    /// Hold lossless mode to its promise.
    ///
    /// A mode check belongs in every compressor, not most of them. With this
    /// on, each compressor without a reversible encoding declines in lossless
    /// mode; with it off the search, diff, tabular and code paths run there
    /// anyway and drop grep matches, diff context, table rows and function
    /// bodies from the one mode that exists to drop nothing.
    ///
    /// It is a switch rather than a constant only because the recorded vectors
    /// in `fixtures/golden.json.gz` were captured without those checks, and the
    /// vector test has to be able to reproduce them.
    pub strict_lossless: bool,

    pub search_max_files: usize,
    pub diff_max_files: usize,
    pub code_max_body_lines: usize,

    /// Give a source file a second pass when the first found nothing to elide.
    ///
    /// `code_max_body_lines` is a floor, and a file whose bodies all sit under
    /// it returns whole. That is not a rare shape: of the eleven corpus
    /// payloads the code compressor declines outright, five are Python files
    /// whose longest body is 24 lines or fewer — small functions, nothing over
    /// the bar, nothing removed.
    ///
    /// Lowering the floor for every file would be the wrong fix: it would elide
    /// short bodies in files that already compress 80-96%, trading a marker for
    /// a handful of lines. So the collection runs a second time, with a much
    /// lower floor, and only when the first pass produced no candidate at all.
    /// A file that already compresses is untouched by this.
    ///
    /// The idea is headroom's: allocate against what the file has rather than
    /// against a fixed threshold. Its version scores symbols by reference
    /// count, fan-out, visibility *and how well the name matches the user's
    /// prompt* — that last signal cannot be used here, because output that
    /// depends on the surrounding conversation breaks the prompt cache and the
    /// `query_dependent` invariant that guards it.
    pub enable_code_budget: bool,

    /// Below this a body is a signature and a line, and eliding it trades a
    /// marker for nothing. Only consulted by the second pass.
    pub code_budget_min_body_lines: usize,

    /// Floor for treating an embedded JSON value in free text as its own span,
    /// so a small object mentioned in a sentence is left alone. String *values*
    /// are not gated by this: anything the crusher is about to clip is offered
    /// to the router first, since clipping is the destructive option.
    pub min_nested_chars: usize,

    /// How many times structure-inside-string may be unwrapped.
    pub max_nested_depth: usize,

    /// Source payloads larger than this decline rather than being parsed.
    pub max_code_bytes: usize,

    /// Roles whose messages are never compressed.
    ///
    /// The assistant's own analysis and plan turns are the agent's working
    /// memory: clipping them makes it re-investigate and re-plan every turn,
    /// which costs far more than compression saves. System and developer turns
    /// carry the instructions. `user` stays compressible on purpose, because
    /// agent harnesses deliver terminal and tool output as user messages.
    pub protected_roles: Vec<String>,

    pub token_guard: TokenGuard,
}

impl Default for EngineOptions {
    fn default() -> Self {
        EngineOptions {
            mode: Mode::Recoverable,
            min_compress_chars: 512,
            max_array_items: 10,
            max_field_chars: 256,
            keep_tail_lines: 20,
            ref_ttl_seconds: 3600,
            ref_max_lifetime_multiplier: 8,
            max_ref_bytes: 1024 * 1024,
            min_elision_saving_chars: 0,
            enable_smart_crusher: true,
            enable_search_compressor: true,
            enable_log_compressor: true,
            enable_diff_compressor: true,
            enable_tabular_compressor: true,
            enable_listing_compressor: true,
            enable_code_aware: true,
            enable_mixed_content: true,
            enable_config_compressor: true,
            enable_config_sections: true,
            enable_nested_routing: true,
            enable_redraw_fold: true,
            enable_gutter_aware_detection: true,
            enable_build_templates: true,
            fold_handles_identical_repeats: false,
            fold_marks_variants: true,
            enable_html_compressor: true,
            enable_normalised_text_fold: true,
            enable_tabular_ratio: true,
            enable_lossy_key_factoring: true,
            enable_stale_read: true,
            enable_preamble_aware_detection: false,
            enable_line_gutter: true,
            enable_cross_turn_dedup: true,
            max_line_chars: crate::longline::DEFAULT_MAX_LINE_CHARS,
            enable_fold_fallback: true,
            strict_lossless: true,
            search_max_files: 8,
            diff_max_files: 8,
            code_max_body_lines: 24,
            enable_code_budget: true,
            code_budget_min_body_lines: 6,
            min_nested_chars: 512,
            max_nested_depth: 2,
            max_code_bytes: 2 * 1024 * 1024,
            protected_roles: vec![
                "assistant".to_string(),
                "system".to_string(),
                "developer".to_string(),
            ],
            token_guard: TokenGuard::Chars,
        }
    }
}

impl EngineOptions {
    /// Whether `role` is withheld from compression.
    pub fn is_protected_role(&self, role: &str) -> bool {
        let role = role.trim().to_ascii_lowercase();
        self.protected_roles
            .iter()
            .any(|r| r.trim().eq_ignore_ascii_case(&role))
    }

    /// A collector for this mode: recoverable retains fragments, the other two
    /// modes get a disabled collector so every compressor emits plain markers
    /// without any of them having to know the mode.
    pub fn collector(&self) -> crate::refs::RefCollector {
        match self.mode {
            Mode::Recoverable => crate::refs::RefCollector::new(self.max_ref_bytes),
            _ => crate::refs::RefCollector::disabled(),
        }
    }

    pub fn is_lossless(&self) -> bool {
        self.mode == Mode::Lossless
    }

    /// Whether a compressor with no reversible encoding must decline.
    ///
    /// Every lossy compressor asks this rather than testing the mode directly,
    /// so the one place that decides what `lossless` means is here.
    pub fn declines_as_lossless(&self) -> bool {
        self.mode == Mode::Lossless && self.strict_lossless
    }

    /// Overlay a partial set of overrides.
    pub fn merged(&self, patch: &EngineOptionsPatch) -> EngineOptions {
        let mut out = self.clone();
        patch.apply(&mut out);
        out
    }

    /// Reject values that would make a compressor behave nonsensically.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_array_items == 0 {
            return Err("max_array_items must be at least 1".into());
        }
        if self.max_field_chars < 16 {
            return Err("max_field_chars must be at least 16".into());
        }
        if self.search_max_files == 0 || self.diff_max_files == 0 {
            return Err("search_max_files and diff_max_files must be at least 1".into());
        }
        if self.code_max_body_lines == 0 {
            return Err("code_max_body_lines must be at least 1".into());
        }
        if self.max_nested_depth > 16 {
            return Err("max_nested_depth must be 16 or less".into());
        }
        if self.ref_ttl_seconds == 0 {
            return Err("ref_ttl_seconds must be positive".into());
        }
        Ok(())
    }
}

/// The all-optional mirror of [`EngineOptions`], as sent per request.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineOptionsPatch {
    pub mode: Option<Mode>,
    pub min_compress_chars: Option<usize>,
    pub max_array_items: Option<usize>,
    pub max_field_chars: Option<usize>,
    pub keep_tail_lines: Option<usize>,
    pub ref_ttl_seconds: Option<u64>,
    pub max_ref_bytes: Option<usize>,
    pub min_elision_saving_chars: Option<usize>,
    pub enable_smart_crusher: Option<bool>,
    pub enable_search_compressor: Option<bool>,
    pub enable_log_compressor: Option<bool>,
    pub enable_diff_compressor: Option<bool>,
    pub enable_tabular_compressor: Option<bool>,
    pub enable_listing_compressor: Option<bool>,
    pub enable_code_aware: Option<bool>,
    pub enable_mixed_content: Option<bool>,
    pub enable_config_compressor: Option<bool>,
    pub enable_config_sections: Option<bool>,
    pub enable_nested_routing: Option<bool>,
    pub enable_redraw_fold: Option<bool>,
    pub enable_gutter_aware_detection: Option<bool>,
    pub enable_build_templates: Option<bool>,
    pub fold_handles_identical_repeats: Option<bool>,
    pub fold_marks_variants: Option<bool>,
    pub enable_html_compressor: Option<bool>,
    pub enable_normalised_text_fold: Option<bool>,
    pub enable_tabular_ratio: Option<bool>,
    pub enable_lossy_key_factoring: Option<bool>,
    pub enable_stale_read: Option<bool>,
    pub enable_preamble_aware_detection: Option<bool>,
    pub enable_line_gutter: Option<bool>,
    pub max_line_chars: Option<usize>,
    pub enable_fold_fallback: Option<bool>,
    pub strict_lossless: Option<bool>,
    pub search_max_files: Option<usize>,
    pub diff_max_files: Option<usize>,
    pub code_max_body_lines: Option<usize>,
    pub enable_code_budget: Option<bool>,
    pub code_budget_min_body_lines: Option<usize>,
    pub min_nested_chars: Option<usize>,
    pub max_nested_depth: Option<usize>,
    pub token_guard: Option<TokenGuard>,
    /// Comma-separated; an empty string disables the role gate for the request.
    pub protected_roles: Option<String>,
}

impl EngineOptionsPatch {
    fn apply(&self, out: &mut EngineOptions) {
        macro_rules! set {
            ($($field:ident),* $(,)?) => { $(if let Some(v) = self.$field { out.$field = v; })* };
        }
        set!(
            mode,
            min_compress_chars,
            max_array_items,
            max_field_chars,
            keep_tail_lines,
            ref_ttl_seconds,
            max_ref_bytes,
            min_elision_saving_chars,
            enable_smart_crusher,
            enable_search_compressor,
            enable_log_compressor,
            enable_diff_compressor,
            enable_tabular_compressor,
            enable_listing_compressor,
            enable_code_aware,
            enable_mixed_content,
            enable_config_compressor,
            enable_config_sections,
            enable_nested_routing,
            enable_redraw_fold,
            enable_gutter_aware_detection,
            enable_build_templates,
            fold_handles_identical_repeats,
            fold_marks_variants,
            enable_html_compressor,
            enable_normalised_text_fold,
            enable_tabular_ratio,
            enable_lossy_key_factoring,
            enable_stale_read,
            enable_preamble_aware_detection,
            enable_fold_fallback,
            strict_lossless,
            search_max_files,
            diff_max_files,
            code_max_body_lines,
            enable_code_budget,
            code_budget_min_body_lines,
            min_nested_chars,
            max_nested_depth,
            token_guard,
        );
        if let Some(roles) = &self.protected_roles {
            out.protected_roles = roles
                .split(',')
                .map(|r| r.trim().to_ascii_lowercase())
                .filter(|r| !r.is_empty())
                .collect();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_mode_is_recoverable() {
        assert_eq!(EngineOptions::default().mode, Mode::Recoverable);
    }

    #[test]
    fn the_model_and_the_instructions_are_protected_by_default() {
        let opts = EngineOptions::default();
        assert!(opts.is_protected_role("assistant"));
        assert!(opts.is_protected_role("System"));
        assert!(opts.is_protected_role("developer"));
        assert!(
            !opts.is_protected_role("user"),
            "harnesses deliver tool output as user"
        );
        assert!(!opts.is_protected_role("tool"));
    }

    #[test]
    fn an_empty_role_override_disables_the_gate() {
        let patch = EngineOptionsPatch {
            protected_roles: Some(String::new()),
            ..Default::default()
        };
        let opts = EngineOptions::default().merged(&patch);
        assert!(opts.protected_roles.is_empty());
        assert!(!opts.is_protected_role("assistant"));
    }

    #[test]
    fn an_override_can_extend_protection() {
        let patch = EngineOptionsPatch {
            protected_roles: Some("assistant, tool".into()),
            ..Default::default()
        };
        let opts = EngineOptions::default().merged(&patch);
        assert!(opts.is_protected_role("tool"));
        assert!(!opts.is_protected_role("system"));
    }

    #[test]
    fn merging_leaves_unset_fields_alone() {
        let base = EngineOptions::default();
        let patch = EngineOptionsPatch {
            max_array_items: Some(40),
            ..Default::default()
        };
        let merged = base.merged(&patch);
        assert_eq!(merged.max_array_items, 40);
        assert_eq!(merged.max_field_chars, base.max_field_chars);
        assert_eq!(merged.mode, base.mode);
    }

    #[test]
    fn a_recoverable_collector_retains_and_the_others_do_not() {
        let mut opts = EngineOptions::default();
        assert!(opts.collector().is_enabled());
        opts.mode = Mode::Aggressive;
        assert!(!opts.collector().is_enabled());
        opts.mode = Mode::Lossless;
        assert!(!opts.collector().is_enabled());
    }

    #[test]
    fn validation_rejects_nonsense() {
        let mut opts = EngineOptions::default();
        assert!(opts.validate().is_ok());
        opts.max_field_chars = 4;
        assert!(opts.validate().is_err());
    }
}

#[cfg(test)]
mod measured_defaults {
    use super::*;

    /// Two knobs ship off because measurement said so, not because they are
    /// unfinished. Pinning the defaults keeps a later reader from "fixing" them
    /// on the reasoning alone, which is what makes them tempting.
    #[test]
    fn the_knobs_that_measured_worse_are_off() {
        let opts = EngineOptions::default();
        assert!(
            !opts.enable_preamble_aware_detection,
            "classifying without the harness frame is more accurate and compressed \
             0.30pp worse on a real corpus"
        );
        assert_eq!(
            opts.token_guard,
            TokenGuard::Chars,
            "the token guard is opt-in until a corpus A/B moves it"
        );
    }

    /// And the ones that measured better are on.
    #[test]
    fn the_knobs_that_measured_better_are_on() {
        let opts = EngineOptions::default();
        assert!(opts.enable_fold_fallback, "worth +0.92pp coverage");
        assert!(
            opts.enable_line_gutter,
            "worth +0.61pp coverage, but only arbitrated"
        );
        assert!(opts.enable_lossy_key_factoring);
        assert!(opts.strict_lossless);
    }
}
