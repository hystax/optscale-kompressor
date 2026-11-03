//! Compressor for path and name listings — `ls`, `find`, glob and tree output.
//!
//! Agent terminal traffic is full of these: a directory of dated log files, a
//! `find` result, a package list. They used to be mis-read as build output,
//! because a filename can begin with a date (`2025-07-03_api.log`), and the log
//! compressor's template normalization then folded `…07-03_api.log` and
//! `…07-04_api.log` into one `<n>-<n>-<n>_api.log` line — deleting exactly the
//! information a listing carries. On one benchmark run that destroyed 20 of 25
//! distinct dates in a task about date ranges.
//!
//! A listing is sampled like an array instead: entries stay **byte-identical**,
//! never normalized, and both endpoints are kept so the range the listing spans
//! stays visible, with one marker counting what sits between them.

use crate::markers::omission_marker;
use crate::options::EngineOptions;
use crate::preamble::split_harness_preamble;
use crate::refs::RefCollector;
use crate::text::{chars_len, is_blank, join_lines, split_lines};
use crate::types::Context;

/// Below this many entries a listing is sent whole: the entries *are* the
/// payload, and hiding a handful of them is not worth the bytes.
const MIN_SAMPLED_ENTRIES: usize = 50;

/// Sample a listing down to its head, a count, and its tail.
pub fn crush_listing(
    content: &str,
    opts: &EngineOptions,
    ctx: &Context<'_>,
    refs: &mut RefCollector,
) -> Option<String> {
    // The switch is honoured here, not only where whole payloads are routed:
    // the segmenter samples listing sections inside compound output through
    // this same function, and with the check only in routing,
    // `enable_listing_compressor = false` still dropped rows from every
    // `ls && cat` and every `objdump` section it found. `crush_code` honours
    // `enable_code_aware` the same way, for the same reason.
    if !opts.enable_listing_compressor {
        return None;
    }
    // Neither the harness frame nor a blank separator is an *entry*, and both
    // were counted as one: a 47-entry dump behind a 5-line frame cleared the
    // 50-entry floor that exists to protect it, and its marker claimed 42
    // omitted when 39 had gone. Holding the frame out also stops correctness
    // depending on the head budget happening to exceed the frame's length.
    let (preamble, body) = split_harness_preamble(content);
    if !preamble.is_empty() {
        let crushed = crush_body(body, opts, ctx, refs)?;
        let out = format!("{preamble}{crushed}");
        return (chars_len(&out) < chars_len(content)).then_some(out);
    }
    crush_body(content, opts, ctx, refs)
}

fn crush_body(
    content: &str,
    opts: &EngineOptions,
    _ctx: &Context<'_>,
    refs: &mut RefCollector,
) -> Option<String> {
    let lines: Vec<&str> = split_lines(content)
        .into_iter()
        .filter(|line| !is_blank(line))
        .collect();
    let budget = 4.max(opts.max_array_items);
    // Dropping entries is the only lever here, and it is not reversible from the
    // output, so lossless has nothing to gain.
    if opts.is_lossless() || lines.len() <= budget {
        return None;
    }
    if lines.len() <= MIN_SAMPLED_ENTRIES {
        // Short enough to send whole. Sampling keeps the endpoints plus a count,
        // which is right for a 100-entry `find` dump and wrong for a listing the
        // agent is inspecting: one read back the `gates.txt` it was building and
        // got 2 of 31 definitions, and earlier runs hid 12 of 17 and 9 of 17
        // document filenames on tasks required to process every document. Across
        // the benchmark corpus (1.0M chars, 326K removed) all listing
        // compression amounted to 908 chars, so this floor costs ~0.3% of
        // savings and closes a failure mode that recurred three times.
        return None;
    }

    // Weight the head, but never lose the last entry: the first and last are
    // what reveal the span a listing covers.
    let head = 1.max(budget * 3 / 5);
    let tail = 1.max(budget - head);
    let omitted = &lines[head..lines.len() - tail];

    let mut out: Vec<String> = lines[..head].iter().map(|l| (*l).to_string()).collect();
    out.push(omission_marker(
        "entry",
        omitted.len(),
        &join_lines(omitted),
        refs,
        "",
    ));
    out.extend(lines[lines.len() - tail..].iter().map(|l| (*l).to_string()));

    let result = join_lines(&out);
    (chars_len(&result) < chars_len(content)).then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs() -> RefCollector {
        RefCollector::new(1 << 20)
    }

    /// A directory of dated log files — the shape the log compressor destroyed.
    fn dated_logs(n: usize) -> Vec<String> {
        (0..n)
            .map(|i| format!("/app/logs/2025-{:02}-{:02}_api.log", 1 + i / 28, 1 + i % 28))
            .collect()
    }

    #[test]
    fn a_small_listing_goes_whole() {
        // 31 definitions is the case that broke: an agent read back the file it
        // was building and got 2 of them.
        let content = (0..31)
            .map(|i| format!("gate_{i} = configure(threshold={i})"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            crush_listing(
                &content,
                &EngineOptions::default(),
                &Context::default(),
                &mut refs()
            )
            .is_none()
        );
    }

    #[test]
    fn a_listing_exactly_at_the_floor_still_goes_whole() {
        let content = dated_logs(MIN_SAMPLED_ENTRIES).join("\n");
        assert!(
            crush_listing(
                &content,
                &EngineOptions::default(),
                &Context::default(),
                &mut refs()
            )
            .is_none()
        );
    }

    #[test]
    fn listing_entries_are_never_normalized() {
        // Every surviving line must be byte-identical: template normalization
        // would fold these into one `<n>-<n>-<n>_api.log`, destroying the dates
        // the listing exists to report.
        let entries = dated_logs(80);
        let content = entries.join("\n");
        let out = crush_listing(
            &content,
            &EngineOptions::default(),
            &Context::default(),
            &mut refs(),
        )
        .unwrap();
        assert!(!out.contains("<n>"), "an entry was normalized: {out}");
        for line in out.lines() {
            if line.contains("omitted") {
                continue;
            }
            assert!(
                entries.iter().any(|e| e == line),
                "rewritten entry {line:?}"
            );
        }
    }

    #[test]
    fn the_first_and_last_entries_always_survive() {
        let entries = dated_logs(120);
        let content = entries.join("\n");
        let out = crush_listing(
            &content,
            &EngineOptions::default(),
            &Context::default(),
            &mut refs(),
        )
        .unwrap();
        assert!(out.starts_with(&entries[0]), "got {out}");
        assert!(out.ends_with(entries.last().unwrap()), "got {out}");
    }

    #[test]
    fn listing_counts_entries_not_frame_lines() {
        // 47 entries behind a 4-line harness frame: 51 lines in total, but the
        // floor counts entries, so the payload is sent whole.
        let body = dated_logs(47).join("\n");
        let framed = format!(
            "Chunk ID: abc\nWall time: 0.4 seconds\nProcess exited with code 0\nOutput:\n{body}"
        );
        assert!(
            crush_listing(
                &framed,
                &EngineOptions::default(),
                &Context::default(),
                &mut refs()
            )
            .is_none()
        );

        // 60 entries behind the same frame: the marker counts entries only.
        let body = dated_logs(60).join("\n");
        let framed = format!(
            "Chunk ID: abc\nWall time: 0.4 seconds\nProcess exited with code 0\nOutput:\n{body}"
        );
        let out = crush_listing(
            &framed,
            &EngineOptions::default(),
            &Context::default(),
            &mut refs(),
        )
        .unwrap();
        assert!(out.starts_with("Chunk ID: abc\n"), "got {out}");
        assert!(out.contains("Process exited with code 0"));
        // budget 10 -> head 6, tail 4, so 50 entries are behind the marker
        assert!(out.contains("[... 50 entries omitted"), "got {out}");
    }

    #[test]
    fn blank_lines_are_not_entries() {
        let entries = dated_logs(60);
        let spaced = entries
            .iter()
            .flat_map(|e| [e.clone(), String::new()])
            .collect::<Vec<_>>()
            .join("\n");
        let out = crush_listing(
            &spaced,
            &EngineOptions::default(),
            &Context::default(),
            &mut refs(),
        )
        .unwrap();
        assert!(out.contains("[... 50 entries omitted"), "got {out}");
    }

    #[test]
    fn a_listing_shorter_than_the_budget_is_left_alone() {
        let content = dated_logs(6).join("\n");
        let options = EngineOptions {
            max_array_items: 20,
            ..Default::default()
        };
        assert!(crush_listing(&content, &options, &Context::default(), &mut refs()).is_none());
    }

    #[test]
    fn listing_declines_as_lossless_but_compresses_as_recoverable() {
        let content = dated_logs(120).join("\n");

        let lossless = EngineOptions {
            mode: crate::types::Mode::Lossless,
            ..Default::default()
        };
        assert!(crush_listing(&content, &lossless, &Context::default(), &mut refs()).is_none());

        let mut collector = refs();
        let out = crush_listing(
            &content,
            &EngineOptions::default(),
            &Context::default(),
            &mut collector,
        )
        .unwrap();
        assert!(chars_len(&out) < chars_len(&content));
        assert!(
            !collector.is_empty(),
            "recoverable mode must retain what it drops"
        );
        assert!(out.contains("ref:k_"), "got {out}");
    }

    #[test]
    fn every_entry_is_either_kept_or_counted() {
        let entries = dated_logs(200);
        let content = entries.join("\n");
        let out = crush_listing(
            &content,
            &EngineOptions::default(),
            &Context::default(),
            &mut refs(),
        )
        .unwrap();
        let kept = out.lines().filter(|l| !l.contains("omitted")).count();
        let counted: usize = out
            .lines()
            .find(|l| l.contains("omitted"))
            .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
            .unwrap();
        assert_eq!(kept + counted, entries.len());
    }

    #[test]
    fn the_same_listing_compresses_to_the_same_bytes() {
        let content = dated_logs(150).join("\n");
        let first = crush_listing(
            &content,
            &EngineOptions::default(),
            &Context::default(),
            &mut refs(),
        )
        .unwrap();
        for _ in 0..4 {
            assert_eq!(
                crush_listing(
                    &content,
                    &EngineOptions::default(),
                    &Context::default(),
                    &mut refs()
                )
                .unwrap(),
                first
            );
        }
    }
}
