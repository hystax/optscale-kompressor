//! Colour for the human-readable parts of `kompressor compress`.
//!
//! Two rules keep this from ever changing what a pipeline sees:
//!
//! - **Only standard error is coloured.** The compressed payload on standard
//!   output, and everything `--json` prints, is written exactly as before.
//! - **Colour is a layer, not a format.** Everything here builds the same text
//!   it always did, with ANSI styles wrapped around parts of it, and writes it
//!   through an [`anstream`] stream that strips the styles again whenever colour
//!   is off: `--color never`, standard error not a terminal, `NO_COLOR`,
//!   `TERM=dumb`. With colour off the bytes are the uncoloured report's, which
//!   the tests check by stripping a forced-colour run and comparing.
//!
//! Text that came from the payload — marker excerpts in the report, every line
//! of `--show-output` — has its control characters replaced with visible
//! stand-ins first, so a payload cannot drive the terminal it is shown in.
//!
//! The highlighter for `--show-output` is deliberately small: it knows the
//! engine's omission markers exactly and the payload types the detector names
//! roughly (JSON tokens, log levels, diff lines, `path:line:` search hits, and
//! comments, strings and keywords in code and configuration). Compressed code is
//! full of elided spans, so a real grammar would mostly be guessing anyway.

use std::borrow::Cow;
use std::io::Write;

use anstream::AutoStream;
use anstyle::{AnsiColor, Style};
use kompressor_engine::markers::has_marker;

/// `--color`: when standard error gets colour.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorWhen {
    /// When standard error is a terminal that takes colour, and neither
    /// `NO_COLOR` nor `TERM=dumb` says otherwise.
    #[default]
    Auto,
    Always,
    Never,
}

/// Standard error, colouring or stripping as `when` says.
pub fn stderr(when: ColorWhen) -> AutoStream<std::io::Stderr> {
    let raw = std::io::stderr();
    match when {
        ColorWhen::Auto => AutoStream::auto(raw),
        ColorWhen::Always => AutoStream::always(raw),
        ColorWhen::Never => AutoStream::never(raw),
    }
}

/// Write `text` to standard error under `when`. A closed standard error is not
/// worth failing a compression that already printed its payload.
pub fn write_stderr(when: ColorWhen, text: &str) {
    let mut stream = stderr(when);
    let _ = stream.write_all(text.as_bytes());
    let _ = stream.flush();
}

// ---------------------------------------------------------------- styles

const fn fg(color: AnsiColor) -> Style {
    Style::new().fg_color(Some(anstyle::Color::Ansi(color)))
}

pub const LABEL: Style = Style::new().dimmed();
pub const VALUE: Style = Style::new().bold();
pub const GOOD: Style = fg(AnsiColor::Green);
pub const WARN: Style = fg(AnsiColor::Yellow);
pub const BAD: Style = fg(AnsiColor::Red).bold();
pub const HANDLE: Style = fg(AnsiColor::Cyan);
pub const MARKER: Style = fg(AnsiColor::Yellow).bold();
const KEY: Style = fg(AnsiColor::Blue);
const STRING: Style = fg(AnsiColor::Green);
const NUMBER: Style = fg(AnsiColor::Magenta);
const LITERAL: Style = fg(AnsiColor::Yellow);
const COMMENT: Style = Style::new().dimmed().italic();
const KEYWORD: Style = fg(AnsiColor::Blue).bold();
const ADDED: Style = fg(AnsiColor::Green);
const REMOVED: Style = fg(AnsiColor::Red);
const HUNK: Style = fg(AnsiColor::Cyan);
const HEADER: Style = Style::new().bold();
const PATH: Style = fg(AnsiColor::Magenta);
const LINE_NO: Style = fg(AnsiColor::Green);
const TAG: Style = fg(AnsiColor::Blue);
const GUTTER: Style = Style::new().dimmed();

/// `text` wrapped in `style`. With colour off the wrapping is stripped and
/// `text` is all that is left.
pub fn paint(style: Style, text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    format!("{style}{text}{style:#}")
}

// ---------------------------------------------------------------- safety

/// `text` with every control character but tab replaced by a visible
/// stand-in: C0 controls by their Unicode control pictures (`␛` for escape,
/// `␍` for a carriage return), DEL by `␡`, and C1 controls by `�`. Newlines
/// are replaced too; callers that show several lines split on them first.
pub fn sanitize(text: &str) -> Cow<'_, str> {
    let unsafe_char = |c: char| (c < ' ' && c != '\t') || ('\u{7f}'..='\u{9f}').contains(&c);
    if !text.chars().any(unsafe_char) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.chars()
            .map(|c| match c {
                '\t' => '\t',
                c if c < ' ' => char::from_u32(0x2400 + c as u32).unwrap_or('\u{fffd}'),
                '\u{7f}' => '\u{2421}',
                c if ('\u{80}'..='\u{9f}').contains(&c) => '\u{fffd}',
                c => c,
            })
            .collect(),
    )
}

// ---------------------------------------------------------------- output view

/// How `--show-output` reads a payload, from what the detector called it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hint {
    Json,
    Log,
    Diff,
    Search,
    Html,
    /// Source code with `#` line comments (Python and other scripting
    /// languages) or with `//` ones.
    Code {
        hash_comments: bool,
        single_quoted_strings: bool,
    },
    /// YAML, TOML or INI.
    Config,
    Plain,
}

impl Hint {
    /// From the detection report's names: `content_type`, `language`, and the
    /// configuration flavour. A forced type arrives as its `--type` name.
    pub fn from_names(content_type: &str, language: Option<&str>, flavor: Option<&str>) -> Hint {
        match content_type {
            "json_array" | "json" => Hint::Json,
            "build" | "log" => Hint::Log,
            "diff" => Hint::Diff,
            "search" => Hint::Search,
            "html" => Hint::Html,
            "config" => match flavor {
                Some("yaml" | "toml" | "ini") | None => Hint::Config,
                Some(_) => Hint::Json,
            },
            "source_code" => {
                let language = language.unwrap_or("");
                Hint::Code {
                    hash_comments: language == "python",
                    single_quoted_strings: matches!(
                        language,
                        "python" | "javascript" | "typescript"
                    ),
                }
            }
            _ => Hint::Plain,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Hint::Json => "json",
            Hint::Log => "log",
            Hint::Diff => "diff",
            Hint::Search => "search",
            Hint::Html => "html",
            Hint::Code { .. } => "code",
            Hint::Config => "config",
            Hint::Plain => "plain text",
        }
    }
}

/// The compressed payload for a person to read: one numbered line per output
/// line, omission markers and their handles picked out, and the rest coloured
/// as `hint` reads it. Strip the styles and drop the gutter, and each line is
/// the payload's line with its control characters made visible.
pub fn output_view(output: &str, hint: Hint) -> String {
    use std::fmt::Write as _;
    let lines: Vec<&str> = output.split('\n').collect();
    // A payload ending in a newline does not have an empty last line to show.
    let shown = if lines.len() > 1 && lines.last() == Some(&"") {
        &lines[..lines.len() - 1]
    } else {
        &lines[..]
    };
    let width = shown.len().to_string().len().max(3);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}{}",
        paint(LABEL, "output       "),
        paint(
            VALUE,
            &format!(
                "{} lines, read as {} (markers highlighted)",
                shown.len(),
                hint.name()
            )
        )
    );
    for (index, line) in shown.iter().enumerate() {
        let line = sanitize(line);
        let _ = writeln!(
            out,
            "{} {}",
            paint(GUTTER, &format!("{:>width$} │", index + 1)),
            highlight_line(&line, hint)
        );
    }
    out
}

/// One output line, markers first, then `hint` over the text between them.
pub fn highlight_line(line: &str, hint: Hint) -> String {
    let mut out = String::with_capacity(line.len() + 32);
    let segments = marker_segments(line);
    // A log marker summarises what it elided (`…; 0 ERROR, 1 WARN]`), so the
    // line's level is read from its own text, never from its markers.
    let own_text: String = segments
        .iter()
        .filter(|(is_marker, _)| !is_marker)
        .map(|(_, text)| *text)
        .collect();
    let line_style = line_style(&own_text, hint);
    let mut first = true;
    for (is_marker, segment) in segments {
        if is_marker {
            out.push_str(&paint_marker(segment));
        } else {
            out.push_str(&highlight_text(segment, hint, line_style, first));
        }
        first = false;
    }
    out
}

/// A style that colours a whole line: diff lines, and log lines by level.
fn line_style(line: &str, hint: Hint) -> Option<Style> {
    match hint {
        Hint::Diff => {
            if line.starts_with("+++")
                || line.starts_with("---")
                || line.starts_with("diff ")
                || line.starts_with("index ")
            {
                Some(HEADER)
            } else if line.starts_with("@@") {
                Some(HUNK)
            } else if line.starts_with('+') {
                Some(ADDED)
            } else if line.starts_with('-') {
                Some(REMOVED)
            } else {
                None
            }
        }
        Hint::Log => log_level(line),
        _ => None,
    }
}

fn log_level(line: &str) -> Option<Style> {
    let trimmed = line.trim_start();
    let words = || {
        line.split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty())
    };
    if trimmed.starts_with("error")
        || trimmed.starts_with("panicked")
        || trimmed.starts_with("thread '")
        || words().any(|w| matches!(w, "ERROR" | "FATAL" | "PANIC" | "CRITICAL" | "FAILED"))
    {
        Some(fg(AnsiColor::Red))
    } else if trimmed.starts_with("warning") || words().any(|w| matches!(w, "WARN" | "WARNING")) {
        Some(WARN)
    } else if words().any(|w| matches!(w, "DEBUG" | "TRACE")) {
        Some(Style::new().dimmed())
    } else {
        None
    }
}

/// `line` cut into marker spans and the text between, in order.
///
/// A marker is a top-level bracket span carrying one of the engine's marker
/// literals — `[... 412 lines omitted [ref:k_…]]`, `[×47]`, `…[+120 chars]` —
/// the same rule `--explain` uses to list them. Other brackets are text.
fn marker_segments(line: &str) -> Vec<(bool, &str)> {
    let mut segments = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut emitted = 0usize;
    for (index, byte) in line.bytes().enumerate() {
        match byte {
            b'[' => {
                if depth == 0 {
                    start = index;
                }
                depth += 1;
            }
            b']' if depth > 0 => {
                depth -= 1;
                if depth == 0 && has_marker(&line[start..=index]) {
                    if start > emitted {
                        segments.push((false, &line[emitted..start]));
                    }
                    segments.push((true, &line[start..=index]));
                    emitted = index + 1;
                }
            }
            _ => {}
        }
    }
    if emitted < line.len() || segments.is_empty() {
        segments.push((false, &line[emitted..]));
    }
    segments
}

/// A marker, with the handle inside it in the handle colour.
fn paint_marker(marker: &str) -> String {
    const NEEDLE: &str = "ref:k_";
    let Some(at) = marker.find(NEEDLE) else {
        return paint(MARKER, marker);
    };
    let tail = &marker[at..];
    let end = NEEDLE.len()
        + tail[NEEDLE.len()..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(tail.len() - NEEDLE.len());
    format!(
        "{}{}{}",
        paint(MARKER, &marker[..at]),
        paint(HANDLE.bold(), &tail[..end]),
        paint(MARKER, &tail[end..])
    )
}

/// The text between markers.
fn highlight_text(text: &str, hint: Hint, line_style: Option<Style>, first: bool) -> String {
    if let Some(style) = line_style {
        return paint(style, text);
    }
    match hint {
        Hint::Json => json_tokens(text),
        Hint::Search if first => search_prefix(text),
        Hint::Html => html_tags(text),
        Hint::Code {
            hash_comments,
            single_quoted_strings,
        } => code_tokens(text, hash_comments, single_quoted_strings),
        Hint::Config if first => config_line(text),
        Hint::Log if first => log_timestamp(text),
        _ => text.to_string(),
    }
}

/// Keys, strings, numbers and literals of JSON text.
fn json_tokens(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (at, c) = chars[i];
        if c == '"' {
            let mut j = i + 1;
            while j < chars.len() {
                match chars[j].1 {
                    '\\' => j += 2,
                    '"' => break,
                    _ => j += 1,
                }
            }
            let end = chars.get(j + 1).map_or(text.len(), |(e, _)| *e);
            let end = end.min(text.len());
            let token = &text[at..end];
            let is_key = text[end..].trim_start().starts_with(':');
            out.push_str(&paint(if is_key { KEY } else { STRING }, token));
            i = j + 1;
            continue;
        }
        let starts_word = i == 0 || !chars[i - 1].1.is_ascii_alphanumeric();
        if starts_word
            && (c.is_ascii_digit()
                || (c == '-' && chars.get(i + 1).is_some_and(|n| n.1.is_ascii_digit())))
        {
            let mut j = i + 1;
            while j < chars.len()
                && (chars[j].1.is_ascii_digit()
                    || matches!(chars[j].1, '.' | 'e' | 'E' | '+' | '-'))
            {
                j += 1;
            }
            let end = chars.get(j).map_or(text.len(), |(e, _)| *e);
            out.push_str(&paint(NUMBER, &text[at..end]));
            i = j;
            continue;
        }
        if starts_word {
            let rest = &text[at..];
            if let Some(word) = ["true", "false", "null"].into_iter().find(|w| {
                rest.starts_with(w)
                    && !rest[w.len()..]
                        .chars()
                        .next()
                        .is_some_and(|n| n.is_ascii_alphanumeric())
            }) {
                out.push_str(&paint(LITERAL, word));
                i += word.len();
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// `path:line:` or `path:line-` at the start of a grep or ripgrep hit.
fn search_prefix(text: &str) -> String {
    let Some(colon) = text.find(':') else {
        return text.to_string();
    };
    let rest = &text[colon + 1..];
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let separator = rest.as_bytes().get(digits);
    if colon == 0 || digits == 0 || !matches!(separator, Some(b':' | b'-')) {
        return text.to_string();
    }
    let number_end = colon + 1 + digits;
    format!(
        "{}:{}{}",
        paint(PATH, &text[..colon]),
        paint(LINE_NO, &text[colon + 1..number_end]),
        &text[number_end..]
    )
}

/// A leading timestamp, dimmed: `2026-01-01T00:00:00Z`, `2026-01-01 00:00:00,123`.
fn log_timestamp(text: &str) -> String {
    let bytes = text.as_bytes();
    let date = bytes.len() >= 10
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(u8::is_ascii_digit);
    if !date {
        return text.to_string();
    }
    let end = text.find([' ', '\t']).unwrap_or(text.len());
    // `2026-01-01 00:00:00`: the time is part of the stamp.
    let end = if end == 10
        && text[11..]
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_digit())
    {
        11 + text[11..].find([' ', '\t']).unwrap_or(text.len() - 11)
    } else {
        end
    };
    format!(
        "{}{}",
        paint(Style::new().dimmed(), &text[..end]),
        &text[end..]
    )
}

/// Tags of HTML text.
fn html_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        let Some(close) = rest[open..].find('>') else {
            break;
        };
        out.push_str(&rest[..open]);
        let tag = &rest[open..open + close + 1];
        out.push_str(&paint(
            if tag.starts_with("<!--") {
                COMMENT
            } else {
                TAG
            },
            tag,
        ));
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out
}

/// Words that read as keywords across the languages the detector names.
const KEYWORDS: &[&str] = &[
    "fn",
    "let",
    "mut",
    "pub",
    "impl",
    "struct",
    "enum",
    "trait",
    "use",
    "mod",
    "match",
    "if",
    "else",
    "for",
    "while",
    "loop",
    "return",
    "break",
    "continue",
    "def",
    "class",
    "import",
    "from",
    "as",
    "with",
    "try",
    "except",
    "finally",
    "raise",
    "lambda",
    "yield",
    "async",
    "await",
    "function",
    "const",
    "var",
    "export",
    "interface",
    "type",
    "new",
    "package",
    "func",
    "go",
    "defer",
    "public",
    "private",
    "protected",
    "static",
    "void",
    "namespace",
    "using",
    "include",
    "switch",
    "case",
    "default",
    "throw",
    "throws",
    "catch",
    "self",
    "this",
    "None",
    "True",
    "False",
    "nil",
    "null",
    "true",
    "false",
    "in",
    "not",
    "and",
    "or",
    "is",
    "where",
    "extends",
    "implements",
    "elif",
    "pass",
    "struct",
    "unsafe",
    "dyn",
    "ref",
    "crate",
    "super",
];

/// Comments, strings and keywords of source code.
fn code_tokens(text: &str, hash_comments: bool, single_quoted: bool) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (at, c) = chars[i];
        let rest = &text[at..];
        let comment = if hash_comments {
            c == '#'
        } else {
            rest.starts_with("//")
        };
        if comment {
            out.push_str(&paint(COMMENT, rest));
            return out;
        }
        if c == '"' || (single_quoted && c == '\'') || (!hash_comments && c == '`' && single_quoted)
        {
            let mut j = i + 1;
            while j < chars.len() {
                match chars[j].1 {
                    '\\' => j += 2,
                    q if q == c => break,
                    _ => j += 1,
                }
            }
            let end = chars
                .get(j + 1)
                .map_or(text.len(), |(e, _)| *e)
                .min(text.len());
            out.push_str(&paint(STRING, &text[at..end]));
            i = j + 1;
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let mut j = i + 1;
            while j < chars.len() && (chars[j].1.is_alphanumeric() || chars[j].1 == '_') {
                j += 1;
            }
            let end = chars.get(j).map_or(text.len(), |(e, _)| *e);
            let word = &text[at..end];
            if KEYWORDS.contains(&word) {
                out.push_str(&paint(KEYWORD, word));
            } else {
                out.push_str(word);
            }
            i = j;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// One YAML, TOML or INI line: comments, section headers, keys.
fn config_line(text: &str) -> String {
    let trimmed = text.trim_start();
    let indent = &text[..text.len() - trimmed.len()];
    if trimmed.starts_with('#') || trimmed.starts_with(';') {
        return format!("{indent}{}", paint(COMMENT, trimmed));
    }
    if trimmed.starts_with('[') && trimmed.trim_end().ends_with(']') {
        return format!("{indent}{}", paint(HEADER, trimmed));
    }
    let (item, body) = match trimmed.strip_prefix("- ") {
        Some(body) => ("- ", body),
        None => ("", trimmed),
    };
    let split = body
        .char_indices()
        .find(|&(_, c)| c == ':' || c == '=' || c == ' ' || c == '"' || c == '\'');
    match split {
        Some((at, sep)) if at > 0 && (sep == ':' || sep == '=') => {
            format!("{indent}{item}{}{}", paint(KEY, &body[..at]), &body[at..])
        }
        _ => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(text: &str) -> String {
        anstream::adapter::strip_str(text).to_string()
    }

    #[test]
    fn stripping_the_colour_gives_back_exactly_the_text() {
        let samples = [
            (
                r#"{"name": "x", "n": -1.5e3, "ok": true, "v": null} [... 3 items omitted [ref:k_AbC123dEf456]]"#,
                Hint::Json,
            ),
            ("2026-01-01T00:00:00Z ERROR boom [×47]", Hint::Log),
            ("+added", Hint::Diff),
            ("@@ -1,2 +1,3 @@", Hint::Diff),
            ("src/app.rs:42: let x = 1; …[+120 chars]", Hint::Search),
            ("<div class=\"a\"><!-- c --></div>", Hint::Html),
            (
                "fn main() { let s = \"x // y\"; } // tail",
                Hint::Code {
                    hash_comments: false,
                    single_quoted_strings: false,
                },
            ),
            (
                "def f(x): return 'a#b'  # note",
                Hint::Code {
                    hash_comments: true,
                    single_quoted_strings: true,
                },
            ),
            ("  - key: value", Hint::Config),
            ("[section]", Hint::Config),
            ("plain [not a marker] text", Hint::Plain),
            ("unterminated \"string", Hint::Json),
            ("", Hint::Plain),
        ];
        for (line, hint) in samples {
            let coloured = highlight_line(line, hint);
            assert_eq!(strip(&coloured), line, "{hint:?}: {coloured:?}");
        }
    }

    #[test]
    fn markers_and_their_handles_are_picked_out() {
        let line = "a [... 3 lines omitted [ref:k_AbC123dEf456]] b [plain]";
        let segments = marker_segments(line);
        assert_eq!(
            segments,
            vec![
                (false, "a "),
                (true, "[... 3 lines omitted [ref:k_AbC123dEf456]]"),
                (false, " b [plain]"),
            ]
        );
        let painted = paint_marker(segments[1].1);
        assert!(painted.contains(&paint(HANDLE.bold(), "ref:k_AbC123dEf456")));
    }

    #[test]
    fn control_characters_cannot_reach_the_terminal() {
        assert_eq!(sanitize("plain\ttab"), "plain\ttab");
        assert_eq!(sanitize("\u{1b}[2J\r\u{7}\u{7f}\u{9b}"), "␛[2J␍␇␡\u{fffd}");
        let view = output_view("ok\n\u{1b}]0;title\u{7}\n", Hint::Plain);
        assert!(!strip(&view).contains('\u{1b}'));
        assert!(strip(&view).contains("␛]0;title␇"));
    }

    #[test]
    fn the_view_numbers_each_line_and_skips_a_trailing_newline() {
        let view = strip(&output_view("one\ntwo\n", Hint::Plain));
        let lines: Vec<&str> = view.lines().collect();
        assert_eq!(
            lines[0],
            "output       2 lines, read as plain text (markers highlighted)"
        );
        assert_eq!(lines[1], "  1 │ one");
        assert_eq!(lines[2], "  2 │ two");
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn the_detector_names_choose_the_reading() {
        assert_eq!(Hint::from_names("json_array", None, None), Hint::Json);
        assert_eq!(Hint::from_names("build", None, None), Hint::Log);
        assert_eq!(Hint::from_names("config", None, Some("yaml")), Hint::Config);
        assert_eq!(
            Hint::from_names("source_code", Some("python"), None),
            Hint::Code {
                hash_comments: true,
                single_quoted_strings: true
            }
        );
        assert_eq!(
            Hint::from_names("source_code", Some("rust"), None),
            Hint::Code {
                hash_comments: false,
                single_quoted_strings: false
            }
        );
        assert_eq!(Hint::from_names("text", None, None), Hint::Plain);
    }

    #[test]
    fn log_levels_colour_the_line() {
        assert_eq!(log_level("2026 ERROR x"), Some(fg(AnsiColor::Red)));
        assert_eq!(log_level("warning: unused"), Some(WARN));
        assert_eq!(log_level("INFO all good"), None);
        assert_eq!(log_level("ERRORS are not a level"), None);
        // A marker's own summary does not colour the line it sits on.
        let line = "INFO done [... 38 log lines omitted [ref:k_AbC123dEf456]; 0 ERROR, 1 WARN]";
        let painted = highlight_line(line, Hint::Log);
        assert!(painted.starts_with("INFO done "), "{painted:?}");
    }
}
