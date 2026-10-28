//! Routing, arbitration, and the guards every compression has to pass.
//!
//! The compressors are independent and none of them can opt out of the
//! invariants, because the invariants are enforced here instead of in each of
//! them. Two decisions live at this level and nowhere else:
//!
//! * **which reading of a payload wins.** A heterogeneous body has more than one
//!   defensible interpretation and they are not ordered by quality, so the
//!   candidates are computed and compared rather than ranked in advance.
//! * **whether a shrink is allowed to ship.** Output that is shorter has to say
//!   what it removed. A rewrite that is neither marked nor provably
//!   layout-preserving is refused and the original returned.

use std::borrow::Cow;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::code::crush_code;
use crate::configs::crush_config;
use crate::crusher::crush_json;
use crate::detect::{code_starts_at, detect_result, file_joins, strategy_for};
use crate::diff::crush_diff;
use crate::gutter::split_line_gutter;
use crate::listing::crush_listing;
use crate::logs::{Identity, build_identity, crush_log};
use crate::markers::{has_envelope, has_marker, redraw_marker, whitespace_only};
use crate::mixed::{crush_mixed, has_mixed_content};
use crate::options::{EngineOptions, TokenGuard};
use crate::preamble::split_harness_preamble;
use crate::redraw::collapse_redraws;
use crate::refs::{Mark, RefCollector};
use crate::search::crush_search;
use crate::tabular::crush_tabular;
use crate::text::chars_len;
use crate::tokens::estimate_tokens;
use crate::types::{ContentType, Context, ForcedType, Kind, Mode, Strategy};

/// A layout-only rewrite below this share of the payload is not worth shipping:
/// it changes bytes the provider prompt cache is holding, and records a
/// compression that removed no information. Measured against a real agent
/// envelope, reserialization there won 0.27%.
const MIN_REFORMAT_GAIN: f32 = 0.02;

/// Terminal redraws below this many characters cannot pay for the marker that
/// accounts for them, and trying costs a second pass over the whole payload. Set
/// from the marker's own width plus slack: of 303 corpus candidates the split was
/// 182 wins to 121 losses without it, and the losses were all marker-sized.
const MIN_REDRAW_CHARS: usize = 128;

/// Too few lines for repetition to mean anything.
const MIN_REPEATED_LINES: usize = 5;
/// Share of distinct lines below which duplicate folding is the right answer,
/// whatever the payload was classified as. Same value the listing detector uses
/// to refuse a block of repeated lines.
const MAX_DISTINCT_FOR_FOLD: f32 = 0.6;

/// Below this share of the payload removed, the segmenter's claim on it is not
/// believed.
///
/// Chosen off a plateau rather than a knee: 15%, 20% and 30% cost identical
/// needle retention and land within 2% of each other on savings, so almost no
/// payload has the segmenter saving anywhere in that band and the exact value
/// does not matter. 40% starts costing retention for very little; 10% gives up a
/// third of the win.
const SEGMENTER_MIN_SAVING: f32 = 0.20;

/// Compress one message body.
///
/// Returns the new content and the type that claimed it; `None` for the type
/// means the body was left untouched, and in that case the returned content is
/// the input byte for byte.
///
/// `applied` collects the constituent types actually compressed, so a caller can
/// unfold one heterogeneous message into the real types it contained instead of
/// reporting the opaque `mixed`.
pub fn compress_content<'a>(
    content: &'a str,
    role: &str,
    opts: &EngineOptions,
    forced: Option<ForcedType>,
    refs: &mut RefCollector,
    ctx: &Context,
    applied: &mut Vec<Kind>,
) -> (Cow<'a, str>, Option<Kind>) {
    let oversized_before = refs.oversized_attempts();
    let mark = refs.mark();
    let applied_before = applied.len();
    // A line-number column has to come off before anything else looks at the
    // payload, because it is what stops everything else from recognising it.
    // Skipped under a forced type — the caller has already said what this is —
    // and below the size gate, where the engine's answer is to leave it alone.
    let result = if opts.enable_line_gutter
        && forced.is_none()
        && chars_len(content) >= opts.min_compress_chars
    {
        match lift_gutter(content, role, opts, refs, ctx, applied) {
            Some((out, kind)) => (Cow::Owned(out), Some(kind)),
            None => compress_framed(content, role, opts, forced, refs, ctx, applied),
        }
    } else {
        compress_framed(content, role, opts, forced, refs, ctx, applied)
    };
    if refs.is_enabled() && refs.oversized_attempts() != oversized_before {
        refs.rollback(mark);
        applied.truncate(applied_before);
        return (Cow::Borrowed(content), None);
    }
    if refs.is_enabled()
        && result.1.is_some()
        && chars_len(content).saturating_sub(chars_len(&result.0)) < opts.min_elision_saving_chars
        && !crate::refs::refs_in(&result.0).is_empty()
    {
        // Worth less than the fetch it invites: see `min_elision_saving_chars`.
        refs.rollback(mark);
        refs.note_gated();
        applied.truncate(applied_before);
        return (Cow::Borrowed(content), None);
    }
    result
}

/// Compress a payload whose line-number column, if it had one, is already off.
fn compress_framed<'a>(
    content: &'a str,
    role: &str,
    opts: &EngineOptions,
    forced: Option<ForcedType>,
    refs: &mut RefCollector,
    ctx: &Context,
    applied: &mut Vec<Kind>,
) -> (Cow<'a, str>, Option<Kind>) {
    // The redraw fold is arbitrated, not applied. It costs one marker, which on
    // 121 of 303 corpus payloads was more than the frames it removed, and
    // resolving can also change routing — so both answers are computed and the
    // smaller wins, the same shape as the diagnostic-echo decision inside the
    // log compressor.
    //
    // It is skipped without a collector, because a marker whose handle has
    // nowhere to be stored is not a trade worth making; in lossless mode, which
    // keeps every byte it was given; under a forced type, which is the caller
    // saying it already knows what this is; and below the size gate, where the
    // engine's answer is "leave it alone" and that has to include this.
    if !refs.is_enabled()
        || !opts.enable_redraw_fold
        || forced.is_some()
        || opts.mode == Mode::Lossless
        || chars_len(content) < opts.min_compress_chars
    {
        return route(content, role, opts, forced, refs, ctx, applied);
    }

    let Some((collapsed, frames)) = collapse_redraws(content) else {
        return route(content, role, opts, forced, refs, ctx, applied);
    };
    // What the fold actually removes, not how long the lines carrying it were: a
    // 200-character line that resolves to 190 pays for no marker at all. This is
    // also the cheap exit for the common case of a stray carriage return.
    if chars_len(content).saturating_sub(chars_len(&collapsed)) < MIN_REDRAW_CHARS {
        return route(content, role, opts, forced, refs, ctx, applied);
    }

    let mark = refs.mark();
    let mut plain_types = Vec::new();
    let (plain, plain_kind) = route(content, role, opts, forced, refs, ctx, &mut plain_types);
    let plain = plain.into_owned();
    let plain_fragments: Vec<String> = refs.pairs()[mark.pairs..]
        .iter()
        .map(|(_, fragment)| fragment.clone())
        .collect();
    refs.rollback(mark);

    let mut folded_types = Vec::new();
    let (folded, folded_kind) = route(&collapsed, role, opts, forced, refs, ctx, &mut folded_types);
    let mut folded = folded.into_owned();
    let handle = refs.stash(&frames.join("\n"));
    if !folded.ends_with('\n') {
        folded.push('\n');
    }
    folded.push_str(&redraw_marker(frames.len(), handle));
    folded.push('\n');

    if chars_len(&folded) < chars_len(&plain) {
        applied.extend(folded_types);
        // The payload is whatever it was; the fold is an extra saving on it, not
        // a new kind of content, so the reported type stays as it was.
        let kind =
            folded_kind.unwrap_or_else(|| detect_result(content, role, opts).content_type.kind());
        return (Cow::Owned(folded), Some(kind));
    }

    refs.rollback(mark);
    refs.restash_all(&plain_fragments);
    applied.extend(plain_types);
    match plain_kind {
        Some(kind) => (Cow::Owned(plain), Some(kind)),
        None => (Cow::Borrowed(content), None),
    }
}

/// Lift a line-number column off `content`, compress what is underneath, and
/// number the surviving lines again.
///
/// **Arbitrated, not applied** — the same shape as the redraw fold above, and
/// for the same reason. Stripping the column is not free: it changes what the
/// payload is classified as, and on a payload that was already routing well
/// that is a change for the worse. `grep -n` output is the case that proves it.
/// The search compressor reads `file:line:match` natively and scores matches
/// against each other; lift the numbers off first and the same payload reads as
/// plain text. Applying the lift unconditionally cost 1.45pp of corpus coverage
/// while *raising* the claimed share — more payloads taken, each handled worse,
/// which is exactly the signature of a transform that is winning arguments it
/// should be losing.
///
/// So both readings are computed and the shorter ships. Only payloads that
/// actually carry a column pay for the second pass.
///
/// Returns `None` — having rolled back anything the inner pass retained —
/// whenever the detour did not earn itself: no column, nothing claimed the body,
/// the plain reading was at least as good, the numbers cost more than the
/// compression saved, or the result could not satisfy the accounting guard.
fn lift_gutter(
    content: &str,
    role: &str,
    opts: &EngineOptions,
    refs: &mut RefCollector,
    ctx: &Context,
    applied: &mut Vec<Kind>,
) -> Option<(String, Kind)> {
    // The harness frame sits *above* the numbering and is not part of it, so it
    // is held out here rather than confusing the column check below.
    let (preamble, body) = split_harness_preamble(content);
    let gutter = split_line_gutter(body)?;

    let mut inner_opts = opts.clone();
    inner_opts.enable_line_gutter = false;
    // The body is a file the agent read by line number, and the segmenter is
    // not for those (`mixed::has_mixed_content` refuses the numbered form for
    // the same reason). With the column off it no longer looks numbered, so
    // the refusal has to be carried in here: a README read this way had the
    // list inside it sampled down to 70 entries omitted.
    inner_opts.enable_mixed_content = false;
    // Two lines that are byte-identical *once the column is off* were not
    // identical in the payload — their numbers differed, which is the whole
    // point of a line number. A bare `[×N]` says "this exact line, N times",
    // and against the original that is false: measured on a 43-line ASCII
    // render whose rows read ` 358|`, ` 359|`, … the engine emitted one
    // unnumbered row and `[×5]`, and rows 359 to 362 were gone with nothing
    // pointing at them. So inside the lift every fold mints a handle, and the
    // occurrences stay fetchable instead of being asserted away.
    //
    // It is applied to every fold under the lift rather than only to the ones
    // that would lie, because under a lift they all do: the gutter check
    // requires the numbers to *increase*, so two lines that are identical once
    // the column is off were necessarily different with it on. Checked against
    // the shipped corpus, on the one payload this costs anything: 288 of its
    // 294 lines carry a number and not one long line repeats verbatim, so all
    // 26 of its folds were claiming repeats the payload does not contain.
    //
    // The price is recorded rather than absorbed: `html` 66.54% -> 65.69% over
    // 5 payloads and corpus coverage 70.0766% -> 70.0385%, which is 494
    // characters of handle on that single payload. `fixtures/baseline.json`
    // was re-recorded for it.
    inner_opts.fold_handles_identical_repeats = true;

    let mark = refs.mark();
    let mut inner_types = Vec::new();
    let (out, kind) = compress_framed(
        &gutter.body,
        role,
        &inner_opts,
        None,
        refs,
        ctx,
        &mut inner_types,
    );
    // Nothing claimed the stripped body, so the column was not what was in the
    // way and there is nothing to put back.
    let Some(kind) = kind else {
        refs.rollback(mark);
        return None;
    };

    let restored = format!("{preamble}{}", gutter.reattach(&out));

    // What the payload compresses to with its column left on. Computed second so
    // that a body nothing claimed costs nothing beyond the split above.
    let mut plain_scratch = refs.scratch();
    let mut plain_types = Vec::new();
    let (plain, plain_kind) = compress_framed(
        content,
        role,
        opts,
        None,
        &mut plain_scratch,
        ctx,
        &mut plain_types,
    );
    // Smaller wins, with one exception: a row sampler must not outbid the code
    // compressor on source.
    //
    // Both results are retrievable — a listing's omission marker carries a
    // handle just as an elided body does — so size looks like the only axis
    // and for most types it is. It is not for code. The lifted result keeps
    // every signature and elides the bodies; the column-on result keeps the
    // head and tail of the file and drops the middle. An agent reading source
    // needs the signatures, and the one it can act on is worth more than the
    // one that is 7 KB smaller.
    //
    // Measured on a 400-line Rust file numbered `cat -n` style: lifted and
    // compressed as code came to ~13 KB against 5,491 bytes as a listing, so
    // size chose the sampler every time and `Read` output — which is how
    // Claude Code delivers every file it opens — was routed to row-dropping.
    // This costs ratio, deliberately, and it is the kind of trade that needle
    // retention (§19) exists to arbitrate properly once it is built.
    // Keyed on what the *stripped body* is, not on the kinds either side
    // reports. Two earlier attempts keyed on `plain_kind` and both failed:
    // naming `Listing | Tabular | Mixed` missed the text fold, and the broader
    // rule missed that a row-sampled result still reports `SourceCode` when the
    // numbered copy was detected as source — the kind says what the payload
    // was, never how it was reduced. The body's own type says it plainly.
    let body_is_code = matches!(
        detect_result(&gutter.body, role, opts).content_type,
        ContentType::SourceCode
    );
    if let Some(plain_kind) = plain_kind
        .filter(|_| !body_is_code)
        .filter(|_| chars_len(&plain) <= chars_len(&restored))
    {
        let plain = plain.into_owned();
        refs.rollback(mark);
        refs.adopt(plain_scratch);
        applied.extend(plain_types);
        return Some((plain, plain_kind));
    }
    drop(plain);
    drop(plain_scratch);

    let mut types = Vec::new();
    match accounted(content, restored, kind, opts, &mut types) {
        // `accounted` enforces that a shrink is explained; this enforces that
        // there *is* one. Re-attachment adds the column back to every surviving
        // line, so a compressor that saved less than the column costs comes out
        // no smaller, and shipping it would rewrite cached bytes for nothing.
        Some((checked, kind)) if chars_len(&checked) < chars_len(content) => {
            applied.extend(inner_types);
            applied.extend(types);
            Some((checked, kind))
        }
        _ => {
            refs.rollback(mark);
            None
        }
    }
}

/// Route one body, arbitrating between the segmenter and whole-message routing.
///
/// A heterogeneous body has two readings and they are not ordered by quality.
/// The segmenter compresses the structured spans it recognises and leaves every
/// other byte alone; whole-message routing asks what the payload *is* and hands
/// all of it to one compressor. Which reading is better is a property of the
/// payload: a build log with one grep block in it is a log, and taking the grep
/// block out of it saves a few hundred bytes where the log compressor saves 97%.
///
/// Preferring the segmenter on any shrink at all — which is what this used to do
/// — cost 1,535,678 characters over the replay corpus, and lost on 682 of 960
/// mixed payloads. The worst single case was a 40,222-character log where the
/// segmenter saved 306 bytes and the log compressor would have left 1,385.
///
/// Asking instead for "whichever result is shorter" is the obvious repair and it
/// is also wrong. Shorter is what a *blunter* compressor produces: handed a log
/// with a grep block and one sentence of prose, the listing compressor reads the
/// whole payload as a single listing, keeps five entries and elides everything
/// between — the prose included. Length cannot separate the candidates, because
/// they differ in *what* they discard. So the question asked is about the
/// segmenter alone: did it earn the payload?
fn route<'a>(
    content: &'a str,
    role: &str,
    opts: &EngineOptions,
    forced: Option<ForcedType>,
    refs: &mut RefCollector,
    ctx: &Context,
    applied: &mut Vec<Kind>,
) -> (Cow<'a, str>, Option<Kind>) {
    if forced.is_none() && chars_len(content) < opts.min_compress_chars {
        return (Cow::Borrowed(content), None);
    }

    let mut mixed_candidate: Option<(String, Kind, Vec<Kind>, RefCollector)> = None;
    if forced.is_none() && opts.enable_mixed_content && has_mixed_content(content, opts) {
        let mut scratch = refs.scratch();
        if let Some((out, subtypes)) = guarded(|| crush_mixed(content, opts, ctx, &mut scratch))
            && chars_len(&out) < chars_len(content)
            // …and the same disqualification, stated once for both readings.
            && scratch.oversized_attempts() == 0
        {
            let mut types = Vec::new();
            if let Some((checked, kind)) = accounted(content, out, Kind::Mixed, opts, &mut types) {
                types.extend(subtypes);
                let earned = (chars_len(content) - chars_len(&checked)) as f32
                    / chars_len(content).max(1) as f32
                    >= SEGMENTER_MIN_SAVING;
                if earned {
                    // Settled, and the second reading is not computed at all,
                    // so a payload the segmenter handles well costs exactly
                    // what it did before. Only the ones it barely touched pay
                    // for two routes.
                    refs.adopt(scratch);
                    applied.extend(types);
                    return (Cow::Owned(checked), Some(kind));
                }
                mixed_candidate = Some((checked, kind, types, scratch));
            }
        }
    }

    let mut whole_scratch = refs.scratch();
    let mut whole_types = Vec::new();
    let whole = route_whole(
        content,
        role,
        opts,
        forced,
        &mut whole_scratch,
        ctx,
        &mut whole_types,
    );
    // A reading that could not retain what it elided is not a candidate.
    //
    // Its output is short — the elision happened, only the fragment behind it
    // was refused — so it wins the size comparison below, is adopted, and then
    // trips the caller's oversized guard, which discards *everything* including
    // the reading that was shippable. Measured: a 1.1 MB payload whose uniform
    // body exceeded `max_ref_bytes` returned byte-for-byte at 0.00%, throwing
    // away a segmented reading that had already removed 21 KB of it.
    //
    // This is the arbitration-level twin of the refusal count `Mark` restores
    // on rollback, and it exists for the same reason: a candidate that cannot
    // ship must not veto the one that can.
    let whole = whole.filter(|_| whole_scratch.oversized_attempts() == 0);
    // The disqualified reading is dropped, so carry across the refusal that
    // disqualified it; it is the only account of why this payload compressed
    // less than it could have.
    refs.absorb_oversized_refusals(&whole_scratch);

    match (mixed_candidate, whole) {
        (Some((mixed_out, mixed_kind, mixed_types, mixed_refs)), whole) => {
            let whole_is_better = whole
                .as_ref()
                .is_some_and(|(out, _)| chars_len(out) <= chars_len(&mixed_out));
            if whole_is_better {
                let (out, kind) = whole.expect("checked above");
                refs.adopt(whole_scratch);
                applied.extend(whole_types);
                (Cow::Owned(out), Some(kind))
            } else {
                refs.adopt(mixed_refs);
                applied.extend(mixed_types);
                (Cow::Owned(mixed_out), Some(mixed_kind))
            }
        }
        (None, Some((out, kind))) => {
            refs.adopt(whole_scratch);
            applied.extend(whole_types);
            (Cow::Owned(out), Some(kind))
        }
        // Nothing understood this payload. Before giving up, check whether it is
        // one of the ones that defeats every compressor by construction: content
        // inside a single enormous line, which a line-oriented compressor can
        // neither fold nor select from. See `crate::longline` for why this runs
        // here and nowhere earlier.
        (None, None) => match clip_overlong(content, opts, refs, applied) {
            Some((out, kind)) => (Cow::Owned(out), Some(kind)),
            None => (Cow::Borrowed(content), None),
        },
    }
}

/// The last-resort clip, guarded exactly like every other candidate.
fn clip_overlong(
    content: &str,
    opts: &EngineOptions,
    refs: &mut RefCollector,
    applied: &mut Vec<Kind>,
) -> Option<(String, Kind)> {
    let mark = refs.mark();
    // `clip_long_lines` mints a handle per clipped line *before* it can know
    // whether the finished result is shorter, so a refusal leaves fragments
    // behind. Rolling back on the `None` path is what keeps them from becoming
    // orphans — a stored fragment no marker points at, which is a write nothing
    // can ever read and an invariant violation on the replay corpus.
    let Some(out) = guarded(|| crate::longline::clip_long_lines(content, opts, refs)) else {
        refs.rollback(mark);
        return None;
    };
    let mut types = Vec::new();
    match accounted(content, out, Kind::Text, opts, &mut types) {
        Some((checked, kind)) if shrank(content, &checked, opts) => {
            applied.extend(types);
            Some((checked, kind))
        }
        _ => {
            refs.rollback(mark);
            None
        }
    }
}

/// Hand the whole body to the one compressor its detected type selects.
fn route_whole(
    content: &str,
    role: &str,
    opts: &EngineOptions,
    forced: Option<ForcedType>,
    refs: &mut RefCollector,
    ctx: &Context,
    applied: &mut Vec<Kind>,
) -> Option<(String, Kind)> {
    let mark = refs.mark();

    let (out, kind, folded_instead) = match forced {
        Some(ForcedType::Json) => (
            guarded(|| crush_json(content, opts, ctx, refs)),
            Kind::Json,
            false,
        ),
        Some(ForcedType::Log) => (
            guarded(|| crush_log(content, opts, ctx, refs, Identity::Values)),
            Kind::Log,
            false,
        ),
        // forced text keeps duplicate folding, without template normalisation:
        // a payload whose identity is its numbers must not have them replaced
        // by placeholders
        Some(ForcedType::Text) => (
            guarded(|| crush_log(content, opts, ctx, refs, Identity::Exact)),
            Kind::Text,
            false,
        ),
        Some(ForcedType::Config) => (
            guarded(|| crush_config(content, opts, ctx, refs)),
            Kind::Config,
            false,
        ),
        None => {
            let detection = detect_result(content, role, opts);
            let strategy = strategy_for(&detection, opts);
            // A uniform stream diverted to the scored path because it carries a
            // crash reports what ran, not what it was detected as. The type is
            // still `listing` and that is what detection records; a route that
            // reported `listing` here would be unmeasurable in analytics, and
            // the whole point of the diversion is being able to see how often
            // it fires.
            let kind = if detection.meta.error_signal && strategy == Strategy::Log {
                Kind::Log
            } else {
                detection.content_type.kind()
            };
            let out = match strategy {
                Strategy::SmartCrusher => guarded(|| crush_json(content, opts, ctx, refs)),
                Strategy::Log => {
                    // build output is the one log shape whose repeats differ in
                    // a name rather than a value
                    let identity = if detection.content_type == ContentType::BuildOutput {
                        build_identity(opts)
                    } else {
                        Identity::Values
                    };
                    guarded(|| crush_log(content, opts, ctx, refs, identity))
                }
                Strategy::Search => guarded(|| crush_search(content, opts, ctx, refs)),
                Strategy::Diff => guarded(|| crush_diff(content, opts, ctx, refs)),
                Strategy::Tabular => guarded(|| crush_tabular(content, opts, ctx, refs)),
                Strategy::Listing => guarded(|| crush_listing(content, opts, ctx, refs)),
                Strategy::Config => {
                    // Config is the one route that regularly claims a payload
                    // and then removes almost nothing. Its transforms elide
                    // *values* one key at a time, which is the right answer for
                    // a hand-written file and the wrong one for the generated
                    // kind an agent actually cats: 600 `[worker.pool_N]` blocks
                    // repeating five keys came back at 0.85%, and a 700-section
                    // INI at 0.00%, while the value-normalised log fold took
                    // 97.96% and 98.39% of the same two payloads.
                    //
                    // So both readings are computed and the shorter ships —
                    // the same arbitration the gutter lift and the redraw fold
                    // already use, and the loser's fragments are rolled back.
                    // The fold's occurrences keep a handle in recoverable mode,
                    // which is what makes it safe here: a config's values are
                    // the whole point of it, and folding them is only
                    // acceptable while they stay fetchable.
                    let claimed = guarded(|| crush_config(content, opts, ctx, refs));
                    arbitrate_config(content, claimed, opts, ctx, refs, mark)
                }
                Strategy::CodeAware => {
                    let language = detection.meta.language;
                    guarded(|| crush_code(content, opts, ctx, refs, language))
                        .or_else(|| {
                            refs.rollback(mark);
                            code_behind_a_lead_in(content, role, opts, ctx, refs)
                        })
                        .or_else(|| {
                            refs.rollback(mark);
                            code_across_files(content, role, opts, ctx, refs)
                        })
                }
                Strategy::Passthrough => {
                    // Duplicate folding still applies, but only to prose: every
                    // other type that reaches here has a reason for declining,
                    // and folding its lines would undo that reason.
                    if detection.content_type == ContentType::PlainText {
                        arbitrate_text(content, opts, ctx, refs, Identity::AdjacentValues)
                    } else {
                        None
                    }
                }
            };
            // A compressor that claimed a payload and then declined leaves it
            // with nothing, even when something obvious was available. On a
            // corpus of real agent traffic that cost 117,608 characters across
            // 14 payloads: warning spew that reads as YAML because every line is
            // `Name: message` (572 lines, 15 distinct) and repeated compiler
            // invocations that read as source. Each was claimed, declined, and
            // returned whole.
            //
            // So when the chosen compressor passes and the payload is mostly
            // repeated lines, fall back to duplicate folding. The guard is what
            // keeps this from undoing the reasons compressors decline: a listing
            // is distinct entries by construction, a real source file is mostly
            // distinct lines, and neither reaches this. What does reach it is
            // output that is the same handful of lines over and over, where
            // folding is the right answer whatever the payload looked like.
            //
            // Except an instruction stream. Its repeats are `pxor xmm0,xmm0`
            // and `ret`, which pass the guard, and folding them moves
            // instructions out of the order that is their meaning.
            let mut folded_instead = false;
            let out = match out {
                Some(out) => Some(out),
                None if opts.enable_fold_fallback
                    && !detection.meta.assembly
                    && mostly_repeated_lines(content) =>
                {
                    refs.rollback(mark);
                    // Both identities, shorter wins. `Exact` alone is the wrong
                    // reading for the shape that reaches here most often — a
                    // generated file repeating one key with a different value on
                    // every line. A 700-section INI came back at 32.96% folding
                    // only its byte-identical lines, where normalising the
                    // values takes 98.39%.
                    // Source keeps its lines in order and is never sampled: the
                    // wide reading runs the log scorer on fifty lines or more,
                    // and with code elision off a C file reached it and came
                    // back as `233 log lines omitted`, 79% of the file gone.
                    let normalised = if detection.content_type == ContentType::SourceCode {
                        Identity::AdjacentValues
                    } else {
                        Identity::Values
                    };
                    let folded = arbitrate_text(content, opts, ctx, refs, normalised);
                    folded_instead = folded.is_some();
                    folded
                }
                None => None,
            };
            (out, kind, folded_instead)
        }
    };

    let Some(out) = out else {
        refs.rollback(mark);
        return None;
    };
    if !shrank(content, &out, opts) {
        refs.rollback(mark);
        return None;
    }
    match accounted(content, out, kind, opts, applied) {
        Some(result) => {
            if folded_instead {
                // Record both: what the payload is, and what actually shrank it.
                // Reporting only the detected type would hide that its own
                // compressor declined, which is the signal that the detector is
                // over-claiming; reporting only the fold would hide what the
                // payload was.
                applied.push(kind);
                applied.push(Kind::Text);
            }
            Some(result)
        }
        None => {
            refs.rollback(mark);
            None
        }
    }
}

/// Whether a payload is mostly the same lines over and over.
///
/// The threshold mirrors the listing detector's own distinctness rule, and
/// deliberately so: that rule exists because a block of repeated lines is not a
/// listing whatever its shape, and duplicate folding serves it better than
/// sampling would. This is the same judgement applied one step later, to a
/// payload some other compressor claimed and then could not help.
/// Fold plain text, keeping whichever identity takes more.
///
/// Prose folds on byte-identical lines and nothing else — normalising its
/// numbers would merge sentences that differ only in one, and `Identity::Exact`
/// is the reading that has always been applied here. It is the wrong reading
/// for *generated* text, which is the other thing that arrives classified as
/// plain: a markdown guide of 80 near-identical fenced blocks came back at
/// 7.15% under `Exact` and 97.10% under `Values`.
///
/// So both are computed and the shorter ships. Exact runs first and wins ties,
/// which keeps prose exactly where it was — a payload whose lines repeat
/// verbatim folds the same under either identity, so the normalised reading can
/// only win where the lines actually differ, which is where it should.
///
/// `normalised` is the reading the second candidate folds by. Text that reached
/// the plain route, and source the fold fallback reaches, get
/// [`Identity::AdjacentValues`] — with [`Identity::AdjacentExact`] as the exact
/// reading — which keeps every line where it stood; any other payload already
/// shown to be mostly repeated lines gets [`Identity::Values`], since there a
/// template recurring across the whole payload is the shape being folded.
pub(crate) fn arbitrate_text(
    content: &str,
    opts: &EngineOptions,
    ctx: &Context,
    refs: &mut RefCollector,
    normalised: Identity,
) -> Option<String> {
    let mark = refs.mark();
    let exact_identity = if normalised == Identity::AdjacentValues {
        Identity::AdjacentExact
    } else {
        Identity::Exact
    };
    let exact = guarded(|| crush_log(content, opts, ctx, refs, exact_identity));
    let exact_fragments: Vec<String> = refs.pairs()[mark.pairs..]
        .iter()
        .map(|(_, fragment)| fragment.clone())
        .collect();
    let exact_len = exact.as_deref().map(chars_len);
    refs.rollback(mark);

    if !opts.enable_normalised_text_fold {
        refs.restash_all(&exact_fragments);
        return exact;
    }
    match guarded(|| crush_log(content, opts, ctx, refs, normalised)) {
        Some(values) if exact_len.is_none_or(|e| chars_len(&values) < e) => Some(values),
        _ => {
            refs.rollback(mark);
            refs.restash_all(&exact_fragments);
            exact
        }
    }
}

/// Keep whichever of the config compressor's output and a value-normalised
/// fold is smaller.
///
/// Only reached from the config route. `None` in, `None` out: a payload the
/// config compressor declined outright is already handled by the fold fallback
/// below, and adding a second path to the same place would make which one ran
/// depend on the order of two ifs.
pub(crate) fn arbitrate_config(
    content: &str,
    claimed: Option<String>,
    opts: &EngineOptions,
    ctx: &Context,
    refs: &mut RefCollector,
    mark: Mark,
) -> Option<String> {
    let claimed = claimed?;
    // Worth arbitrating only when the claim was thin. A config compressor that
    // already took a third of the payload has read it correctly, and folding it
    // as a log would trade that reading for bytes.
    if !opts.enable_fold_fallback
        || chars_len(&claimed) < (chars_len(content) as f32 * CONFIG_THIN) as usize
    {
        return Some(claimed);
    }
    let claimed_fragments: Vec<String> = refs.pairs()[mark.pairs..]
        .iter()
        .map(|(_, fragment)| fragment.clone())
        .collect();
    refs.rollback(mark);
    let folded = guarded(|| crush_log(content, opts, ctx, refs, Identity::Values));
    match folded {
        Some(folded) if chars_len(&folded) < chars_len(&claimed) => Some(folded),
        _ => {
            refs.rollback(mark);
            refs.restash_all(&claimed_fragments);
            Some(claimed)
        }
    }
}

/// How much a config compressor has to have removed for its reading to stand
/// unchallenged. Below this the payload is offered to the fold as well.
const CONFIG_THIN: f32 = 0.70;

pub(crate) fn mostly_repeated_lines(content: &str) -> bool {
    let lines: Vec<&str> = crate::text::split_lines(content)
        .into_iter()
        .filter(|line| !crate::text::is_blank(line))
        .collect();
    if lines.len() < MIN_REPEATED_LINES {
        return false;
    }
    let distinct: std::collections::HashSet<&&str> = lines.iter().collect();
    (distinct.len() as f32 / lines.len() as f32) < MAX_DISTINCT_FOR_FOLD
}

/// Whether `out` is genuinely smaller than `content`.
///
/// Characters by default, which is what the recorded corpus baseline is
/// measured in. Under [`TokenGuard::Tokens`] the
/// estimated token count has to fall too: roughly a tenth of compressed payloads
/// lose characters without losing tokens, because compression strips cheap
/// letters and spaces while keeping expensive digits and separators, so the
/// remainder is denser than what it replaced.
fn shrank(content: &str, out: &str, opts: &EngineOptions) -> bool {
    if chars_len(out) >= chars_len(content) {
        return false;
    }
    match opts.token_guard {
        TokenGuard::Chars => true,
        TokenGuard::Tokens => estimate_tokens(out, "") < estimate_tokens(content, ""),
    }
}

/// Gate every shrink on being accounted for.
///
/// Returns the compression when it carries a marker or declares a reversible
/// envelope, or when nothing but layout changed and that saved enough to be
/// worth the rewrite — declaring `reformat` in that case, since a marker-less
/// shrink is otherwise indistinguishable from data loss by inspection. Anything
/// else is refused.
///
/// This closes a real hole: a JSON round-trip can rewrite numeric literals, so a
/// segmenter could compact a pretty-printed block by 44% while silently changing
/// values, with no marker to show it.
fn accounted(
    content: &str,
    out: String,
    kind: Kind,
    opts: &EngineOptions,
    applied: &mut Vec<Kind>,
) -> Option<(String, Kind)> {
    if opts.mode == Mode::Lossless || has_marker(&out) {
        return Some((out, kind));
    }
    if has_envelope(&out) {
        // A columnar envelope removes repeated key names and is reversible from
        // its own contents, so it needs no omission marker — but it is not
        // "layout only" either, and without this it would be refused.
        applied.push(Kind::Factored);
        return Some((out, kind));
    }
    if !whitespace_only(content, &out) {
        // Something changed that is not layout and was never signposted.
        return None;
    }
    if chars_len(content) - chars_len(&out)
        < (chars_len(content) as f32 * MIN_REFORMAT_GAIN) as usize
    {
        // Rewriting bytes for a rounding error's worth of layout costs
        // prompt-cache stability and records a compression that removed nothing.
        return None;
    }
    applied.push(Kind::Reformat);
    Some((out, kind))
}

/// Run a compressor, treating a panic as a decline.
///
/// Compression is best-effort: it may refuse a payload, but it must never take
/// the request down with it. A parse tree is as deep as the source nests and a
/// *wrong* grammar nests it much further, so an unexpected panic in a compressor
/// is a real possibility on adversarial input — and the caller's contract is
/// that the original content survives it.
/// Source code behind a lead-in the grammar cannot parse.
///
/// The code compressor refuses a parse with errors in it, which is right for a
/// file and wrong for `ls && cat interp.py`: the file is fine, the listing in
/// front of it is the error. Set the lead-in aside verbatim, compress what
/// follows as the code it is, and put the two back together — the lead-in's
/// bytes are untouched, so nothing about it needs accounting.
fn code_behind_a_lead_in(
    content: &str,
    role: &str,
    opts: &EngineOptions,
    ctx: &Context,
    refs: &mut RefCollector,
) -> Option<String> {
    let (lead_in, body) = content.split_at(code_starts_at(content)?);
    let found = detect_result(body, role, opts);
    if found.content_type != ContentType::SourceCode {
        return None;
    }
    let body = guarded(|| crush_code(body, opts, ctx, refs, found.meta.language))?;
    Some(format!("{lead_in}{body}"))
}

/// Several source files concatenated by `cat`, compressed one file at a time.
///
/// Each file goes to the grammar of its own language, behind any listing in
/// front of it. A file that does not compress is kept byte for byte and its
/// fragments are rolled back, so the result is the concatenation it arrived
/// as with only the files that understood themselves rewritten. `None` when
/// there is one file, or when none of them compressed.
fn code_across_files(
    content: &str,
    role: &str,
    opts: &EngineOptions,
    ctx: &Context,
    refs: &mut RefCollector,
) -> Option<String> {
    let joins = file_joins(content);
    if joins.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(content.len());
    let mut compressed = false;
    let mut start = 0;
    for end in joins.into_iter().chain(std::iter::once(content.len())) {
        let file = &content[start..end];
        start = end;
        let mark = refs.mark();
        let found = detect_result(file, role, opts);
        let shrunk = (found.content_type == ContentType::SourceCode)
            .then(|| guarded(|| crush_code(file, opts, ctx, refs, found.meta.language)))
            .flatten()
            .or_else(|| {
                refs.rollback(mark);
                code_behind_a_lead_in(file, role, opts, ctx, refs)
            })
            .filter(|shrunk| chars_len(shrunk) < chars_len(file));
        match shrunk {
            Some(shrunk) => {
                out.push_str(&shrunk);
                compressed = true;
            }
            None => {
                refs.rollback(mark);
                out.push_str(file);
            }
        }
    }
    compressed.then_some(out)
}

fn guarded<T>(f: impl FnOnce() -> Option<T>) -> Option<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!("compressor panicked; payload left uncompressed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run<'a>(
        content: &'a str,
        opts: &EngineOptions,
    ) -> (Cow<'a, str>, Option<Kind>, RefCollector, Vec<Kind>) {
        let mut refs = opts.collector();
        let mut applied = Vec::new();
        let ctx = Context::for_role("tool");
        let (out, kind) =
            compress_content(content, "tool", opts, None, &mut refs, &ctx, &mut applied);
        (out, kind, refs, applied)
    }

    /// Three templates folded independently, sized so that exactly one of the
    /// three fragments is over a 20,000-byte `max_ref_bytes`.
    ///
    /// The lines within a group differ, so the fold cannot be reversed from a
    /// bare `[\u{d7}N]` count and each group needs a handle of its own — which is
    /// what makes one group refusable while the others are not.
    fn three_fold_groups() -> String {
        let mut lines: Vec<String> = Vec::new();
        for i in 0..900 {
            lines.push(format!(
                "worker alpha processed batch {i} in {}ms queue={i}",
                10 + i % 7
            ));
        }
        for i in 0..90 {
            lines.push(format!(
                "cache bravo evicted key k{i} age={i}s size={}",
                100 + i
            ));
        }
        for i in 0..90 {
            lines.push(format!(
                "sync delta pushed shard {i} rev={} bytes={}",
                1000 + i,
                i * 13
            ));
        }
        lines.join("\n")
    }

    fn log_payload(lines: usize) -> String {
        (0..lines)
            .map(|i| {
                format!(
                    "2026-01-02T03:04:{:02}Z INFO worker heartbeat seq={i}",
                    i % 60
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_payload_below_the_size_gate_is_left_alone() {
        let opts = EngineOptions::default();
        let (out, kind, refs, _) = run("short output", &opts);
        assert_eq!(out, "short output");
        assert_eq!(kind, None);
        assert!(refs.is_empty());
    }

    #[test]
    fn a_declined_payload_comes_back_byte_identical() {
        let opts = EngineOptions::default();
        let prose = "Some ordinary prose that no compressor claims. ".repeat(30);
        let (out, kind, _, _) = run(&prose, &opts);
        if kind.is_none() {
            assert_eq!(out, prose);
        }
    }

    #[test]
    fn compression_is_never_longer_than_its_input() {
        let opts = EngineOptions::default();
        for payload in [log_payload(200), "x".repeat(2000), log_payload(4)] {
            let (out, _, _, _) = run(&payload, &opts);
            assert!(out.chars().count() <= payload.chars().count());
        }
    }

    #[test]
    fn compression_is_deterministic() {
        let opts = EngineOptions::default();
        let payload = log_payload(200);
        let (first, first_kind, first_refs, _) = run(&payload, &opts);
        let (second, second_kind, second_refs, _) = run(&payload, &opts);
        assert_eq!(first, second);
        assert_eq!(first_kind, second_kind);
        assert_eq!(first_refs.len(), second_refs.len());
    }

    #[test]
    fn every_retained_fragment_is_pointed_at_by_the_output() {
        let opts = EngineOptions::default();
        let payload = log_payload(300);
        let (out, _, refs, _) = run(&payload, &opts);
        for (handle, fragment) in refs.pairs() {
            assert!(!fragment.is_empty());
            assert!(
                out.contains(&format!("ref:{handle}")),
                "handle {handle} is stored but unreachable"
            );
        }
    }

    #[test]
    fn aggressive_mode_mints_no_handles() {
        let opts = EngineOptions {
            mode: Mode::Aggressive,
            ..Default::default()
        };
        let payload = log_payload(300);
        let (out, kind, refs, _) = run(&payload, &opts);
        assert!(refs.is_empty(), "no collector means no retained fragments");
        if kind.is_some() {
            assert!(!out.contains("ref:k_"), "no handle without a fragment");
        }
    }

    #[test]
    fn a_marker_less_shrink_that_changed_content_is_refused() {
        // whitespace_only is the discriminator; a changed value is not layout
        assert!(!whitespace_only("{\"a\": 1e5}", "{\"a\":100000.0}"));
        assert!(whitespace_only("{\n  \"a\": 1\n}", "{\"a\":1}"));
    }

    #[test]
    fn a_trivial_reformat_is_not_worth_the_cache_churn() {
        let mut applied = Vec::new();
        let opts = EngineOptions {
            mode: Mode::Aggressive,
            ..Default::default()
        };
        // one character of whitespace out of a hundred is under the 2% floor
        let content = format!("{}\n", "a b".repeat(50));
        let out = content.replace('\n', "");
        assert!(accounted(&content, out, Kind::Text, &opts, &mut applied).is_none());
        assert!(applied.is_empty());
    }

    #[test]
    fn a_real_reformat_is_kept_and_declared() {
        let mut applied = Vec::new();
        let opts = EngineOptions {
            mode: Mode::Aggressive,
            ..Default::default()
        };
        let content = "{\n    \"alpha\": 1,\n    \"beta\": 2,\n    \"gamma\": 3\n}";
        let out = "{\"alpha\":1,\"beta\":2,\"gamma\":3}".to_string();
        let result = accounted(content, out, Kind::Json, &opts, &mut applied);
        assert!(result.is_some());
        assert_eq!(applied, vec![Kind::Reformat]);
    }

    #[test]
    fn a_columnar_envelope_is_accepted_as_a_declared_transform() {
        let mut applied = Vec::new();
        let opts = EngineOptions {
            mode: Mode::Aggressive,
            ..Default::default()
        };
        let content = r#"[{"a":1,"b":2},{"a":3,"b":4},{"a":5,"b":6}]"#;
        let out = r#"{"__cols__":["a","b"],"__rows__":[[1,2],[3,4],[5,6]]}"#.to_string();
        assert!(accounted(content, out, Kind::Json, &opts, &mut applied).is_some());
        assert_eq!(applied, vec![Kind::Factored]);
    }

    #[test]
    fn lossless_mode_is_exempt_from_the_marker_requirement() {
        let mut applied = Vec::new();
        let opts = EngineOptions {
            mode: Mode::Lossless,
            ..Default::default()
        };
        let out = "anything at all".to_string();
        assert!(accounted("a longer input here", out, Kind::Json, &opts, &mut applied).is_some());
    }

    #[test]
    fn a_panicking_compressor_declines_rather_than_propagating() {
        let result: Option<u8> = guarded(|| panic!("grammar exploded"));
        assert_eq!(result, None);
    }

    #[test]
    fn the_token_guard_refuses_a_shrink_that_costs_no_tokens() {
        let chars = EngineOptions {
            token_guard: TokenGuard::Chars,
            ..Default::default()
        };
        let tokens = EngineOptions {
            token_guard: TokenGuard::Tokens,
            ..Default::default()
        };
        // stripping cheap letters and spaces while keeping the expensive id
        let before = "INFO starting the service now with identifier 9f3a7c2b8e1d4a6f";
        let after = "9f3a7c2b8e1d4a6f";
        assert!(shrank(before, after, &chars));
        assert!(
            shrank(before, after, &tokens),
            "this one does fall in both measures"
        );
        // The case the guard exists for: twenty cheap letters replaced by eight
        // expensive digits is 60% shorter and costs exactly as many tokens, so
        // the character measure reports a saving that was never real.
        let dense_before = "a".repeat(20);
        let dense_after = "12345678";
        assert_eq!(
            estimate_tokens(&dense_before, ""),
            estimate_tokens(dense_after, "")
        );
        assert!(shrank(&dense_before, dense_after, &chars));
        assert!(!shrank(&dense_before, dense_after, &tokens));
    }

    #[test]
    fn config_arbitration_discards_fragments_from_the_losing_candidate() {
        let content = log_payload(300);
        let opts = EngineOptions::default();
        let mut refs = opts.collector();
        let mark = refs.mark();
        let claimed_id = refs.stash("fragment from the config candidate").unwrap();
        let claimed = content[..content.len() - 1].to_string();
        let output = arbitrate_config(
            &content,
            Some(claimed),
            &opts,
            &Context::for_role("tool"),
            &mut refs,
            mark,
        )
        .expect("the fold wins");
        assert!(output.len() < content.len() / 2);
        assert!(!refs.pairs().iter().any(|(id, _)| *id == claimed_id));
        for (id, _) in refs.pairs() {
            assert!(output.contains(&format!("ref:{id}")));
        }
    }

    /// One region whose elision cannot be retained must not discard the others.
    ///
    /// The whole-message reading of such a payload is *short* — the elision
    /// happened, only the fragment behind it was refused — so it used to win
    /// the size arbitration, get adopted, and then trip the oversized guard,
    /// which returns the payload untouched. Everything another reading had
    /// legitimately compressed went with it. Measured on a 1.1 MB log: 0.00%,
    /// discarding 21 KB the segmenter had already removed.
    ///
    /// `max_ref_bytes` is set small here so the same arithmetic runs on a
    /// payload a test can hold.
    #[test]
    fn a_region_that_cannot_retain_its_elision_does_not_discard_the_others() {
        let opts = EngineOptions {
            max_ref_bytes: 3_000,
            ..EngineOptions::default()
        };
        // Two fenced blocks. The first elides far more than the cap allows and
        // so can never ship; the second folds well within it.
        let bulky = (0..200)
            .map(|i| format!("epoch 1 step {i}/200 loss=1.8520 lr=0.000300 tok/s=5000{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let foldable = (0..40)
            .map(|i| {
                format!(
                    "src/mod.c:{}:5: warning: unused variable `t{i}` [-Wall]",
                    10 + i
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let content = format!(
            "Training run:\n\n```\n{bulky}\n```\n\nBuild warnings:\n\n```\n{foldable}\n```\n"
        );

        let (out, kind, refs, _) = run(&content, &opts);

        assert!(
            kind.is_some(),
            "the retainable region must still ship; got the payload back untouched"
        );
        assert!(
            chars_len(&out) < chars_len(&content),
            "{} -> {}, nothing was removed",
            chars_len(&content),
            chars_len(&out)
        );
        // The unretainable region keeps its bytes rather than carrying a marker
        // nothing can honour.
        assert!(
            out.contains("epoch 1 step 100/200"),
            "a line from the region that could not be retained must survive verbatim"
        );
        // Every handle that did ship is honoured by the output.
        for (id, _) in refs.pairs() {
            assert!(
                out.contains(&id.to_string()),
                "stashed fragment {id} is named by no marker"
            );
        }
    }
    /// Within one reading: a fold group over the cap must not discard the
    /// groups under it.
    ///
    /// `fold_suffix` already declines a group whose fragment cannot be
    /// retained, keeping every occurrence — nothing unfetchable ships. But the
    /// refusal was still counted, and the pipeline guard vetoes on the count,
    /// so a payload whose other groups folded perfectly came back whole.
    /// Measured with three groups and one over the cap: 0.00% against the
    /// 13.18% the two retainable groups are worth.
    #[test]
    fn a_fold_group_over_the_cap_does_not_discard_the_groups_under_it() {
        let opts = EngineOptions {
            max_ref_bytes: 20_000,
            ..EngineOptions::default()
        };
        let content = three_fold_groups();
        // Forced, because this payload's *detected* type is `listing` and the
        // sampler mints one fragment rather than one per template. The defect
        // is in how several fragments from a single reading interact, so the
        // test drives the reading that produces several.
        let mut refs = opts.collector();
        let mut applied = Vec::new();
        let ctx = Context::for_role("tool");
        let (out, kind) = compress_content(
            &content,
            "tool",
            &opts,
            Some(ForcedType::Log),
            &mut refs,
            &ctx,
            &mut applied,
        );

        assert!(kind.is_some(), "the retainable groups must still fold");
        assert!(
            chars_len(&out) < chars_len(&content),
            "{} -> {}, nothing was removed",
            chars_len(&content),
            chars_len(&out)
        );
        // The group whose fragment was refused keeps every occurrence: a bare
        // `[×N]` there would assert a repeat of lines that differ.
        assert_eq!(
            out.lines()
                .filter(|l| l.starts_with("worker alpha"))
                .count(),
            900,
            "every occurrence of the refused group must survive"
        );
        // …while the two that fit are folded behind handles.
        assert!(
            out.lines()
                .any(|l| l.contains("cache bravo") && l.contains("[\u{d7}")),
            "the bravo group must fold, got:\n{out}"
        );
        assert!(
            out.lines()
                .any(|l| l.contains("sync delta") && l.contains("[\u{d7}")),
            "the delta group must fold, got:\n{out}"
        );
        assert_eq!(
            refs.pairs().len(),
            2,
            "exactly the two retainable fragments"
        );
        for (id, _) in refs.pairs() {
            assert!(
                out.contains(&id.to_string()),
                "stashed fragment {id} is named by no marker"
            );
        }
    }
}

#[cfg(test)]
mod fold_fallback_tests {
    use super::*;

    fn opts() -> EngineOptions {
        EngineOptions {
            mode: Mode::Aggressive,
            ..Default::default()
        }
    }

    fn run(content: &str, opts: &EngineOptions) -> (String, Option<Kind>, Vec<Kind>) {
        let mut refs = opts.collector();
        let mut applied = Vec::new();
        let ctx = Context::for_role("tool");
        let (out, kind) =
            compress_content(content, "tool", opts, None, &mut refs, &ctx, &mut applied);
        (out.into_owned(), kind, applied)
    }

    /// Warning spew where every line reads as a YAML mapping. The config
    /// classifier claims it, the config compressor finds no comments to elide,
    /// and without the fallback the payload comes back whole — 572 lines of 15
    /// distinct ones, at full price.
    fn warning_spew() -> String {
        let distinct = [
            "MassiveIntentClassification: Missing subsets {'da', 'nb'} for split test",
            "MassiveIntentClassification: Missing splits {'validation'}",
            "MassiveScenarioClassification: Missing subsets {'da', 'nb'} for split test",
            "MassiveScenarioClassification: Missing splits {'validation'}",
        ];
        let mut lines = Vec::new();
        for _ in 0..60 {
            lines.extend(distinct.iter().copied());
        }
        lines.join("\n")
    }

    #[test]
    fn a_claimed_then_declined_payload_of_repeated_lines_is_folded_after_all() {
        let content = warning_spew();
        let without = EngineOptions {
            enable_fold_fallback: false,
            ..opts()
        };
        let (untouched, kind, _) = run(&content, &without);
        assert_eq!(kind, None, "its own compressor declines it");
        assert_eq!(untouched, content);

        let (folded, kind, applied) = run(&content, &opts());
        assert!(kind.is_some(), "the fallback rescues it");
        assert!(
            folded.chars().count() < content.chars().count() / 4,
            "240 lines of four distinct ones should mostly go: {} -> {}",
            content.chars().count(),
            folded.chars().count()
        );
        assert!(folded.contains("[×"), "the fold announces its counts");
        assert!(
            applied.contains(&Kind::Text),
            "the transform that ran is recorded, not just the type detected"
        );
    }

    #[test]
    fn a_payload_of_distinct_lines_is_left_to_its_own_compressor() {
        // The guard is what keeps the fallback from undoing the reasons a
        // compressor declines. A listing is distinct entries by construction and
        // a real source file is mostly distinct lines; neither should reach it.
        let listing: String = (0..80)
            .map(|i| format!("/app/logs/2026-01-{:02}_service_{i}.log", (i % 28) + 1))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!mostly_repeated_lines(&listing));

        let header: String = std::iter::once("#ifndef GRAPHX_H".to_string())
            .chain((0..60).map(|i| format!("int function_{i}(int a);")))
            .chain(std::iter::repeat_n("#endif".to_string(), 8))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !mostly_repeated_lines(&header),
            "a header with repeated #endif is still mostly distinct, and its \
             guards are structure rather than redundancy"
        );
    }

    #[test]
    fn the_repetition_guard_needs_enough_lines_to_mean_anything() {
        assert!(!mostly_repeated_lines("a\na\na"));
        assert!(mostly_repeated_lines("a\na\na\na\na\na"));
        assert!(!mostly_repeated_lines(""));
    }

    #[test]
    fn the_fallback_still_obeys_never_larger_and_determinism() {
        let content = warning_spew();
        let (first, kind, _) = run(&content, &opts());
        let (second, kind2, _) = run(&content, &opts());
        assert_eq!(first, second);
        assert_eq!(kind, kind2);
        assert!(first.chars().count() < content.chars().count());
    }
}

#[cfg(all(test, feature = "code-aware"))]
mod lead_in_tests {
    use super::*;

    fn compress(content: &str, opts: &EngineOptions) -> (String, RefCollector) {
        let mut refs = opts.collector();
        let mut applied = Vec::new();
        let ctx = Context::for_role("tool");
        let (out, _) = compress_content(content, "tool", opts, None, &mut refs, &ctx, &mut applied);
        (out.into_owned(), refs)
    }

    fn recoverable(enable_code_aware: bool) -> EngineOptions {
        EngineOptions {
            mode: Mode::Recoverable,
            enable_code_aware,
            ..Default::default()
        }
    }

    /// `objdump -d` as it prints by default: address, raw bytes, instruction.
    /// `per_function` matters: the segmenter samples only a long run of rows,
    /// and a function of a few dozen instructions is what it used to take.
    fn objdump(functions: usize, per_function: usize) -> String {
        let insns = [
            ("f3 0f 1e fa", "endbr64"),
            ("48 83 ec 78", "sub    rsp,0x78"),
            ("f3 0f 10 54 24 60", "movss  xmm2,DWORD PTR [rsp+0x60]"),
            ("0f 28 c2", "movaps xmm0,xmm2"),
            ("f3 0f 59 c2", "mulss  xmm0,xmm2"),
            ("66 0f ef db", "pxor   xmm3,xmm3"),
            ("0f 2f d9", "comiss xmm3,xmm1"),
            ("e8 4b 00 00 00", "call   402020 <vector_dot>"),
            ("48 83 c4 78", "add    rsp,0x78"),
            ("c3", "ret"),
        ];
        let mut out = vec![
            "vec:     file format elf64-x86-64".to_string(),
            String::new(),
            "Disassembly of section .text:".to_string(),
        ];
        let mut addr = 0x401090;
        for f in 0..functions {
            out.push(String::new());
            out.push(format!("{addr:016x} <fn_{f}>:"));
            for (bytes, insn) in insns.iter().cycle().take(per_function) {
                out.push(format!("  {addr:x}:\t{bytes:<21}\t{insn}"));
                addr += 4;
            }
        }
        out.join("\n")
    }

    /// The fixed run's remaining failure: disassembly the detector named
    /// correctly, sampled anyway by the segmenter, whose dump-row rule reads
    /// `401090:` as an offset. Four fetches of one function in one task.
    #[test]
    fn a_disassembly_with_addresses_ships_whole_through_every_route() {
        let dump = objdump(2, 120);
        for enable_code_aware in [true, false] {
            let opts = recoverable(enable_code_aware);
            let (out, refs) = compress(&dump, &opts);
            assert_eq!(out, dump, "whole, code-aware {enable_code_aware}");
            assert!(refs.is_empty());

            // numbered, the way `cat -n dis.txt` or Claude Code's Read shows it
            let numbered: String = dump
                .lines()
                .enumerate()
                .map(|(n, line)| format!("{:6}\t{line}\n", n + 294))
                .collect();
            let (out, _) = compress(&numbered, &opts);
            assert_eq!(out, numbered, "numbered, code-aware {enable_code_aware}");
        }
    }

    #[test]
    fn a_disassembly_inside_compound_output_keeps_every_instruction() {
        // `file x; objdump -d x; ls /usr/lib` in one call: the listing may be
        // sampled, the instructions may not.
        let names: String = (0..400)
            .map(|i| format!("libmodule_{i:03}.so.1\n"))
            .collect();
        let dump = objdump(1, 90);
        let content = format!(
            "Checking the binary first, then its neighbours.\n\n{dump}\n\nThe libraries next to it:\n{names}"
        );
        let (out, _) = compress(&content, &recoverable(false));
        for line in dump.lines().filter(|l| l.contains(":\t")) {
            assert!(out.contains(line), "instruction dropped: {line:?}");
        }
        assert!(
            out.len() < content.len(),
            "the listing beside it still compresses"
        );
    }

    #[test]
    fn a_numbered_file_read_is_never_row_sampled() {
        // Claude Code's Read of a README: the agent asked for these lines by
        // number, and the segmenter used to drop 1,831 of them.
        // Prose of varied shape, which the whole-message route reads as text
        // and keeps in place; the list inside it is the section the segmenter
        // sampled.
        let prose = [
            "This directory contains a number of test programs demonstrating various features.",
            "Each program prints its result, and the expected output sits beside it.",
            "Run them with the interpreter; a mismatch is reported as FAIL with a diff.",
            "Tail calls must not grow the stack.",
            "Some programs read from standard input, so pipe the matching .in file to them.",
            "Errors are expected in a few cases and are part of the output that is compared.",
            "Numbers print in their shortest exact form.",
            "Strings print without quotes unless written with write rather than display.",
        ];
        let mut body = String::from("# Scheme Interpreter Test Suite\n\n");
        for i in 0..96 {
            body.push_str(prose[i % prose.len()]);
            body.push_str(if i % 4 == 3 { "\n\n" } else { "\n" });
        }
        body.push_str("The programs:\n");
        for i in 0..80 {
            body.push_str(&format!("test/{i:02}-case-{i}.scm\n"));
        }
        let numbered: String = body
            .lines()
            .enumerate()
            .map(|(n, line)| format!("{:6}\t{line}\n", n + 1))
            .collect();
        let (out, _) = compress(&numbered, &recoverable(false));
        assert!(
            !out.contains("entries omitted"),
            "rows were sampled out of a file read"
        );
        // What may still happen is the in-place fold: a run of consecutive
        // lines differing only in a number keeps its first line and a handle,
        // where it stood. Every line of prose stays.
        for line in body
            .lines()
            .filter(|l| l.ends_with('.') || l.starts_with('#'))
        {
            assert!(
                out.contains(line),
                "prose dropped from a file read: {line:?}"
            );
        }
    }

    #[test]
    fn listing_sampling_off_reaches_the_segmenter_too() {
        let names: String = (0..400)
            .map(|i| format!("src/module_{i:03}/mod.rs\n"))
            .collect();
        let compound =
            format!("Here is what the repository holds:\n{names}\nand that is all of it.\n");
        let whole = names.clone();
        let on = recoverable(false);
        let off = EngineOptions {
            enable_listing_compressor: false,
            ..recoverable(false)
        };
        for (name, content) in [("compound", &compound), ("whole", &whole)] {
            let (sampled, _) = compress(content, &on);
            assert!(
                sampled.contains("entries omitted"),
                "{name}: the fixture must sample when on"
            );
            let (kept, refs) = compress(content, &off);
            assert!(
                !kept.contains("entries omitted"),
                "{name}: sampled with the switch off"
            );
            assert!(refs.is_empty() || !kept.contains("ref:k_"), "{name}");
        }
    }

    fn names(n: usize) -> String {
        (0..n)
            .map(|i| format!("src/module_{i:03}/mod.rs\n"))
            .collect()
    }

    fn gated(min_elision_saving_chars: usize) -> EngineOptions {
        EngineOptions {
            min_elision_saving_chars,
            ..recoverable(false)
        }
    }

    #[test]
    fn an_elision_worth_less_than_the_gate_ships_whole() {
        let small = names(60);
        let (sampled, refs) = compress(&small, &gated(0));
        let saved = small.len() - sampled.len();
        assert!(
            sampled.contains("ref:k_") && !refs.is_empty() && saved < 4000,
            "the fixture must elide with a handle and save under the gate: saved {saved}"
        );
        let (kept, refs) = compress(&small, &gated(4000));
        assert_eq!(kept, small, "shipped whole");
        assert!(refs.is_empty(), "and retained nothing to fetch");
    }

    #[test]
    fn an_elision_worth_more_than_the_gate_still_happens() {
        let large = names(400);
        let (sampled, refs) = compress(&large, &gated(4000));
        assert!(large.len() - sampled.len() >= 4000);
        assert!(sampled.contains("entries omitted") && !refs.is_empty());
        assert_eq!(
            sampled,
            compress(&large, &gated(0)).0,
            "and is unchanged by it"
        );
    }

    #[test]
    fn the_gate_leaves_compression_without_a_handle_alone() {
        // Nothing can be fetched through a marker without a handle, so it
        // invites no round and a small saving from one is still a saving.
        let lossless = EngineOptions {
            mode: Mode::Lossless,
            ..gated(4000)
        };
        let small = names(60);
        let (out, _) = compress(&small, &lossless);
        let (ungated, _) = compress(
            &small,
            &EngineOptions {
                mode: Mode::Lossless,
                ..gated(0)
            },
        );
        assert_eq!(out, ungated);
        let repeated = "connection refused\n".repeat(60);
        let (folded, refs) = compress(&repeated, &gated(4000));
        assert!(
            folded.len() < repeated.len() && !folded.contains("ref:k_") && refs.is_empty(),
            "the fixture must fold without a handle: {folded}"
        );
        assert_eq!(folded, compress(&repeated, &gated(0)).0);
    }

    /// The 89-task run's failure: a disassembly reached the plain-text fold,
    /// came back as `movss xmm10,DWORD PTR [rsp+0x90] [×46 similar …]`, and the
    /// agent paged the listing by hand instead. It ships whole now, with code
    /// elision on or off.
    #[test]
    fn a_disassembly_ships_whole() {
        let registers = [
            "xmm0", "xmm1", "xmm2", "xmm3", "xmm4", "xmm5", "xmm6", "xmm7",
        ];
        let mut lines = vec!["00000000004021a0 <sphere_intersect>:".to_string()];
        for i in 0..120 {
            let reg = registers[i % registers.len()];
            lines.push(match i % 5 {
                0 => format!("movss  {reg},DWORD PTR [rsp+0x{:x}]", 0x60 + 4 * i),
                1 => format!("mulss  {reg},xmm{}", (i + 3) % 8),
                2 => format!("addss  xmm{},{reg}", (i + 5) % 8),
                3 => "pxor   xmm3,xmm3".to_string(),
                _ => format!("movaps xmm{},{reg}", (i + 1) % 8),
            });
        }
        lines.push("ret".to_string());
        let content = lines.join("\n");
        for enable_code_aware in [true, false] {
            let opts = EngineOptions {
                mode: Mode::Recoverable,
                enable_code_aware,
                ..Default::default()
            };
            let mut refs = opts.collector();
            let mut applied = Vec::new();
            let ctx = Context::for_role("tool");
            let (out, _) =
                compress_content(&content, "tool", &opts, None, &mut refs, &ctx, &mut applied);
            assert_eq!(out, content, "code-aware {enable_code_aware}");
            assert!(refs.is_empty());
        }
    }

    /// `ls && cat interp.py`: the listing used to make the grammar fail on the
    /// first line and the whole payload ship at 0%.
    #[test]
    fn source_behind_a_listing_is_compressed_and_the_listing_kept_verbatim() {
        let lead_in = "interp.py\ntest\n01-factorial.scm\n02-fibonacci.scm\nREADME.md\n";
        let mut source = String::from("#!/usr/bin/env python3\nimport sys\n\n");
        for f in 0..12 {
            source.push_str(&format!("def handler_{f}(env, args):\n"));
            for line in 0..30 {
                source.push_str(&format!(
                    "    value_{line} = env.lookup(args[{line}]) + {f}\n"
                ));
            }
            source.push_str("    return value_0\n\n");
        }
        let content = format!("{lead_in}{source}");
        let opts = EngineOptions {
            mode: Mode::Recoverable,
            ..Default::default()
        };
        let mut refs = opts.collector();
        let mut applied = Vec::new();
        let ctx = Context::for_role("tool");
        let (out, kind) =
            compress_content(&content, "tool", &opts, None, &mut refs, &ctx, &mut applied);
        assert_eq!(kind, Some(Kind::SourceCode));
        assert!(out.starts_with(lead_in), "the listing is untouched");
        assert!(
            out.len() < content.len() / 2,
            "the bodies go: {}",
            out.len()
        );
        assert!(
            out.contains("def handler_11(env, args):"),
            "every signature stays"
        );
        assert!(!refs.is_empty(), "and what left is retrievable");
    }

    /// Turning code-aware compression off turns it off on every path that
    /// reaches the code compressor, not only whole-payload routing.
    #[test]
    fn code_elision_off_means_off_behind_a_listing_and_across_files() {
        let mut source = String::from("#!/usr/bin/env python3\nimport sys\n\n");
        for f in 0..6 {
            source.push_str(&format!("def handler_{f}(env, args):\n"));
            for line in 0..30 {
                source.push_str(&format!(
                    "    value_{line} = env.lookup(args[{line}]) + {f}\n"
                ));
            }
            source.push_str("    return value_0\n\n");
        }
        let content = format!("interp.py\nREADME.md\n{source}");
        let opts = EngineOptions {
            mode: Mode::Recoverable,
            enable_code_aware: false,
            ..Default::default()
        };
        let mut refs = opts.collector();
        let mut applied = Vec::new();
        let ctx = Context::for_role("tool");
        let (out, _) =
            compress_content(&content, "tool", &opts, None, &mut refs, &ctx, &mut applied);
        assert!(
            !out.contains("body lines omitted"),
            "no body is elided: {}",
            &out[..out.len().min(300)]
        );
    }

    /// `cat baseline.py ext.c wrapper.py`: each file goes to its own grammar,
    /// and a file that cannot be compressed is kept byte for byte.
    #[test]
    fn concatenated_files_are_compressed_one_grammar_at_a_time() {
        let body = |name: &str| -> String {
            let mut out = format!("def {name}(weights, cov):\n");
            for line in 0..30 {
                out.push_str(&format!(
                    "    total_{line} = {name}_weights[{line}] * cov[{line}][{line}]\n"
                ));
            }
            out.push_str("    return total_0\n\n");
            out
        };
        let baseline = format!(
            "import math\n\n{}{}print(risk_a([1], [[1]]))",
            body("risk_a"),
            body("risk_b")
        );
        let ext = "/*\n * extension\n */\n#include <math.h>\nstatic double half(double w) {\n    return w / 2;\n}\n";
        let wrapper = format!(
            "\"\"\"wrapper\"\"\"\n\n{}{}",
            body("call_a"),
            body("call_b")
        );
        let content = format!("{baseline}{ext}{wrapper}");
        let opts = EngineOptions {
            mode: Mode::Recoverable,
            ..Default::default()
        };
        let mut refs = opts.collector();
        let mut applied = Vec::new();
        let ctx = Context::for_role("tool");
        let (out, kind) =
            compress_content(&content, "tool", &opts, None, &mut refs, &ctx, &mut applied);
        assert_eq!(kind, Some(Kind::SourceCode));
        assert!(
            out.contains(ext),
            "the file that did not compress is untouched"
        );
        for name in ["def risk_a(", "def risk_b(", "def call_a(", "def call_b("] {
            assert!(out.contains(name), "every signature stays: {name}");
        }
        assert!(
            out.len() < content.len() / 2,
            "the Python bodies go: {}",
            out.len()
        );
        assert_eq!(refs.len(), 4, "one retrievable fragment per elided body");
    }
}

#[cfg(test)]
mod longline_tests {
    use super::*;

    fn opts() -> EngineOptions {
        EngineOptions {
            mode: Mode::Recoverable,
            ..Default::default()
        }
    }

    fn run(content: &str, opts: &EngineOptions) -> (String, Option<Kind>, usize) {
        let mut refs = opts.collector();
        let mut applied = Vec::new();
        let ctx = Context::for_role("tool");
        let (out, kind) =
            compress_content(content, "tool", opts, None, &mut refs, &ctx, &mut applied);
        let retained = refs.pairs().len();
        (out.into_owned(), kind, retained)
    }

    #[test]
    fn a_payload_that_is_one_enormous_line_is_clipped_at_last() {
        // Nothing else can touch this: there is one line, so there is nothing to
        // fold, group or select from.
        let content = format!("Chunk ID: a\nOutput:\n{}\n", "x".repeat(20_000));
        let (out, kind, _) = run(&content, &opts());
        assert!(out.len() < content.len() / 4, "the bulk has to go");
        assert_eq!(kind, Some(Kind::Text));
        assert!(out.contains("ref:k_"), "and it is retrievable");
    }

    /// The bug this guard exists for: `clip_long_lines` mints a handle per
    /// clipped line before it can know the finished result is shorter, so a
    /// refusal used to leave fragments behind that no marker referenced. On the
    /// replay corpus that was 14 `orphaned_ref` / `refs_without_marker`
    /// violations.
    #[test]
    fn a_refused_clip_leaves_no_orphaned_fragment() {
        // A limit of 1 makes every line "over the limit", and the markers cost
        // far more than the content, so the clip is refused after minting.
        let tiny = EngineOptions {
            mode: Mode::Recoverable,
            max_line_chars: 1,
            ..Default::default()
        };
        let content = format!("{}\n", "ab\n".repeat(400));
        let (out, kind, retained) = run(&content, &tiny);
        if kind.is_none() {
            assert_eq!(out, content, "a decline returns the input byte for byte");
            assert_eq!(
                retained, 0,
                "and retains nothing: a stored fragment no marker points at is a \
                 write nothing can ever read"
            );
        }
    }

    #[test]
    fn the_clip_never_takes_a_payload_a_compressor_understood() {
        // A JSON array with one long field: the crusher claims it, so the
        // last-resort clip must never see it. If it did, the output would carry
        // a line clip instead of the crusher's field clip.
        let items: Vec<String> = (0..40)
            .map(|i| format!(r#"{{"id":{i},"note":"{}"}}"#, "n".repeat(50)))
            .collect();
        let content = format!("[{}]", items.join(","));
        let (_, kind, _) = run(&content, &opts());
        assert_eq!(kind, Some(Kind::Json), "the crusher keeps its payload");
    }
}

#[cfg(test)]
mod gutter_tests {
    use super::*;

    fn opts() -> EngineOptions {
        EngineOptions {
            mode: Mode::Aggressive,
            ..Default::default()
        }
    }

    fn run(content: &str, opts: &EngineOptions) -> (String, Option<Kind>) {
        let mut refs = opts.collector();
        let mut applied = Vec::new();
        let ctx = Context::for_role("tool");
        let (out, kind) =
            compress_content(content, "tool", opts, None, &mut refs, &ctx, &mut applied);
        (out.into_owned(), kind)
    }

    #[test]
    fn a_fold_under_the_lift_never_claims_a_bare_repeat() {
        // Rows of an ASCII render, numbered ` 358|`, ` 359|`, … Every row is a
        // distinct line; they are byte-identical only once the column comes
        // off. A bare `[×N]` says "this exact line, N times", which against the
        // payload is false — measured on a real 43-row payload the engine kept
        // one unnumbered row and `[×5]`, and rows 359 to 362 were gone with
        // nothing pointing at them. Under the lift every fold mints a handle,
        // so the occurrences stay fetchable.
        let mut content = String::new();
        for i in 0..43 {
            let row = if i % 8 == 0 { "#....." } else { "......" };
            content.push_str(&format!(" {}|{}{}\n", 358 + i, row, ".".repeat(150)));
        }
        let opts = EngineOptions {
            mode: Mode::Recoverable,
            ..Default::default()
        };
        let (out, _) = run(&content, &opts);
        for line in out.lines() {
            let Some(at) = line.rfind(" [×") else {
                continue;
            };
            assert!(
                line[at..].contains("ref:"),
                "a fold over a stripped column must carry a handle, got {:?}",
                line[at..].to_string()
            );
        }
    }

    /// A numbered C source file behind a harness frame — the shape an agent's
    /// file-reading tool actually produces, and the shape this exists for.
    ///
    /// Both halves matter. Uniform generated lines read as a two-column *table*
    /// (number, text) and get claimed by the tabular compressor with the column
    /// still on, which would make this pass without the lift doing anything; the
    /// frame breaks that column, and with it the payload classifies `text` and
    /// routes to passthrough, exactly as the 85 real payloads in the corpus did.
    fn numbered_source() -> String {
        let shapes = [
            "  ctx.total += measure_{n}(&ctx, {i});",
            "  if (parse_flags(argv[{i}]) < 0) {\n    report(&ctx);\n    return {i};\n  }",
            "  registry_add(\"{n}\", handler_{n});",
            "  strncpy(ctx.name, \"{n}\", sizeof ctx.name - 1);",
            "  for (int k = 0; k < {i}; ++k) {\n    ctx.total += table_{n}[k];\n  }",
            "",
        ];
        let names = [
            "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india",
            "juliet",
        ];

        let mut src = String::from(
            "#include <stdio.h>\n#include <string.h>\n#include <stdlib.h>\n\n\
             #include \"registry.h\"\n#include \"measure.h\"\n\n\
             struct context {\n  long total;\n  char name[64];\n  int flags;\n};\n\n\
             static int parse_flags(const char *s) {\n  int flags = 0;\n  while (*s) {\n\
             \x20   switch (*s++) {\n      case 'v': flags |= FLAG_VERBOSE; break;\n\
             \x20     case 'q': flags |= FLAG_QUIET; break;\n      default:\n\
             \x20       fprintf(stderr, \"unknown flag\\n\");\n        return -1;\n\
             \x20   }\n  }\n  return flags;\n}\n\n\
             static void report(struct context *ctx) {\n\
             \x20 printf(\"%s: %ld\\n\", ctx->name, ctx->total);\n}\n\n\
             int main(int argc, char **argv) {\n  struct context ctx;\n\
             \x20 memset(&ctx, 0, sizeof ctx);\n",
        );
        for i in 0..60 {
            let n = format!("{}{}", names[i % names.len()], i / names.len());
            src.push_str(
                &shapes[(i * 3) % shapes.len()]
                    .replace("{n}", &n)
                    .replace("{i}", &(i + 3).to_string()),
            );
            src.push('\n');
        }
        src.push_str("  report(&ctx);\n  return 0;\n}\n");

        let mut out = String::from(
            "Chunk ID: abc123\nWall time: 0.0000 seconds\nProcess exited with code 0\nOutput:\n",
        );
        for (i, line) in src.lines().enumerate() {
            out.push_str(&format!("{:6}\t{line}\n", i + 1));
        }
        out
    }

    // The code compressor is what reaches the body; without the grammars there
    // is nothing for the lifted column to hand it to.
    #[cfg(feature = "code-aware")]
    #[test]
    fn a_numbered_source_file_compresses_once_the_column_is_off() {
        let src = numbered_source();
        let (with_lift, kind) = run(&src, &opts());
        let (without_lift, _) = run(
            &src,
            &EngineOptions {
                enable_line_gutter: false,
                ..opts()
            },
        );
        assert!(
            with_lift.len() < without_lift.len(),
            "lifting the column should let the code compressor reach the body: \
             {} vs {}",
            with_lift.len(),
            without_lift.len()
        );
        assert!(kind.is_some());
    }

    #[test]
    fn a_line_number_column_does_not_change_what_survives() {
        // The property that matters, stated directly: numbering a file must not
        // change which of its lines come back. The lift is arbitrated on output
        // size, and for source that picks the wrong answer -- a numbered file is
        // two columns, so a row sampler claims it, keeps the head and tail and
        // drops the middle, while the lifted copy keeps every signature and
        // elides the bodies behind handles. Both are retrievable, so size cannot
        // tell them apart; an agent reading source can act on a file of
        // signatures and cannot act on its first and last twenty lines.
        //
        // Measured on 400 numbered lines of this crate's own `crusher.rs`:
        // 5,491 bytes as a row sample against 13,197 as code, so size chose the
        // sampler and `Read` output -- how Claude Code delivers every file it
        // opens -- was routed to row-dropping. Afterwards the numbered file
        // returns 30.0% against the bare file's 29.4%, with its original line
        // numbers intact.
        //
        // Asserted as a comparison rather than an absolute count because what
        // the code compressor keeps depends on the source; what it must not do
        // is keep *less* because someone numbered the lines.
        let mut plain = String::new();
        for i in 0..60 {
            let (j, k) = (i * 7 % 43, i * 13 % 97);
            plain.push_str(&format!(
                "/// Handle case {i}: reconcile ledger {i} against snapshot {j}.\n\
                 ///\n\
                 /// Entry {i} is settled when its weight exceeds {j} and the run at\n\
                 /// offset {k} has been acknowledged by peer {i}, which the caller\n\
                 /// establishes before it calls this. Case {j} is the exception.\n\
                 pub fn handler_{i}(payload: &Payload) -> usize {{\n\
                 \x20   let mut total = 0;\n\
                 \x20   for row in payload.rows.iter() {{\n\
                 \x20       total += row.weight * {i};\n\
                 \x20       if row.flagged {{\n\
                 \x20           total += row.penalty;\n\
                 \x20       }}\n\
                 \x20   }}\n\
                 \x20   total\n\
                 }}\n\n"
            ));
        }
        let numbered: String = crate::text::split_lines(&plain)
            .into_iter()
            .enumerate()
            .map(|(n, line)| format!("{:6}\t{line}\n", n + 1))
            .collect();

        let signatures = |text: &str| {
            crate::text::split_lines(text)
                .into_iter()
                .filter(|line| line.contains("fn handler_"))
                .count()
        };
        let (bare_out, _) = run(&plain, &opts());
        let (numbered_out, _) = run(&numbered, &opts());

        assert!(
            signatures(&bare_out) > 1,
            "the bare file has to keep signatures for this to compare anything: {}",
            signatures(&bare_out)
        );
        assert_eq!(
            signatures(&numbered_out),
            signatures(&bare_out),
            "numbering a file must not change what survives: {} signatures kept \
             numbered against {} bare",
            signatures(&numbered_out),
            signatures(&bare_out)
        );
    }

    #[test]
    fn the_surviving_lines_keep_their_original_numbers() {
        let src = numbered_source();
        let (out, _) = run(&src, &opts());

        // Only lines whose text occurs once can be checked, because a line that
        // repeats has no single right number and re-attachment is free to pick
        // either occurrence as long as it picks them in order.
        let mut seen: std::collections::HashMap<&str, Option<&str>> =
            std::collections::HashMap::new();
        for line in crate::text::split_lines(&src) {
            if let Some((number, rest)) = line.split_once('\t') {
                seen.entry(rest)
                    .and_modify(|e| *e = None)
                    .or_insert(Some(number));
            }
        }

        let mut checked = 0;
        for line in crate::text::split_lines(&out) {
            let Some((number, rest)) = line.split_once('\t') else {
                continue; // a marker, which is allowed to carry no number
            };
            if let Some(Some(expected)) = seen.get(rest) {
                assert_eq!(&number, expected, "line {rest:?} was renumbered");
                checked += 1;
            }
        }
        assert!(checked > 0, "nothing survived to check");
    }

    /// The case that forced arbitration: `grep -n` output is *better* with its
    /// numbers on, because the search compressor reads them.
    #[test]
    fn grep_output_is_not_taken_away_from_the_search_compressor() {
        let mut hits = String::new();
        for i in 0..80 {
            hits.push_str(&format!(
                "src/module_{}.rs:{}:    let handler = Handler::new(config_{i});\n",
                i % 7,
                i * 13 + 4
            ));
        }
        let (with_lift, _) = run(&hits, &opts());
        let (without_lift, _) = run(
            &hits,
            &EngineOptions {
                enable_line_gutter: false,
                ..opts()
            },
        );
        assert!(
            with_lift.len() <= without_lift.len(),
            "arbitration must never ship the worse of the two readings"
        );
    }

    #[test]
    fn the_lift_never_ships_a_result_that_is_not_smaller() {
        // A column on content nothing can compress: the numbers would be
        // re-attached for no gain, rewriting cached bytes for nothing.
        let mut content = String::new();
        for i in 0..40 {
            content.push_str(&format!("{i:5}\tunique prose line {i} about {i} things\n"));
        }
        let (out, _) = run(&content, &opts());
        assert!(out.len() <= content.len(), "invariant: never larger");
    }
}
