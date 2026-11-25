//! Rewriting a shell command so that what it prints is already compressed.
//!
//! Cursor offers no way to replace shell output after the command has run, so
//! the only place left is before: `preToolUse` rewrites `command`. For an
//! eligible command `C` the rewrite is
//!
//! ```text
//! {
//! C
//! } > 'F' 2>&1; '/abs/kompressor' hook emit --status "$?" 'F'
//! ```
//!
//! and every piece of it was measured (docs/design/CURSOR.md, "Shell output"): a brace
//! group so `C` runs in the shell Cursor keeps between calls and `cd` and
//! `export` persist; a file rather than a pipe so the exit status is `C`'s; no
//! variables, subshells, `||` or `rm`, because Cursor's allow-list parser
//! rejects them; absolute paths because the shell may not share our `PATH`.
//!
//! What follows is a tokenizer, not a shell parser. It knows quotes, escapes,
//! heredocs, comments and the control operators, which is enough to answer
//! the questions in the "not rewritten" table. Where it cannot decide it does
//! not rewrite: a missed rewrite costs tokens, a wrong one costs a confused
//! agent.

use std::path::Path;

/// Why a command was left as the model wrote it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leave {
    /// Nothing to run.
    Empty,
    /// It already runs this binary: `kompressor retrieve` must come back whole.
    OwnBinary,
    /// It backgrounds a job, which would keep writing to a file that is gone.
    Background,
    /// It wants a terminal, which a redirected stdout no longer is.
    Interactive,
    /// It already sends its standard output to a file.
    RedirectsStdout,
    /// Every command in it is on `hooks.cursor.shell.skip`.
    Skipped,
    /// Unterminated quote or heredoc, unbalanced brace or parenthesis, a
    /// trailing line continuation: embedding it would change its meaning.
    Unbalanced,
    /// It would end the shell, or change how it treats errors, before the
    /// output could be printed: `exit`, `exec`, `set`, `trap`.
    ControlsShell,
    /// A construct the tokenizer does not follow (`case`).
    Unsure,
    /// Windows: the rewrite is POSIX `sh`.
    Platform,
}

impl Leave {
    pub fn as_str(self) -> &'static str {
        match self {
            Leave::Empty => "empty",
            Leave::OwnBinary => "own_binary",
            Leave::Background => "background",
            Leave::Interactive => "interactive",
            Leave::RedirectsStdout => "redirects_stdout",
            Leave::Skipped => "skipped",
            Leave::Unbalanced => "unbalanced",
            Leave::ControlsShell => "controls_shell",
            Leave::Unsure => "unsure",
            Leave::Platform => "platform",
        }
    }
}

/// One simple command at the top level: its words, quotes removed, and
/// whether it sends standard output to a file.
#[derive(Debug, Default)]
struct Simple {
    words: Vec<String>,
    stdout_to_file: bool,
}

/// What the tokenizer learned about a whole command line.
#[derive(Debug, Default)]
struct Scan {
    commands: Vec<Simple>,
    background: bool,
    unbalanced: bool,
}

/// Walk `text` once.
fn scan(text: &str) -> Scan {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Scan::default();
    let mut current = Simple::default();
    let mut word = String::new();
    let mut in_word = false;
    // Depth of `(`, `$(`, and `{` (as a word) — anything inside is not the
    // top level, and must balance.
    let mut parens: i64 = 0;
    let mut braces: i64 = 0;
    // Heredoc delimiters waiting for the end of the current line.
    let mut pending: Vec<(String, bool)> = Vec::new();
    // A redirection operator seen, waiting for its target word: whether it
    // was standard output going to a file.
    let mut redirect: Option<bool> = None;
    let mut i = 0;

    let finish_word = |word: &mut String,
                       in_word: &mut bool,
                       current: &mut Simple,
                       redirect: &mut Option<bool>,
                       parens: i64,
                       braces: &mut i64| {
        if !*in_word {
            return;
        }
        let w = std::mem::take(word);
        *in_word = false;
        if let Some(to_stdout_file) = redirect.take() {
            if to_stdout_file && !w.starts_with('&') {
                current.stdout_to_file = true;
            }
            return;
        }
        let _ = parens;
        if current.words.is_empty() {
            if w == "{" {
                *braces += 1;
                return;
            } else if w == "}" {
                *braces -= 1;
                return;
            }
        } else if w == "}" {
            *braces -= 1;
            return;
        }
        current.words.push(w);
    };

    macro_rules! end_command {
        () => {{
            finish_word(
                &mut word,
                &mut in_word,
                &mut current,
                &mut redirect,
                parens,
                &mut braces,
            );
            if !current.words.is_empty() || current.stdout_to_file {
                out.commands.push(std::mem::take(&mut current));
            }
        }};
    }

    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                match chars.get(i + 1) {
                    None => {
                        // A trailing continuation would join our `}` to C.
                        out.unbalanced = true;
                        i += 1;
                    }
                    Some('\n') => {
                        if i + 2 >= chars.len() {
                            out.unbalanced = true;
                        }
                        i += 2;
                    }
                    Some(&next) => {
                        word.push(next);
                        in_word = true;
                        i += 2;
                    }
                }
                continue;
            }
            '\'' => {
                in_word = true;
                i += 1;
                let mut closed = false;
                while i < chars.len() {
                    if chars[i] == '\'' {
                        closed = true;
                        i += 1;
                        break;
                    }
                    word.push(chars[i]);
                    i += 1;
                }
                if !closed {
                    out.unbalanced = true;
                }
                continue;
            }
            '"' => {
                in_word = true;
                i += 1;
                let mut closed = false;
                let mut sub = 0i64;
                while i < chars.len() {
                    match chars[i] {
                        '\\' if i + 1 < chars.len() => {
                            word.push(chars[i + 1]);
                            i += 2;
                            continue;
                        }
                        '$' if chars.get(i + 1) == Some(&'(') => {
                            sub += 1;
                            i += 2;
                            continue;
                        }
                        ')' if sub > 0 => sub -= 1,
                        '"' if sub == 0 => {
                            closed = true;
                            i += 1;
                            break;
                        }
                        other => word.push(other),
                    }
                    i += 1;
                }
                if !closed || sub != 0 {
                    out.unbalanced = true;
                }
                continue;
            }
            '`' => {
                in_word = true;
                i += 1;
                let mut closed = false;
                while i < chars.len() {
                    if chars[i] == '\\' {
                        i += 2;
                        continue;
                    }
                    if chars[i] == '`' {
                        closed = true;
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                if !closed {
                    out.unbalanced = true;
                }
                continue;
            }
            '#' if !in_word => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            // What runs inside `( )` and `$( )` runs too, so it is classified
            // like any other command; the parentheses only have to balance.
            '$' if chars.get(i + 1) == Some(&'(') => {
                end_command!();
                parens += 1;
                i += 2;
                continue;
            }
            '(' => {
                end_command!();
                parens += 1;
                i += 1;
                continue;
            }
            ')' => {
                end_command!();
                parens -= 1;
                if parens < 0 {
                    out.unbalanced = true;
                    parens = 0;
                }
                i += 1;
                continue;
            }
            '<' if chars.get(i + 1) == Some(&'<') && chars.get(i + 2) != Some(&'<') => {
                // A heredoc: read its delimiter word now, its body after the
                // end of this line.
                finish_word(
                    &mut word,
                    &mut in_word,
                    &mut current,
                    &mut redirect,
                    parens,
                    &mut braces,
                );
                i += 2;
                let strip_tabs = chars.get(i) == Some(&'-');
                if strip_tabs {
                    i += 1;
                }
                while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
                    i += 1;
                }
                let mut delimiter = String::new();
                while i < chars.len() && !" \t\n;&|<>()".contains(chars[i]) {
                    if chars[i] != '\'' && chars[i] != '"' && chars[i] != '\\' {
                        delimiter.push(chars[i]);
                    }
                    i += 1;
                }
                if delimiter.is_empty() {
                    out.unbalanced = true;
                } else {
                    pending.push((delimiter, strip_tabs));
                }
                continue;
            }
            '>' | '<' => {
                // `N>`, `>`, `>>`, `>|`, `&>`, `>&`. Standard output to a file
                // is `>` / `1>` / `>>` / `&>` / `>|` followed by a word that
                // is not `&N`.
                let fd: Option<String> = if in_word && word.chars().all(|d| d.is_ascii_digit()) {
                    let fd = std::mem::take(&mut word);
                    in_word = false;
                    Some(fd)
                } else {
                    finish_word(
                        &mut word,
                        &mut in_word,
                        &mut current,
                        &mut redirect,
                        parens,
                        &mut braces,
                    );
                    None
                };
                let mut stdout = c == '>' && fd.as_deref().is_none_or(|fd| fd == "1");
                i += 1;
                if c == '>' && matches!(chars.get(i), Some('>') | Some('|')) {
                    i += 1;
                }
                if chars.get(i) == Some(&'&') {
                    // `>&N` duplicates a descriptor; `>&file` is rare.
                    i += 1;
                    stdout = false;
                }
                redirect = Some(stdout);
                while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
                    i += 1;
                }
                continue;
            }
            '&' => {
                if chars.get(i + 1) == Some(&'&') {
                    end_command!();
                    i += 2;
                    continue;
                }
                if chars.get(i + 1) == Some(&'>') {
                    // `&>file`: both streams to a file.
                    finish_word(
                        &mut word,
                        &mut in_word,
                        &mut current,
                        &mut redirect,
                        parens,
                        &mut braces,
                    );
                    i += 2;
                    if chars.get(i) == Some(&'>') {
                        i += 1;
                    }
                    redirect = Some(true);
                    while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
                        i += 1;
                    }
                    continue;
                }
                // A lone `&`: a background job, wherever it is.
                out.background = true;
                end_command!();
                i += 1;
                continue;
            }
            '|' | ';' => {
                end_command!();
                i += 1;
                if matches!(chars.get(i), Some('|') | Some('&') | Some(';')) {
                    i += 1;
                }
                continue;
            }
            '\n' => {
                end_command!();
                i += 1;
                // Heredoc bodies start here, in the order they were opened.
                for (delimiter, strip_tabs) in std::mem::take(&mut pending) {
                    let mut found = false;
                    while i < chars.len() {
                        let start = i;
                        while i < chars.len() && chars[i] != '\n' {
                            i += 1;
                        }
                        let line: String = chars[start..i].iter().collect();
                        if i < chars.len() {
                            i += 1;
                        }
                        let line = if strip_tabs {
                            line.trim_start_matches('\t')
                        } else {
                            line.as_str()
                        };
                        if line == delimiter {
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        out.unbalanced = true;
                    }
                }
                continue;
            }
            ' ' | '\t' | '\r' => {
                finish_word(
                    &mut word,
                    &mut in_word,
                    &mut current,
                    &mut redirect,
                    parens,
                    &mut braces,
                );
                i += 1;
                continue;
            }
            other => {
                word.push(other);
                in_word = true;
                i += 1;
            }
        }
    }
    end_command!();
    if !pending.is_empty() || parens != 0 || braces != 0 || redirect.is_some() {
        out.unbalanced = true;
    }
    out
}

/// The command word of a simple command: past variable assignments and the
/// wrappers that only change how it is run.
fn command_words(words: &[String]) -> &[String] {
    let mut i = 0;
    while i < words.len() {
        let w = &words[i];
        let assignment = w.split_once('=').is_some_and(|(name, _)| {
            !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        if assignment || matches!(w.as_str(), "command" | "builtin" | "time" | "env" | "!") {
            i += 1;
            continue;
        }
        break;
    }
    &words[i..]
}

fn basename(word: &str) -> &str {
    word.rsplit(['/', '\\']).next().unwrap_or(word)
}

/// Whether a command needs a terminal on standard output.
fn interactive(words: &[String]) -> bool {
    let Some(first) = words.first() else {
        return false;
    };
    let name = basename(first);
    let args = &words[1..];
    let has = |flag: &str| args.iter().any(|a| a == flag);
    match name {
        "vim" | "vi" | "nvim" | "nano" | "emacs" | "less" | "more" | "most" | "top" | "htop"
        | "btop" | "watch" | "tmux" | "screen" | "man" | "fzf" | "telnet" | "ftp" | "mc"
        | "ranger" | "nnn" | "tig" | "lazygit" => true,
        "sudo" => !has("-n") && !args.iter().any(|a| a.starts_with("--non-interactive")),
        "ssh" | "mosh" => {
            // ssh [options] host [command...]: interactive without a command.
            const WITH_ARG: &[&str] = &[
                "-b", "-c", "-D", "-E", "-e", "-F", "-I", "-i", "-J", "-L", "-l", "-m", "-O", "-o",
                "-p", "-Q", "-R", "-S", "-W", "-w",
            ];
            let mut positional = 0;
            let mut i = 0;
            while i < args.len() {
                if WITH_ARG.contains(&args[i].as_str()) {
                    i += 2;
                    continue;
                }
                if !args[i].starts_with('-') {
                    positional += 1;
                }
                i += 1;
            }
            positional < 2
        }
        "git" => {
            let sub = args
                .iter()
                .find(|a| !a.starts_with('-'))
                .map(String::as_str);
            match sub {
                Some("rebase") => has("-i") || has("--interactive"),
                Some("add") | Some("checkout") | Some("reset") | Some("stash")
                | Some("restore") => {
                    has("-p") || has("--patch") || has("-i") || has("--interactive")
                }
                // Without a message, commit opens an editor.
                Some("commit") => {
                    has("-p")
                        || has("--patch")
                        || has("-i")
                        || !args.iter().any(|a| {
                            a == "-m"
                                || a.starts_with("-m")
                                || a.starts_with("--message")
                                || a == "-F"
                                || a.starts_with("--file")
                                || a == "--no-edit"
                                || a == "--amend" && has("--no-edit")
                        })
                }
                _ => false,
            }
        }
        "python" | "python3" | "python2" | "ipython" | "node" | "deno" | "bun" | "irb" | "ruby"
        | "perl" | "php" | "lua" | "ghci" | "R" | "julia" | "bash" | "sh" | "zsh" | "fish"
        | "dash" | "ksh" | "psql" | "mysql" | "sqlite3" | "redis-cli" | "mongo" | "mongosh" => {
            // A REPL with no script: every argument an option, and none of
            // the options that take a program.
            let program = args.iter().any(|a| {
                matches!(
                    a.as_str(),
                    "-c" | "-e"
                        | "-m"
                        | "--eval"
                        | "-p"
                        | "--print"
                        | "-r"
                        | "-f"
                        | "--command"
                        | "--file"
                        | "-x"
                )
            });
            let script = args.iter().any(|a| !a.starts_with('-'));
            let one_arg_is_db = matches!(name, "sqlite3" | "psql" | "mysql" | "redis-cli")
                && args.iter().filter(|a| !a.starts_with('-')).count() < 2;
            if one_arg_is_db {
                return !program;
            }
            !program && !script
        }
        _ => false,
    }
}

fn on_skip_list(words: &[String], skip: &[String]) -> bool {
    let Some(first) = words.first() else {
        return true;
    };
    let first = basename(first);
    skip.iter().any(|entry| {
        let mut parts = entry.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some(cmd), None) => cmd == first,
            (Some(cmd), Some(sub)) => {
                cmd == first
                    && words
                        .iter()
                        .skip(1)
                        .find(|w| !w.starts_with('-'))
                        .is_some_and(|w| w == sub)
            }
            _ => false,
        }
    })
}

/// Decide whether `command` may be rewritten.
pub fn classify(command: &str, skip: &[String]) -> Result<(), Leave> {
    if cfg!(windows) {
        return Err(Leave::Platform);
    }
    if command.trim().is_empty() {
        return Err(Leave::Empty);
    }
    let scan = scan(command);
    if scan.unbalanced {
        return Err(Leave::Unbalanced);
    }
    if scan.background {
        return Err(Leave::Background);
    }
    let mut all_skipped = true;
    for simple in &scan.commands {
        if simple.stdout_to_file {
            return Err(Leave::RedirectsStdout);
        }
        let words = command_words(&simple.words);
        let Some(first) = words.first() else { continue };
        let name = basename(first);
        if matches!(name, "kompressor" | "kompressor.exe") {
            return Err(Leave::OwnBinary);
        }
        if matches!(name, "nohup" | "disown" | "setsid" | "bg") {
            return Err(Leave::Background);
        }
        if matches!(
            name,
            "exit" | "exec" | "return" | "logout" | "set" | "trap" | "shopt" | "setopt"
        ) {
            return Err(Leave::ControlsShell);
        }
        if matches!(name, "case" | "esac" | "select" | "coproc") {
            return Err(Leave::Unsure);
        }
        if interactive(words) {
            return Err(Leave::Interactive);
        }
        if !on_skip_list(words, skip) {
            all_skipped = false;
        }
    }
    if scan.commands.is_empty() {
        return Err(Leave::Empty);
    }
    if all_skipped {
        return Err(Leave::Skipped);
    }
    Ok(())
}

/// Single-quote a path for `sh`, or `None` when it cannot be done without an
/// escape the allow-list might refuse.
pub fn quote(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    if text.is_empty() || text.contains(['\'', '\n', '\r']) {
        return None;
    }
    Some(format!("'{text}'"))
}

/// A token safe to pass to `hook emit` unquoted inside single quotes.
pub fn safe_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c))
}

/// What the tail of the rewrite carries for attribution.
#[derive(Default)]
pub struct EmitArgs<'a> {
    pub config: Option<&'a Path>,
    pub request: Option<&'a str>,
    pub conversation: Option<&'a str>,
    pub model: Option<&'a str>,
}

/// How this binary is named at the head of a command Cursor will judge.
///
/// Cursor's allow-list compares the command word **as written**, measured on
/// CLI 2026.11.22: `Shell(kompressor)` matches a bare `kompressor` and nothing
/// else; `Shell(/opt/bin/kompressor)` matches that path unquoted and *not*
/// `'/opt/bin/kompressor'`. Every other piece of the rewrite — the quoted
/// capture file, `"$?"`, the flags — passed. The first form shipped quoted the
/// absolute path, and every rewritten command was refused, approved or not.
///
/// So, in order: the bare name when `path` resolves `kompressor` to this very
/// binary (`Shell(kompressor)` then approves it); the absolute path unquoted
/// when it holds nothing a shell would split or expand (`Shell(<path>)`
/// approves that); quoted as a last resort, which runs where everything is
/// approved and is refused under an allow-list. `None` when not even quoting
/// is safe.
pub fn program_word(exe: &Path, path: Option<&std::ffi::OsStr>) -> Option<String> {
    let canonical = |p: &Path| std::fs::canonicalize(p).ok();
    if let Some(path) = path
        && let Some(this) = canonical(exe)
    {
        let found = std::env::split_paths(path)
            .map(|dir| {
                dir.join(if cfg!(windows) {
                    "kompressor.exe"
                } else {
                    "kompressor"
                })
            })
            .find(|candidate| candidate.is_file());
        if found.and_then(|f| canonical(&f)).is_some_and(|f| f == this) {
            return Some("kompressor".to_string());
        }
    }
    let text = exe.to_str()?;
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c))
    {
        return Some(text.to_string());
    }
    quote(exe)
}

/// The rewritten command with this binary named as [`program_word`] would
/// name it with no `PATH` to search, or `None` when a path cannot be quoted
/// safely.
#[cfg(test)]
pub fn rewrite(
    command: &str,
    binary: &Path,
    capture: &Path,
    emit: &EmitArgs<'_>,
) -> Option<String> {
    rewrite_with(command, &program_word(binary, None)?, capture, emit)
}

/// The rewritten command, `program` being this binary's command word (see
/// [`program_word`]), or `None` when a path cannot be quoted safely.
///
/// `command` is embedded verbatim: no quoting pass that could change what it
/// means. The newline before `}` lets it end in a heredoc or a comment.
pub fn rewrite_with(
    command: &str,
    program: &str,
    capture: &Path,
    emit: &EmitArgs<'_>,
) -> Option<String> {
    let bin = program;
    let file = quote(capture)?;
    let mut tail = format!("{bin} hook emit");
    if let Some(config) = emit.config {
        tail.push_str(&format!(" --config {}", quote(config)?));
    }
    for (flag, value) in [
        ("--request", emit.request),
        ("--conversation", emit.conversation),
        ("--model", emit.model),
    ] {
        if let Some(value) = value.filter(|v| safe_token(v)) {
            tail.push_str(&format!(" {flag} '{value}'"));
        }
    }
    let body = command.strip_suffix('\n').unwrap_or(command);
    Some(format!(
        "{{\n{body}\n}} > {file} 2>&1; {tail} --status \"$?\" {file}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skip() -> Vec<String> {
        kompressor_core::config::CursorShellHooksConfig::default().skip
    }

    fn leaves(command: &str) -> Result<(), Leave> {
        classify(command, &skip())
    }

    /// AC-5.1: one test per row of the "not rewritten" table, each with a
    /// command left alone and a near miss that is rewritten.
    #[test]
    fn this_binary_is_never_wrapped() {
        assert_eq!(
            leaves("kompressor retrieve k_AbC123dEf456"),
            Err(Leave::OwnBinary)
        );
        assert_eq!(
            leaves("'/usr/bin/kompressor' retrieve k_AbC123dEf456 --lines 1:5"),
            Err(Leave::OwnBinary)
        );
        assert_eq!(leaves("cat kompressor.log"), Ok(()));
    }

    #[test]
    fn a_background_job_is_left_alone() {
        assert_eq!(leaves("npm run dev &"), Err(Leave::Background));
        assert_eq!(leaves("sleep 1 & wait"), Err(Leave::Background));
        assert_eq!(leaves("nohup ./server"), Err(Leave::Background));
        assert_eq!(leaves("setsid ./server"), Err(Leave::Background));
        assert_eq!(leaves("cargo build && cargo test"), Ok(()));
        assert_eq!(leaves("cargo test 2>&1"), Ok(()));
        assert_eq!(leaves("echo 'a & b' | wc -c"), Ok(()));
    }

    #[test]
    fn a_command_that_wants_a_terminal_is_left_alone() {
        for command in [
            "vim src/main.rs",
            "less log.txt",
            "top",
            "watch ls",
            "tmux attach",
            "ssh host",
            "ssh -p 2222 host",
            "python3",
            "node",
            "sudo apt install x",
            "git rebase -i HEAD~3",
            "git add -p",
        ] {
            assert_eq!(leaves(command), Err(Leave::Interactive), "{command}");
        }
        for command in [
            "ssh host uptime",
            "python3 script.py",
            "python3 -c 'print(1)'",
            "node build.js",
            "sudo -n true",
            "git rebase main",
            "git log --oneline",
        ] {
            assert_eq!(leaves(command), Ok(()), "{command}");
        }
    }

    #[test]
    fn output_already_going_to_a_file_is_left_alone() {
        assert_eq!(
            leaves("cargo build > build.log"),
            Err(Leave::RedirectsStdout)
        );
        assert_eq!(leaves("make 1> out.txt 2>&1"), Err(Leave::RedirectsStdout));
        assert_eq!(leaves("make &> out.txt"), Err(Leave::RedirectsStdout));
        assert_eq!(leaves("make >> out.txt"), Err(Leave::RedirectsStdout));
        assert_eq!(leaves("make 2> err.txt"), Ok(()));
        assert_eq!(leaves("make >&2"), Ok(()));
        assert_eq!(leaves("grep x < in.txt"), Ok(()));
    }

    #[test]
    fn a_command_on_the_skip_list_is_left_alone() {
        assert_eq!(leaves("cd src"), Err(Leave::Skipped));
        assert_eq!(leaves("git commit -m 'x'"), Err(Leave::Skipped));
        assert_eq!(leaves("mkdir -p a && cd a"), Err(Leave::Skipped));
        assert_eq!(leaves("cd src && cargo build"), Ok(()));
        assert_eq!(leaves("git status"), Ok(()));
    }

    #[test]
    fn an_unbalanced_command_is_left_alone() {
        for command in [
            "echo 'unterminated",
            "echo \"unterminated",
            "{ ls",
            "ls }",
            "(ls",
            "ls )",
            "cat <<EOF\nno end",
            "ls \\",
            "echo $(date",
            "ls >",
        ] {
            assert_eq!(leaves(command), Err(Leave::Unbalanced), "{command:?}");
        }
        for command in [
            "{ ls; }",
            "(cd a && ls)",
            "cat <<EOF\nhello\nEOF",
            "echo $(date)",
            "f() { ls; }; f",
        ] {
            assert_eq!(leaves(command), Ok(()), "{command:?}");
        }
    }

    #[test]
    fn a_command_that_would_end_the_shell_is_left_alone() {
        assert_eq!(leaves("exit 1"), Err(Leave::ControlsShell));
        assert_eq!(leaves("make || exit 1"), Err(Leave::ControlsShell));
        assert_eq!(leaves("set -e; make"), Err(Leave::ControlsShell));
        assert_eq!(leaves("exec bash"), Err(Leave::ControlsShell));
        assert_eq!(leaves("make; echo exit"), Ok(()));
    }

    #[test]
    fn nothing_and_comments_are_left_alone() {
        assert_eq!(leaves(""), Err(Leave::Empty));
        assert_eq!(leaves("   \n"), Err(Leave::Empty));
        assert_eq!(leaves("# just a comment"), Err(Leave::Empty));
    }

    #[test]
    fn the_rewrite_is_the_measured_brace_form() {
        let rewritten = rewrite(
            "seq 1 30",
            Path::new("/usr/bin/kompressor"),
            Path::new("/tmp/kompressor-x.out"),
            &EmitArgs::default(),
        )
        .unwrap();
        assert_eq!(
            rewritten,
            "{\nseq 1 30\n} > '/tmp/kompressor-x.out' 2>&1; /usr/bin/kompressor hook emit \
             --status \"$?\" '/tmp/kompressor-x.out'"
        );
    }

    /// AC-5.8: around the user's command there is no `(`, no `||` and no `$`
    /// but `$?`, and no executable but this one.
    #[test]
    fn the_wrapper_uses_nothing_the_allow_list_refuses() {
        let command = "echo $(date) || (exit 3)";
        let rewritten = rewrite(
            command,
            Path::new("/opt/kompressor"),
            Path::new("/tmp/c.out"),
            &EmitArgs {
                config: Some(Path::new("/etc/k.toml")),
                request: Some("gen-1"),
                conversation: Some("conv-2"),
                model: Some("model.x"),
            },
        )
        .unwrap();
        let wrapper = rewritten.replacen(command, "", 1);
        assert!(!wrapper.contains('('), "{wrapper}");
        assert!(!wrapper.contains("||"), "{wrapper}");
        assert_eq!(wrapper.matches('$').count(), 1, "{wrapper}");
        assert!(wrapper.contains("\"$?\""));
        assert!(!wrapper.contains("rm "));
    }

    #[test]
    fn a_path_that_cannot_be_single_quoted_is_not_used() {
        assert!(
            rewrite(
                "ls",
                Path::new("/opt/it's/kompressor"),
                Path::new("/tmp/c.out"),
                &EmitArgs::default()
            )
            .is_none()
        );
    }

    #[test]
    fn an_unsafe_attribution_value_is_dropped_not_quoted() {
        let rewritten = rewrite(
            "ls",
            Path::new("/k"),
            Path::new("/tmp/c.out"),
            &EmitArgs {
                request: Some("x'; rm -rf /"),
                ..EmitArgs::default()
            },
        )
        .unwrap();
        assert!(!rewritten.contains("--request"));
    }

    /// The command word is what Cursor's allow-list judges, literally.
    #[test]
    fn the_program_is_named_as_an_allow_list_can_match_it() {
        // No PATH, a plain path: unquoted, so `Shell(<path>)` matches it.
        assert_eq!(
            program_word(Path::new("/opt/kz-1.2/bin/kompressor"), None).as_deref(),
            Some("/opt/kz-1.2/bin/kompressor")
        );
        // A path a shell would split: quoted, the last resort.
        assert_eq!(
            program_word(Path::new("/opt/my tools/kompressor"), None).as_deref(),
            Some("'/opt/my tools/kompressor'")
        );
        assert_eq!(program_word(Path::new("/opt/it's/kompressor"), None), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_bare_name_is_used_only_when_path_finds_this_very_binary() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let exe = real.join("kompressor");
        std::fs::write(&exe, "").unwrap();
        // PATH holds a symlink to it: the same binary, so the bare name.
        let linked = dir.path().join("linked");
        std::fs::create_dir_all(&linked).unwrap();
        std::os::unix::fs::symlink(&exe, linked.join("kompressor")).unwrap();
        let path = std::env::join_paths([&linked]).unwrap();
        assert_eq!(
            program_word(&exe, Some(&path)).as_deref(),
            Some("kompressor")
        );
        // PATH finds a different kompressor first: never the bare name.
        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("kompressor"), "x").unwrap();
        let path = std::env::join_paths([&other, &linked]).unwrap();
        assert_eq!(program_word(&exe, Some(&path)).as_deref(), exe.to_str());
    }

    /// AC-5.5: a heredoc, a comment and a continuation at the end.
    #[cfg(unix)]
    #[test]
    fn awkward_endings_rewrite_to_a_form_sh_accepts() {
        for command in [
            "cat <<EOF\nhello\nEOF",
            "ls # trailing comment",
            "ls a \\\n  b",
        ] {
            assert_eq!(leaves(command), Ok(()), "{command:?}");
            let rewritten = rewrite(
                command,
                Path::new("/bin/true"),
                Path::new("/tmp/c.out"),
                &EmitArgs::default(),
            )
            .unwrap();
            let status = std::process::Command::new("sh")
                .args(["-n", "-c", &rewritten])
                .status()
                .unwrap();
            assert!(status.success(), "sh -n refuses {rewritten:?}");
        }
    }
}
