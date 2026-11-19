//! Config bodies: classifying them, and eliding the comment and blank lines a
//! model does not need.
//!
//! A config read is dense with whole-line comments and spacing that carry no
//! information the model is going to act on, so dropping them is a high ratio
//! for a small risk — and in recoverable mode the dropped lines come back
//! through one handle, in their original order. The elision is not reversible
//! by construction, though, so lossless mode passes config bodies through.
//!
//! The risk that is not small is mistaking a comment for data. Three constructs
//! make `#` meaningful content rather than a remark, and each gets its own
//! guard below: a YAML block scalar, a TOML multi-line string, and an INI value
//! continued across indented lines.

use once_cell::sync::Lazy;
use regex::Regex;

use crate::markers::omission_marker;
use crate::options::EngineOptions;
use crate::refs::RefCollector;
use crate::text::{chars_len, is_blank, join_lines, nonblank_lines, split_lines};
use crate::types::{ConfigFlavor, Context};

/// `[section]`, and TOML's `[[array of tables]]`.
static SECTION: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*\[\[?[^\[\]\n]+\]\]?\s*$").unwrap());
/// `key = value`, the INI/TOML assignment.
static ASSIGN: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*[A-Za-z0-9_.\-]+\s*=\s*\S").unwrap());
/// `key: value` or a bare `key:`.
///
/// The colon has to be followed by whitespace and content, or end the line.
/// Without that, every `Note: something` in prose and every line holding a bare
/// `http://…` URL counts as a YAML mapping, and a wall of links reads as config.
static YAML_KEY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\s*[A-Za-z0-9_.\-]+:(?:\s+\S.*)?\s*$").unwrap());
/// `- item`.
static YAML_LIST: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*-\s+\S").unwrap());
static COMMENT: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*[#;]").unwrap());

static HASH_COMMENT: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*#").unwrap());

/// `key: |`, `key: >-`, `key: |2`, and — YAML 1.2 allows the chomping and
/// indentation indicators in either order — both `|2-` and `|-2`.
///
/// A match means "do not elide anything in this payload", so the digits are
/// deliberately looser than the spec's single 1-9: over-matching costs one
/// uncompressed payload, while under-matching strips `#` lines that were text
/// inside the scalar.
///
/// The indicator need not follow a key. A sequence entry opens a scalar with
/// `- |`, which is how every CI file writes a multi-line script; a tag or an
/// anchor may sit in front of it (`key: !!str |`, `key: &run |`); and a comment
/// may follow it (`key: | # why`). Each of those used to slip through, and the
/// `#` lines of a shell script inside the scalar were then elided as comments.
static YAML_BLOCK_SCALAR: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?m)(?::|^\s*-)[ \t]*(?:[!&]\S+[ \t]+)*[|>][+-]?\d*[+-]?[ \t]*(?:#.*)?\r?$")
        .unwrap()
});

/// Below this the marker costs about as much as the lines it replaces, and the
/// payload is churned for nothing.
const MIN_ELIDED_LINES: usize = 3;

/// Lines sampled for classification, and the floor for deciding at all.
const CLASSIFY_LINES: usize = 200;
const MIN_CLASSIFY_LINES: usize = 5;
/// Share of sampled lines that must be structural before a body is config.
const STRUCTURAL_RATIO: f64 = 0.8;

/// Classify a config body as YAML / TOML / INI — or nothing.
///
/// Deliberately conservative, because markdown reads as config from a distance:
/// its headings match the comment pattern and its bullets match the YAML list
/// pattern. So the structural-line ratio has to be high *and* the primary
/// signal — assignments under a `[section]`, or `key: value` mappings — has to
/// dominate. Getting this wrong is not a missed ratio, it is `#` lines stripped
/// out of a document where they were the content.
pub fn config_flavor(content: &str) -> Option<ConfigFlavor> {
    let lines: Vec<&str> = nonblank_lines(content)
        .into_iter()
        .take(CLASSIFY_LINES)
        .collect();
    if lines.len() < MIN_CLASSIFY_LINES {
        return None;
    }
    let count = |re: &Regex| lines.iter().filter(|l| re.is_match(l)).count();
    let sections = count(&SECTION);
    let assigns = count(&ASSIGN);
    let yaml_keys = count(&YAML_KEY);
    let yaml_lists = count(&YAML_LIST);
    let comments = count(&COMMENT);
    let total = lines.len() as f64;

    if sections > 0
        && assigns >= 2
        && (sections + assigns + comments) as f64 / total >= STRUCTURAL_RATIO
    {
        // `;` is the INI comment character; TOML has only `#`. The distinction
        // is not cosmetic here — it decides whether indented comment lines are
        // elidable or are the continuation of a value.
        let semi = lines.iter().any(|l| l.trim_start().starts_with(';'));
        return Some(if semi {
            ConfigFlavor::Ini
        } else {
            ConfigFlavor::Toml
        });
    }
    if yaml_keys >= 3
        && yaml_keys >= yaml_lists
        && (yaml_keys + yaml_lists + comments) as f64 / total >= STRUCTURAL_RATIO
    {
        return Some(ConfigFlavor::Yaml);
    }
    None
}

/// Elide the comment and blank lines of a config body.
///
/// `None` is a decline: the caller keeps the payload byte for byte.
pub fn crush_config(
    content: &str,
    opts: &EngineOptions,
    _ctx: &Context<'_>,
    refs: &mut RefCollector,
) -> Option<String> {
    // Taken for a uniform compressor signature; nothing about a config body is
    // decided by where in the conversation it arrived.
    if opts.is_lossless() {
        return None;
    }
    let flavor = config_flavor(content)?;
    if !elision_safe(content, flavor) {
        return None;
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut omitted: Vec<&str> = Vec::new();
    for line in split_lines(content) {
        if elidable(line, flavor) {
            omitted.push(line);
        } else {
            kept.push(line);
        }
    }
    if omitted.len() < MIN_ELIDED_LINES {
        return None;
    }

    let marker = omission_marker("config", omitted.len(), &join_lines(&omitted), refs, "");
    let mut result = join_lines(&kept);
    if !kept.is_empty() {
        result.push('\n');
    }
    result.push_str(&marker);
    // The never-larger invariant: a body that is mostly one long comment can
    // come out longer once the handle is spelled in.
    (chars_len(&result) < chars_len(content)).then_some(result)
}

/// Whether comments in this body are remarks rather than data.
fn elision_safe(content: &str, flavor: ConfigFlavor) -> bool {
    match flavor {
        // Everything indented under `key: |` is literal text, `#` lines
        // included. There is no cheap way to find where the scalar ends, so the
        // whole payload is left alone.
        ConfigFlavor::Yaml => !YAML_BLOCK_SCALAR.is_match(content),
        // Same argument for TOML's `'''`/`"""` multi-line strings.
        ConfigFlavor::Toml => !content.contains("'''") && !content.contains("\"\"\""),
        ConfigFlavor::Ini => true,
    }
}

/// Whether one line can be dropped.
fn elidable(line: &str, flavor: ConfigFlavor) -> bool {
    match flavor {
        // An INI value may continue onto indented lines that hold anything at
        // all, blank lines and `#`/`;` among it, so only a comment starting in
        // column 0 is safely a comment — and blanks are kept, since a blank can
        // be part of such a value.
        ConfigFlavor::Ini => line.starts_with(['#', ';']),
        _ => is_blank(line) || HASH_COMMENT.is_match(line),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Mode;

    const YAML: &str = "\
# deployment settings
name: service
replicas: 3

# the ports the sidecar listens on
port: 8080
timeout: 30

# retries are cheap here
retries: 5
";

    const TOML: &str = "\
[package]
# the crate name
name = \"thing\"
version = \"0.1.0\"

# dependencies follow
[dependencies]
serde = \"1\"

regex = \"1\"
";

    const INI: &str = "\
; server configuration
[server]
; listening address
host = 127.0.0.1
port = 8080

; timeouts are in seconds
read = 30
write = 30
";

    fn crush(content: &str) -> Option<String> {
        let opts = EngineOptions::default();
        let mut refs = RefCollector::disabled();
        crush_config(content, &opts, &Context::default(), &mut refs)
    }

    #[test]
    fn yaml_toml_and_ini_are_each_recognised() {
        assert_eq!(config_flavor(YAML), Some(ConfigFlavor::Yaml));
        assert_eq!(config_flavor(TOML), Some(ConfigFlavor::Toml));
        assert_eq!(config_flavor(INI), Some(ConfigFlavor::Ini));
    }

    #[test]
    fn a_semicolon_comment_is_what_separates_ini_from_toml() {
        // identical structure, one character apart
        let toml = "[a]\n# c\nx = 1\ny = 2\nz = 3\n";
        let ini = "[a]\n; c\nx = 1\ny = 2\nz = 3\n";
        assert_eq!(config_flavor(toml), Some(ConfigFlavor::Toml));
        assert_eq!(config_flavor(ini), Some(ConfigFlavor::Ini));
    }

    #[test]
    fn markdown_is_not_mistaken_for_config() {
        let markdown = "\
# Title

Some introduction that runs on for a sentence.

## Steps

- install the thing
- configure the thing
- run the thing

See the notes below for details.
";
        assert_eq!(config_flavor(markdown), None);
    }

    #[test]
    fn prose_is_not_mistaken_for_config() {
        let prose = "\
The service reads its configuration at startup.
Note: the file must exist before the process starts.
Otherwise it exits with a nonzero status.
Operators usually keep it under /etc.
There is no reload signal; restart instead.
";
        assert_eq!(config_flavor(prose), None);
    }

    #[test]
    fn a_wall_of_urls_is_not_a_yaml_mapping() {
        // `http:` would be a key if the colon did not have to be followed by
        // whitespace, and a link dump would read as config
        let urls = "\
https://example.com/one
https://example.com/two
https://example.com/three
https://example.com/four
https://example.com/five
";
        assert_eq!(config_flavor(urls), None);
    }

    #[test]
    fn a_body_too_short_to_judge_is_not_classified() {
        assert_eq!(config_flavor("a: 1\nb: 2\nc: 3\nd: 4\n"), None);
    }

    #[test]
    fn a_bullet_list_alone_is_not_yaml() {
        let bullets = "- one\n- two\n- three\n- four\n- five\n";
        assert_eq!(config_flavor(bullets), None, "keys must dominate lists");
    }

    #[test]
    fn comments_and_blanks_are_replaced_by_one_marker() {
        let out = crush(YAML).expect("a yaml body with three comments compresses");
        assert!(out.contains("name: service"));
        assert!(out.contains("retries: 5"));
        assert!(!out.contains("retries are cheap"));
        assert!(out.contains("comment/blank lines omitted"));
        assert!(chars_len(&out) < chars_len(YAML));
    }

    #[test]
    fn the_elided_lines_are_counted_and_retained_in_order() {
        let opts = EngineOptions::default();
        let mut refs = RefCollector::new(65536);
        let out = crush_config(YAML, &opts, &Context::default(), &mut refs).unwrap();
        // three comments plus two blanks
        assert!(
            out.contains("[... 5 comment/blank lines omitted"),
            "got {out}"
        );
        assert!(out.contains("ref:k_"));
        let (_, fragment) = &refs.pairs()[0];
        assert!(fragment.starts_with("# deployment settings"));
        assert!(fragment.ends_with("# retries are cheap here"));
    }

    #[test]
    fn the_marker_is_the_last_line() {
        let out = crush(TOML).unwrap();
        let last = out.lines().last().unwrap();
        assert!(last.starts_with("[... "), "got {last}");
    }

    #[test]
    fn a_block_scalar_makes_the_whole_body_untouchable() {
        let yaml = format!("{YAML}script: |\n  # this is shell, not a remark\n  run --now\n");
        assert_eq!(config_flavor(&yaml), Some(ConfigFlavor::Yaml));
        assert_eq!(crush(&yaml), None, "a `#` inside a block scalar is content");
    }

    #[test]
    fn every_block_scalar_indicator_spelling_is_caught() {
        for header in ["|", "|-", "|+", ">", ">-", "|2", "|2-", "|-2", ">+1"] {
            let yaml = format!("{YAML}script: {header}\n  # content\n  more\n");
            assert_eq!(crush(&yaml), None, "header {header} should block elision");
        }
    }

    #[test]
    fn a_toml_multiline_string_makes_the_whole_body_untouchable() {
        for quote in ["'''", "\"\"\""] {
            let toml = format!("{TOML}notes = {quote}\n# not a comment\n{quote}\n");
            assert_eq!(crush(&toml), None, "quote {quote} should block elision");
        }
    }

    #[test]
    fn an_ini_keeps_indented_comments_because_they_may_be_values() {
        let ini = "\
; server configuration
[server]
; listening address
host = 127.0.0.1
banner = first line
    # this is part of the banner
    ; so is this
port = 8080
; and this one is a real comment
read = 30
write = 30
";
        let out = crush(ini).expect("three column-zero comments are elidable");
        assert!(out.contains("    # this is part of the banner"));
        assert!(out.contains("    ; so is this"));
        assert!(!out.contains("; listening address"));
        assert!(
            out.contains("[... 3 comment/blank lines omitted"),
            "got {out}"
        );
    }

    #[test]
    fn an_ini_keeps_blank_lines() {
        // a blank line can sit inside a multi-line value
        let out = crush(INI).expect("three column-zero comments are elidable");
        assert!(out.contains("\n\n"), "blanks survive in ini: {out:?}");
    }

    #[test]
    fn the_lossless_gate_declines() {
        let opts = EngineOptions {
            mode: Mode::Lossless,
            ..Default::default()
        };
        let mut refs = RefCollector::disabled();
        assert_eq!(
            crush_config(YAML, &opts, &Context::default(), &mut refs),
            None,
            "elision is not reversible by construction"
        );
    }

    #[test]
    fn too_few_elidable_lines_is_left_alone() {
        let yaml = "# one comment\nname: service\nport: 8080\ntimeout: 30\nretries: 5\n";
        assert_eq!(config_flavor(yaml), Some(ConfigFlavor::Yaml));
        assert_eq!(crush(yaml), None);
    }

    #[test]
    fn a_body_that_is_not_config_is_left_alone() {
        assert_eq!(crush("just\nsome\nplain\nprose\nlines\nhere\n"), None);
    }

    #[test]
    fn output_is_never_longer_than_the_input() {
        // three short comments cost less than the marker that would replace them
        let yaml = "#\n#\n#\nname: x\nport: 1\nhost: y\nuser: z\nmode: q\n";
        assert_eq!(config_flavor(yaml), Some(ConfigFlavor::Yaml));
        assert_eq!(crush(yaml), None, "a longer result must be refused");
    }

    #[test]
    fn compression_is_deterministic() {
        let a = crush(YAML).unwrap();
        let b = crush(YAML).unwrap();
        assert_eq!(a, b);
    }
}
