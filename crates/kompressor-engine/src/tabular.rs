//! Conservative CSV/TSV/markdown-table row sampling.
//!
//! A table is uniform by construction, so a handful of rows shows its shape and
//! the rest mostly repeat it. What makes that safe is the parse gate below: the
//! payload has to survive a real CSV read with every row the same width before a
//! single row is dropped. Ragged output is not a table — it is text that happens
//! to hold commas, and eliding "rows" from it removes arbitrary lines.

use std::collections::HashSet;

use crate::adaptive::adaptive_k;
use crate::markers::omission_marker;
use crate::options::EngineOptions;
use crate::refs::RefCollector;
use crate::text::{chars_len, join_lines, split_lines};
use crate::types::Context;

/// Fewest rows that can still be called a table rather than a few lines.
const MIN_ROWS: usize = 4;

/// Compress a delimited or markdown table by sampling its body rows.
pub fn crush_tabular(
    content: &str,
    opts: &EngineOptions,
    _ctx: &Context<'_>,
    refs: &mut RefCollector,
) -> Option<String> {
    // Dropped rows cannot be rebuilt from the output, so lossless declines. The
    // reference engine had no mode check here, which made lossless mode silently
    // lose table rows.
    if opts.declines_as_lossless() {
        return None;
    }

    let lines = split_lines(content);
    if lines.len() <= 3.max(opts.max_array_items) {
        return None;
    }

    // The header decides the dialect: a table's first line carries its
    // separators whatever its cells hold.
    let delimiter = if lines[0].contains('\t') {
        b'\t'
    } else if lines[0].contains(',') {
        b','
    } else {
        return crush_markdown(&lines, content, opts, refs);
    };

    if !is_uniform_table(content, delimiter) {
        return None;
    }

    // Selection runs over the *lines*, not the parsed cells: the parse is a gate
    // proving the payload really is a table, and re-serialising cells would
    // rewrite quoting the model can see.
    let (kept, omitted) = select(&lines[1..], opts);
    let mut out: Vec<String> = vec![lines[0].to_string()];
    out.extend(kept.into_iter().map(str::to_string));
    if !omitted.is_empty() {
        out.push(omission_marker(
            "row",
            omitted.len(),
            &join_lines(&omitted),
            refs,
            "",
        ));
    }
    let result = join_lines(&out);
    (chars_len(&result) < chars_len(content)).then_some(result)
}

/// Whether the payload parses as CSV with at least [`MIN_ROWS`] rows, all of the
/// same width.
///
/// The empty-line test is not redundant. The `csv` crate silently *skips* blank
/// lines rather than yielding a zero-width row for them, so a payload whose
/// blank lines break its column structure would still pass the uniform-width
/// test below and be claimed. Selection runs over lines, so a skipped blank
/// would also make the marker's count disagree with what actually left.
fn is_uniform_table(content: &str, delimiter: u8) -> bool {
    let lines = split_lines(content);
    if lines.iter().any(|line| line.is_empty()) {
        return false;
    }
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(false)
        .flexible(true)
        .quoting(true)
        .double_quote(true)
        .from_reader(content.as_bytes());
    let mut width: Option<usize> = None;
    let mut rows = 0usize;
    for record in reader.records() {
        let Ok(record) = record else { return false };
        rows += 1;
        match width {
            None => width = Some(record.len()),
            Some(expected) if expected != record.len() => return false,
            Some(_) => {}
        }
    }
    // One record per line, or it is not a table selection can work on. A
    // quoted cell may hold a newline, so a record can span several lines, and
    // the reader does not break on the form feeds and Unicode separators that
    // `split_lines` does. Selection drops *lines*: on such a payload it kept
    // one half of a record and omitted the other, leaving a quote open.
    rows >= MIN_ROWS && rows == lines.len()
}

/// Markdown tables carry no delimiter the header test can see, so they are
/// recognised by their pipes instead: a header, a rule, and a first body row.
fn crush_markdown(
    lines: &[&str],
    content: &str,
    opts: &EngineOptions,
    refs: &mut RefCollector,
) -> Option<String> {
    if lines.len() < MIN_ROWS || !lines[..3].iter().all(|l| l.trim_start().starts_with('|')) {
        return None;
    }
    let (kept, omitted) = select(&lines[2..], opts);
    let mut out: Vec<String> = lines[..2].iter().map(|l| (*l).to_string()).collect();
    out.extend(kept.into_iter().map(str::to_string));
    if !omitted.is_empty() {
        out.push(omission_marker(
            "row",
            omitted.len(),
            &join_lines(&omitted),
            refs,
            "",
        ));
    }
    let result = join_lines(&out);
    (chars_len(&result) < chars_len(content)).then_some(result)
}

/// Split body rows into `(kept, omitted)`.
///
/// The first two rows establish the shape and the last one reports where the
/// table ends, so all three are kept unconditionally; the budget fills in from
/// the front of what is left.
fn select<'a>(rows: &[&'a str], opts: &EngineOptions) -> (Vec<&'a str>, Vec<&'a str>) {
    if rows.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let budget = adaptive_k(rows, 3, opts.max_array_items.min(rows.len()), 1.0);
    let mut keep: HashSet<usize> = (0..2.min(rows.len())).collect();
    keep.insert(rows.len() - 1);
    for index in 2..budget.saturating_sub(1).min(rows.len() - 1) {
        keep.insert(index);
    }
    let kept = rows
        .iter()
        .enumerate()
        .filter(|(index, _)| keep.contains(index))
        .map(|(_, row)| *row)
        .collect();
    let omitted = rows
        .iter()
        .enumerate()
        .filter(|(index, _)| !keep.contains(index))
        .map(|(_, row)| *row)
        .collect();
    (kept, omitted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs() -> RefCollector {
        RefCollector::new(1 << 20)
    }

    fn csv_table(rows: usize) -> String {
        let mut out = vec!["id,region,status,amount,updated_at".to_string()];
        for i in 0..rows {
            out.push(format!(
                "{i},eu-west-{},active,{},2025-07-03T00:00:00Z",
                i % 4,
                i * 37
            ));
        }
        out.join("\n")
    }

    fn markdown_table(rows: usize) -> String {
        let mut out = vec![
            "| id | region | status |".to_string(),
            "| --- | --- | --- |".to_string(),
        ];
        for i in 0..rows {
            out.push(format!("| {i} | eu-west-{} | active |", i % 4));
        }
        out.join("\n")
    }

    #[test]
    fn a_table_no_longer_than_the_budget_is_left_alone() {
        let content = csv_table(8);
        assert!(
            crush_tabular(
                &content,
                &EngineOptions::default(),
                &Context::default(),
                &mut refs()
            )
            .is_none()
        );
    }

    #[test]
    fn csv_keeps_its_header_and_its_last_row() {
        let content = csv_table(60);
        let out = crush_tabular(
            &content,
            &EngineOptions::default(),
            &Context::default(),
            &mut refs(),
        )
        .unwrap();
        assert!(
            out.starts_with("id,region,status,amount,updated_at\n"),
            "got {out}"
        );
        assert!(
            out.contains("59,eu-west-3,active,2183,"),
            "the last row reports where the table ends: {out}"
        );
        assert!(out.contains("rows omitted"), "got {out}");
        assert!(chars_len(&out) < chars_len(&content));
    }

    #[test]
    fn a_ragged_payload_is_not_a_table() {
        // Prose that happens to hold commas: eliding its "rows" would remove
        // arbitrary lines of text.
        let mut lines = vec!["a,b,c".to_string()];
        for i in 0..40 {
            lines.push(format!("line {i}, with a comma, and, sometimes, more"));
        }
        let content = lines.join("\n");
        assert!(
            crush_tabular(
                &content,
                &EngineOptions::default(),
                &Context::default(),
                &mut refs()
            )
            .is_none()
        );
    }

    #[test]
    fn a_tsv_is_recognised_from_its_first_line() {
        let mut lines = vec!["id\tregion\tstatus".to_string()];
        for i in 0..50 {
            lines.push(format!("{i}\teu-west-{}\tactive and well described", i % 4));
        }
        let content = lines.join("\n");
        let out = crush_tabular(
            &content,
            &EngineOptions::default(),
            &Context::default(),
            &mut refs(),
        )
        .unwrap();
        assert!(out.starts_with("id\tregion\tstatus\n"), "got {out}");
        assert!(out.contains("rows omitted"), "got {out}");
    }

    #[test]
    fn a_blank_line_makes_it_not_a_table() {
        // A blank row is a zero-width row: the payload is not uniform, and the
        // line-based selection below would otherwise count it as an entry.
        let mut lines = vec!["id,region,status".to_string()];
        for i in 0..40 {
            lines.push(format!("{i},eu-west-{},active", i % 4));
            if i == 20 {
                lines.push(String::new());
            }
        }
        let content = lines.join("\n");
        assert!(
            crush_tabular(
                &content,
                &EngineOptions::default(),
                &Context::default(),
                &mut refs()
            )
            .is_none()
        );
    }

    #[test]
    fn a_quoted_cell_holding_the_delimiter_is_still_one_cell() {
        // Counting delimiters instead of parsing would call this ragged.
        let mut lines = vec!["id,note,status".to_string()];
        for i in 0..40 {
            lines.push(format!("{i},\"a note, with a comma\",active"));
        }
        let content = lines.join("\n");
        assert!(
            crush_tabular(
                &content,
                &EngineOptions::default(),
                &Context::default(),
                &mut refs()
            )
            .is_some()
        );
    }

    #[test]
    fn markdown_keeps_its_header_and_its_rule() {
        let content = markdown_table(60);
        let out = crush_tabular(
            &content,
            &EngineOptions::default(),
            &Context::default(),
            &mut refs(),
        )
        .unwrap();
        let kept: Vec<&str> = out.lines().collect();
        assert_eq!(kept[0], "| id | region | status |");
        assert_eq!(
            kept[1], "| --- | --- | --- |",
            "the rule is what makes it a table"
        );
        assert!(out.contains("| 59 | eu-west-3 | active |"), "got {out}");
        assert!(out.contains("rows omitted"), "got {out}");
    }

    #[test]
    fn text_with_neither_a_delimiter_nor_pipes_is_left_alone() {
        let mut lines = Vec::new();
        for i in 0..40 {
            lines.push(format!("plain line number {i} with no separators at all"));
        }
        let content = lines.join("\n");
        assert!(
            crush_tabular(
                &content,
                &EngineOptions::default(),
                &Context::default(),
                &mut refs()
            )
            .is_none()
        );
    }

    #[test]
    fn selection_keeps_the_ends_and_omits_the_middle() {
        let rows: Vec<String> = (0..40).map(|i| format!("row {i} of the body")).collect();
        let body: Vec<&str> = rows.iter().map(|s| s.as_str()).collect();
        let (kept, omitted) = select(&body, &EngineOptions::default());
        assert_eq!(kept[0], "row 0 of the body");
        assert_eq!(kept[1], "row 1 of the body");
        assert_eq!(*kept.last().unwrap(), "row 39 of the body");
        assert_eq!(
            kept.len() + omitted.len(),
            rows.len(),
            "every row is accounted for"
        );
        assert!(!omitted.is_empty());
    }

    #[test]
    fn selection_of_a_single_row_keeps_it() {
        let (kept, omitted) = select(&["only row"], &EngineOptions::default());
        assert_eq!(kept, vec!["only row"]);
        assert!(omitted.is_empty());
    }

    #[test]
    fn tabular_declines_as_lossless_but_compresses_as_recoverable() {
        let content = csv_table(60);

        let lossless = EngineOptions {
            mode: crate::types::Mode::Lossless,
            ..Default::default()
        };
        assert!(crush_tabular(&content, &lossless, &Context::default(), &mut refs()).is_none());

        let mut collector = refs();
        let out = crush_tabular(
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
    fn the_same_table_compresses_to_the_same_bytes() {
        let content = csv_table(80);
        let first = crush_tabular(
            &content,
            &EngineOptions::default(),
            &Context::default(),
            &mut refs(),
        )
        .unwrap();
        for _ in 0..4 {
            assert_eq!(
                crush_tabular(
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
