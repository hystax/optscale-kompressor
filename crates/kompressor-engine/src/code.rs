//! Source compression that is validated by a parser, not by a heuristic.
//!
//! A function body is the largest thing in a source payload an agent almost
//! never needs verbatim, and the one thing that can be removed without touching
//! what the file *declares*: imports, signatures, types, class structure and
//! decorators all survive. So bodies come out and a marker goes in their place.
//!
//! The rule that keeps this safe is that the output is parsed again before it
//! is returned. Validity outranks ratio: anything that fails to parse either
//! before or after the edit is declined and the caller keeps the original
//! bytes. A language with no grammar entry is left alone by design.
//!
//! Two paths live here because two shapes of language do. Python is line-based
//! — a body is a run of lines, and what replaces it has to be indented far
//! enough to still be that body. Brace languages are byte-based — a body is the
//! span between its braces, and the replacement is a comment between the same
//! two braces.

/// Without the grammars the compressor declines everything rather than guessing
/// at structure with regexes. That is the same fail-open answer the Python
/// service gives when its optional grammar pack is not installed: less
/// compression, never a mangled file.
#[cfg(not(feature = "code-aware"))]
pub fn crush_code(
    content: &str,
    opts: &crate::options::EngineOptions,
    _ctx: &crate::types::Context<'_>,
    refs: &mut crate::refs::RefCollector,
    language: Option<crate::types::Language>,
) -> Option<String> {
    let _ = (content, opts, refs, language);
    None
}

#[cfg(feature = "code-aware")]
pub use imp::crush_code;

#[cfg(feature = "code-aware")]
mod imp {
    use std::borrow::Cow;

    use once_cell::sync::Lazy;
    use tree_sitter::{Language as Grammar, Node, Parser, Tree};

    use crate::markers::code_marker;
    use crate::options::EngineOptions;
    use crate::preamble::split_harness_preamble;
    use crate::refs::RefCollector;
    use crate::text::{chars_len, join_lines, split_lines, split_lines_keepends};
    use crate::types::{Context, Language};

    /// Elide the long function bodies of `content`, or decline.
    ///
    /// `None` means "keep the input untouched" and is returned for every
    /// refusal, including the ones that got as far as building an output. A
    /// refusal can leave fragments in `refs` that no marker now points at; the
    /// pipeline rolls the collector back when a candidate loses, which is the
    /// only reason this does not have to.
    ///
    /// The conversation context is deliberately unread — see [`Context`]:
    /// consulting it would make a payload's bytes depend on the turn that asked
    /// for it, and a prompt that changes every turn is a prompt-cache miss.
    pub fn crush_code(
        content: &str,
        opts: &EngineOptions,
        _ctx: &Context<'_>,
        refs: &mut RefCollector,
        language: Option<Language>,
    ) -> Option<String> {
        let language = language?;
        // The switch is honoured here, not only where whole payloads are
        // routed: the mixed segmenter, the lead-in and multi-file retries all
        // reach this function directly, and with the check only in routing a
        // run configured without code elision still elided the bodies inside
        // every `ls && cat` and fenced block.
        if !opts.enable_code_aware {
            return None;
        }
        // Eliding a function body is not reversible, so this compressor has no
        // business running in `lossless` — the one mode whose promise is that
        // nothing is dropped. With `strict_lossless` off the check is skipped
        // and bodies go anyway, which is the behaviour the recorded vectors in
        // `fixtures/golden.json.gz` were captured with.
        if opts.declines_as_lossless() {
            return None;
        }
        // Bounded work. A payload this size is not the source file this exists
        // for, and parsing it twice is not worth finding out.
        if content.len() > opts.max_code_bytes {
            return None;
        }

        // The harness frame is not part of the source, so it comes off before
        // any parse: with it in place the grammar reports a syntax error on
        // line 1 and the whole file is handed back. Three real C payloads
        // (24.4K, bodies up to 83 lines) were declined for that reason alone.
        let (preamble, source) = split_harness_preamble(content);

        let crushed = match language {
            Language::Python => crush_python(source, opts, refs),
            _ => crush_braces(source, opts, refs, language),
        }?;

        let result = format!("{preamble}{crushed}");
        // Chars, not bytes: every size comparison in this engine counts
        // code points (see `crate::text::chars_len`).
        (chars_len(&result) < chars_len(content)).then_some(result)
    }

    // ---------------------------------------------------------------- grammars

    // Loaded once per process. A grammar is a static table behind a pointer, so
    // the cost is in `Parser::set_language` rather than here, but re-deriving it
    // per call would put an FFI round trip on every payload for nothing.
    static PYTHON: Lazy<Grammar> = Lazy::new(|| tree_sitter_python::LANGUAGE.into());
    static JAVASCRIPT: Lazy<Grammar> = Lazy::new(|| tree_sitter_javascript::LANGUAGE.into());
    static TYPESCRIPT: Lazy<Grammar> =
        Lazy::new(|| tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into());
    static GO: Lazy<Grammar> = Lazy::new(|| tree_sitter_go::LANGUAGE.into());
    static RUST: Lazy<Grammar> = Lazy::new(|| tree_sitter_rust::LANGUAGE.into());
    static JAVA: Lazy<Grammar> = Lazy::new(|| tree_sitter_java::LANGUAGE.into());
    static CSHARP: Lazy<Grammar> = Lazy::new(|| tree_sitter_c_sharp::LANGUAGE.into());
    static C: Lazy<Grammar> = Lazy::new(|| tree_sitter_c::LANGUAGE.into());
    static CPP: Lazy<Grammar> = Lazy::new(|| tree_sitter_cpp::LANGUAGE.into());

    fn grammar(language: Language) -> &'static Grammar {
        match language {
            Language::Python => &PYTHON,
            Language::JavaScript => &JAVASCRIPT,
            Language::TypeScript => &TYPESCRIPT,
            Language::Go => &GO,
            Language::Rust => &RUST,
            Language::Java => &JAVA,
            Language::CSharp => &CSHARP,
            Language::C => &C,
            Language::Cpp => &CPP,
        }
    }

    /// Parse, or `None` if the grammar cannot be installed at all.
    ///
    /// Note what this does *not* promise: tree-sitter always returns a tree,
    /// error nodes and all. Whether that tree is acceptable is a separate
    /// question each path answers for itself.
    fn parse(text: &str, language: Language) -> Option<Tree> {
        let mut parser = Parser::new();
        parser.set_language(grammar(language)).ok()?;
        parser.parse(text, None)
    }

    // ------------------------------------------------------------ tree walking

    /// Pre-order over named descendants.
    ///
    /// Iterative, and that is load-bearing rather than a style choice. A parse
    /// tree is as deep as the source nests, and a *wrong* grammar nests it far
    /// further — two real 34K and 40K payloads produce a tree that blew
    /// a recursive walker's stack when read with the Go grammar, failing straight
    /// past the fail-open promise and turning a compress call into a 500.
    /// Compression may decline a payload; it must never take the request down.
    struct PreOrder<'t> {
        stack: Vec<Node<'t>>,
    }

    impl<'t> Iterator for PreOrder<'t> {
        type Item = Node<'t>;

        fn next(&mut self) -> Option<Node<'t>> {
            let node = self.stack.pop()?;
            // tree-sitter 0.27 takes a `u32` index while still counting in
            // `usize`; the cast is on the count so the conversion happens once
            // rather than per child. A node with more than 4 billion named
            // children is not a file this ever sees.
            for index in (0..node.named_child_count() as u32).rev() {
                if let Some(child) = node.named_child(index) {
                    self.stack.push(child);
                }
            }
            Some(node)
        }
    }

    fn pre_order(root: Node<'_>) -> PreOrder<'_> {
        PreOrder { stack: vec![root] }
    }

    /// How many places the grammar could not read, over *all* children rather
    /// than the named ones: a MISSING node is usually an anonymous token the
    /// grammar inserted to keep going.
    fn error_count(root: Node<'_>) -> usize {
        let mut total = 0usize;
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if node.is_error() || node.is_missing() {
                total += 1;
            }
            for index in (0..node.child_count()).rev() {
                if let Some(child) = node.child(index) {
                    stack.push(child);
                }
            }
        }
        total
    }

    /// Whether anything under `root` is an ERROR or MISSING node — the same
    /// walk as [`error_count`], stopping at the first hit.
    fn has_syntax_error(root: Node<'_>) -> bool {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if node.is_error() || node.is_missing() {
                return true;
            }
            for index in (0..node.child_count()).rev() {
                if let Some(child) = node.child(index) {
                    stack.push(child);
                }
            }
        }
        false
    }

    // -------------------------------------------------------------------- Python

    /// A slice of a file (`sed -n '120,180p' app.py`) starts mid-body, so every
    /// line is indented and the grammar rejects it at line 1. Re-reading it
    /// under a synthetic scope makes the same text parse without altering a
    /// byte of it or shifting any column — only line numbers move, by exactly
    /// the one line this adds.
    const SYNTHETIC_SCOPE: &str = "if True:\n";

    /// The largest chunk of a payload that parses, and the coordinates to read
    /// it in.
    struct Salvage<'a> {
        tree: Tree,
        /// `scope + head`, kept because the tree's byte offsets index it.
        parsed: String,
        scope: &'static str,
        /// The text that parsed, in the payload's own coordinates.
        head: &'a str,
        /// Everything after `head`, emitted verbatim.
        tail: &'a str,
    }

    /// Whether a Python tree parsed cleanly enough to trust.
    ///
    /// tree-sitter never fails outright: it answers a broken file with a tree
    /// full of ERROR nodes rather than raising, so "did this parse" has to be
    /// asked as "is the tree clean". That is more tolerant than a strict parser,
    /// and the tolerance works in this engine's favour — the grammar reads a
    /// uniformly indented mid-file slice as a clean module, where a strict
    /// parser rejects it at line 1, so those payloads are recovered at the first
    /// rung below rather than the second.
    ///
    /// The synthetic scope still earns its place on slices that open
    /// mid-construct — a chunk starting at an `elif`, say — which the grammar
    /// does refuse. Every edit is re-validated the same way regardless, which is
    /// what bounds the risk of trusting a tolerant parse.
    fn parse_python_clean(text: &str) -> Option<Tree> {
        let tree = parse(text, Language::Python)?;
        (!has_syntax_error(tree.root_node())).then_some(tree)
    }

    /// Leading line count of `source` that parses under `scope`.
    ///
    /// Found by bisection, which is deterministic but not exhaustive: Python
    /// validity is not monotone in line count (`def f():` alone is invalid
    /// while both a shorter and a longer prefix parse), so this returns *a*
    /// valid prefix rather than provably the longest. That is the right trade —
    /// the alternative is a linear reparse of every prefix of a 40K file, and a
    /// prefix that is merely long enough already recovers the bodies worth
    /// eliding.
    fn valid_prefix_lines(source: &str, scope: &str) -> usize {
        let lines = split_lines_keepends(source);
        let (mut low, mut high, mut best) = (0usize, lines.len(), 0usize);
        while low <= high {
            let middle = (low + high) / 2;
            let candidate = format!("{scope}{}", lines[..middle].concat());
            if parse_python_clean(&candidate).is_some() {
                best = middle;
                low = middle + 1;
            } else if middle == 0 {
                break;
            } else {
                high = middle - 1;
            }
        }
        best
    }

    /// Degrade in steps rather than give up.
    ///
    /// Tool output is not always a whole file, and the strict reading cost real
    /// coverage: of the Python payloads the engine declined on this corpus, 143
    /// (870K characters) failed to parse outright. They come in two
    /// recognisable shapes — mid-file slices, and files the harness truncated
    /// at its output cap with a docstring or bracket left open — and both are
    /// ordinary Python up to the cut. So: whole module, then the same text
    /// under a synthetic scope, then a parseable prefix of either.
    ///
    /// Nothing is ever dropped here. The unparsed tail is handed back to be
    /// re-attached untouched, so salvage only widens what may be *considered*;
    /// what is removed is still only a function body, still marked, still
    /// re-validated.
    fn salvage_python(source: &str) -> Option<Salvage<'_>> {
        if let Some(tree) = parse_python_clean(source) {
            return Some(Salvage {
                tree,
                parsed: source.to_string(),
                scope: "",
                head: source,
                tail: "",
            });
        }
        let scoped = format!("{SYNTHETIC_SCOPE}{source}");
        if let Some(tree) = parse_python_clean(&scoped) {
            return Some(Salvage {
                tree,
                parsed: scoped,
                scope: SYNTHETIC_SCOPE,
                head: source,
                tail: "",
            });
        }
        for scope in ["", SYNTHETIC_SCOPE] {
            let kept = valid_prefix_lines(source, scope);
            if kept == 0 {
                continue;
            }
            let lines = split_lines_keepends(source);
            let split_at: usize = lines[..kept].iter().map(|l| l.len()).sum();
            let (head, tail) = source.split_at(split_at);
            let parsed = format!("{scope}{head}");
            if let Some(tree) = parse_python_clean(&parsed) {
                return Some(Salvage {
                    tree,
                    parsed,
                    scope,
                    head,
                    tail,
                });
            }
        }
        None
    }

    /// Text of `node` within the buffer it was parsed from.
    ///
    /// `get` rather than an index: a byte range that is not a char boundary
    /// would panic, and a panic here is a 500 on a request that was only ever
    /// meant to save tokens.
    fn node_text<'a>(node: Node<'_>, buffer: &'a str) -> Option<&'a str> {
        buffer.get(node.start_byte()..node.end_byte())
    }

    /// How many unparseable points one payload may be resumed past.
    ///
    /// One is enough for what this exists for; the bound is what keeps a
    /// pathological file from recursing per line.
    const MAX_RESUMES: usize = 4;

    /// Lines to step over while looking for a point the parser can resume at.
    ///
    /// A middle truncation leaves exactly one spliced line, so this is small
    /// on purpose: every step costs a salvage, and salvage bisects.
    const MAX_SKIPPED_LINES: usize = 4;

    /// Compress the text a prefix salvage could not reach.
    ///
    /// `salvage_python` degrades to the largest parseable *prefix* and hands
    /// back the rest to be re-attached untouched. That is right when the tail
    /// is a few broken lines at the end of a cut-off file, and wrong when
    /// something unparseable sits in the *middle*: everything after it is
    /// ordinary source that nothing ever looks at again.
    ///
    /// Which is the shape agent harnesses actually deliver. codex truncates a
    /// tool result in the middle rather than at the end, splicing the two
    /// halves together inside a line and marking the join
    /// (`…1112 tokens truncated…`). The result is one invalid line with a
    /// valid file on either side. Measured on that exact payload: the prefix
    /// salvage elided 12 of 23 bodies and removed 45.5%, against 92.0% for the
    /// same text with the spliced line repaired — and the half left verbatim
    /// is as likely as not to hold what the reader was looking for.
    fn crush_python_tail(
        mut rest: &str,
        opts: &EngineOptions,
        refs: &mut RefCollector,
        depth: usize,
    ) -> String {
        let mut out = String::new();
        for _ in 0..=MAX_SKIPPED_LINES {
            if rest.is_empty() {
                return out;
            }
            if let Some(done) = crush_python_at(rest, opts, refs, depth) {
                out.push_str(&done);
                return out;
            }
            // The line that blocked the parse goes back exactly as it arrived,
            // and the search resumes after it.
            let cut = rest.find('\n').map_or(rest.len(), |at| at + 1);
            out.push_str(&rest[..cut]);
            rest = &rest[cut..];
        }
        out.push_str(rest);
        out
    }

    fn crush_python(source: &str, opts: &EngineOptions, refs: &mut RefCollector) -> Option<String> {
        crush_python_at(source, opts, refs, 0)
    }

    fn crush_python_at(
        source: &str,
        opts: &EngineOptions,
        refs: &mut RefCollector,
        depth: usize,
    ) -> Option<String> {
        let salvage = salvage_python(source)?;
        let lines = split_lines(salvage.head);
        // The synthetic scope shifts every row down by its own line count and
        // nothing else; columns and bytes are untouched.
        let shift = salvage.scope.matches('\n').count() as i64;

        // (first body line, one past the definition's last line, symbol,
        // definition column) — a half-open span of `lines`, in the payload's
        // own coordinates rather than the parsed buffer's.
        //
        // The threshold is a floor, not a fixed rule. A file whose bodies all
        // sit under it yields nothing at all, and that is not a rare shape: of
        // the eleven corpus payloads the code compressor returns whole, five
        // are Python files whose longest body is 24 lines or fewer. Rather than
        // lower the floor for everyone — which would elide short bodies in
        // files that already compress well — the pass runs twice, and the
        // second run only happens when the first found nothing.
        let mut threshold = opts.code_max_body_lines as i64;
        let mut candidates: Vec<(usize, usize, String, usize)> = Vec::new();
        for pass in 0..2 {
            if pass == 1 {
                if !candidates.is_empty() || !opts.enable_code_budget {
                    break;
                }
                // Second pass: the largest bodies in the file, whatever their
                // size. `code_budget_min_body_lines` is the point below which a
                // body is signature-and-a-line and eliding it would trade a
                // marker for nothing.
                threshold = opts.code_budget_min_body_lines as i64;
            }
            for node in pre_order(salvage.tree.root_node()) {
                if !matches!(node.kind(), "function_definition" | "class_definition") {
                    continue;
                }
                let Some(body) = node.child_by_field_name("body") else {
                    continue;
                };
                let end = node.end_position().row as i64 + 1 - shift;
                let body_start = body.start_position().row as i64 + 1 - shift;
                // inclusive of both ends: the body's first line through the
                // definition's last
                let body_lines = end - body_start + 1;
                if body_lines <= threshold {
                    continue;
                }
                if body_start < 1 || end > lines.len() as i64 {
                    // a span belonging to the synthetic scope rather than the payload
                    continue;
                }
                let name = node
                    .child_by_field_name("name")
                    .and_then(|n| node_text(n, &salvage.parsed))
                    .filter(|name| !name.is_empty())
                    .unwrap_or("body")
                    .to_string();
                candidates.push((
                    (body_start - 1) as usize,
                    end as usize,
                    name,
                    node.start_position().column,
                ));
            }
        }

        // Outermost first, so a class that is itself being elided claims its
        // methods rather than marking them twice.
        candidates.sort_by_key(|(start, end, _, _)| (*start, std::cmp::Reverse(*end)));
        let mut replacements: Vec<(usize, usize, String, String)> = Vec::new();
        let mut last_end: Option<usize> = None;
        for (start, end, name, column) in candidates {
            if last_end.is_some_and(|previous| start < previous) {
                continue;
            }
            let indent = " ".repeat(column + 4);
            let fragment = join_lines(&lines[start..end]);
            if refs.code_is_preserved(&fragment) {
                last_end = Some(end);
                continue;
            }
            let marker = code_marker(&name, end - start, &fragment, refs, "#");
            // A filler keeps the definition a definition: a body of nothing but
            // a comment is a syntax error, and the re-parse below would then
            // refuse every edit this path makes.
            //
            // `...` rather than `pass`, which this used to emit. Both parse, and
            // the difference is what they mean to a reader: `pass` is a body
            // that deliberately does nothing, so the stub reads as finished
            // code, while `...` is the convention for a body not shown — it is
            // what a `.pyi` stub file puts there. That mattered in a measured
            // failure: an agent reading `join_revisions` to decide how a
            // leaderboard aggregates scores got back the signature, the marker
            // and `pass`, never retrieved the 77 elided lines or the docstring
            // stating the rules, asserted the semantics and answered wrong.
            // Three characters shorter, too.
            replacements.push((
                start,
                end,
                format!("{indent}{marker}"),
                format!("{indent}..."),
            ));
            last_end = Some(end);
        }
        if replacements.is_empty() {
            // Nothing was elided. Rebuilding the file here would still look
            // like a win to a naive length test, because splitting lines drops
            // the trailing newline — one byte, no marker, and a payload edited
            // for nothing.
            return None;
        }

        let mut rebuilt: Vec<Cow<'_, str>> = Vec::with_capacity(lines.len());
        let mut cursor = 0usize;
        for (start, end, marker, keeper) in &replacements {
            rebuilt.extend(lines[cursor..*start].iter().map(|l| Cow::Borrowed(*l)));
            rebuilt.push(Cow::Borrowed(marker.as_str()));
            rebuilt.push(Cow::Borrowed(keeper.as_str()));
            cursor = *end;
        }
        rebuilt.extend(lines[cursor..].iter().map(|l| Cow::Borrowed(*l)));
        let mut result = join_lines(&rebuilt);
        if salvage.head.ends_with('\n') {
            // splitting lines dropped it, and without it the last kept line
            // would be glued onto the first line of the tail
            result.push('\n');
        }

        // Validity outranks ratio: an edit the grammar can no longer read is
        // discarded whole, however much it would have saved.
        parse_python_clean(&format!("{}{result}", salvage.scope))?;
        if depth < MAX_RESUMES {
            let tail = crush_python_tail(salvage.tail, opts, refs, depth + 1);
            result.push_str(&tail);
        } else {
            result.push_str(salvage.tail);
        }
        Some(result)
    }

    // ------------------------------------------------------------ brace languages

    fn function_kinds(language: Language) -> &'static [&'static str] {
        match language {
            Language::JavaScript | Language::TypeScript => &[
                "function_declaration",
                "method_definition",
                "arrow_function",
            ],
            Language::Go => &["function_declaration", "method_declaration"],
            Language::Rust => &["function_item", "function_signature_item"],
            Language::Java => &["method_declaration", "constructor_declaration"],
            Language::CSharp => &[
                "method_declaration",
                "constructor_declaration",
                "local_function_statement",
            ],
            // C and C++ spell every function — free, method, constructor,
            // operator — as one node type, and a C++ method defined inside its
            // class is still a function_definition, so the single entry covers
            // both languages.
            Language::C | Language::Cpp => &["function_definition"],
            Language::Python => &[],
        }
    }

    /// C and C++ hang the name off a declarator chain instead of a `name`
    /// field: `int *f(void)` nests pointer_declarator around function_declarator
    /// around the identifier, and a C++ method adds a qualified_identifier.
    /// Without this the marker reads "omitted from body", which tells the model
    /// nothing about what it can no longer see.
    const NAME_KINDS: [&str; 4] = [
        "identifier",
        "field_identifier",
        "qualified_identifier",
        "type_identifier",
    ];
    const MAX_DECLARATOR_DEPTH: usize = 8;

    fn declarator_name<'t>(node: Node<'t>) -> Option<Node<'t>> {
        let mut current = node.child_by_field_name("declarator");
        for _ in 0..MAX_DECLARATOR_DEPTH {
            let found = current?;
            if NAME_KINDS.contains(&found.kind()) {
                return Some(found);
            }
            current = found.child_by_field_name("declarator");
        }
        None
    }

    fn function_name(node: Node<'_>, source: &str) -> String {
        node.child_by_field_name("name")
            .or_else(|| declarator_name(node))
            .and_then(|n| node_text(n, source))
            .filter(|name| !name.is_empty())
            .unwrap_or("body")
            .to_string()
    }

    fn crush_braces(
        source: &str,
        opts: &EngineOptions,
        refs: &mut RefCollector,
        language: Language,
    ) -> Option<String> {
        let tree = parse(source, language)?;
        let before = error_count(tree.root_node());
        let kinds = function_kinds(language);

        let mut replacements: Vec<(usize, usize, String)> = Vec::new();
        for node in pre_order(tree.root_node()) {
            if !kinds.contains(&node.kind()) {
                continue;
            }
            let mut ancestor = node.parent();
            let mut nested = false;
            while let Some(current) = ancestor {
                if kinds.contains(&current.kind()) {
                    nested = true;
                    break;
                }
                ancestor = current.parent();
            }
            if nested {
                continue;
            }
            if has_syntax_error(node) {
                // This function alone is mis-parsed, so its extent cannot be
                // trusted; the rest of the file still can. Skipping the node
                // rather than abandoning the payload is what makes C worth
                // routing at all — a single unparseable macro used to discard
                // the whole file.
                continue;
            }
            let Some(body) = node.child_by_field_name("body") else {
                continue;
            };
            let Some(block) = node_text(body, source) else {
                continue;
            };
            let (Some(open), Some(close)) = (block.find('{'), block.rfind('}')) else {
                continue;
            };
            if close <= open {
                continue;
            }
            let hidden = &block[open + 1..close];
            if refs.code_is_preserved(hidden) {
                continue;
            }
            let count = hidden.matches('\n').count() + 1;
            if count <= opts.code_max_body_lines {
                continue;
            }
            let name = function_name(node, source);
            // Built here rather than through `code_marker`, whose shape is the
            // `#`-comment line form; the inner text is identical.
            let handle = match refs.stash(hidden) {
                Some(id) => format!("; retrieve: ref:{id}"),
                None => String::new(),
            };
            replacements.push((
                body.start_byte(),
                body.end_byte(),
                format!("{{/* [... {count} body lines omitted from {name}{handle}] */}}"),
            ));
        }
        if replacements.is_empty() {
            return None;
        }

        // Descending, so an earlier replacement cannot invalidate a later
        // offset.
        replacements.sort_by_key(|(start, _, _)| *start);
        let mut result = source.to_string();
        for (start, end, replacement) in replacements.iter().rev() {
            result.replace_range(*start..*end, replacement);
        }

        let reparsed = parse(&result, language)?;
        if error_count(reparsed.root_node()) > before {
            // The guard is "no worse than what arrived", not "clean".
            // Demanding a clean reparse is what kept C out: real C files
            // routinely defeat the grammar somewhere (preprocessor
            // conditionals, GNU extensions), and since that error is still in
            // the output it would fail every time — so the whole payload came
            // back even when every body elided came from a provably clean
            // subtree. Counting instead lets pre-existing damage through
            // untouched while refusing any edit that adds to it.
            return None;
        }
        Some(result)
    }

    /// Error nodes in `text` under `language`, for tests that need to assert
    /// what the grammar makes of a payload. `usize::MAX` stands for "would not
    /// parse at all", which no caller can confuse with an error count.
    #[cfg(test)]
    pub(super) fn probe_error_count(text: &str, language: Language) -> usize {
        match parse(text, language) {
            Some(tree) => error_count(tree.root_node()),
            None => usize::MAX,
        }
    }
}

#[cfg(all(test, feature = "code-aware"))]
mod tests {
    use super::imp::probe_error_count;
    use super::*;
    use crate::options::EngineOptions;
    use crate::text::chars_len;
    use crate::types::{Context, Language, Mode};

    /// Whether the grammar reads `text` without a single error node.
    fn parses(text: &str, language: Language) -> bool {
        probe_error_count(text, language) == 0
    }

    fn options() -> EngineOptions {
        EngineOptions::default()
    }

    fn run(content: &str, language: Language) -> Option<String> {
        let opts = options();
        let mut refs = opts.collector();
        crush_code(
            content,
            &opts,
            &Context::default(),
            &mut refs,
            Some(language),
        )
    }

    /// A Python function whose body is `body_lines` long, at column 0.
    fn python_function(name: &str, body_lines: usize) -> String {
        let mut out = format!("def {name}(value):\n");
        for i in 0..body_lines {
            out.push_str(&format!("    value = value + {i}\n"));
        }
        out.push_str("    return value\n");
        out
    }

    fn c_body(lines: usize) -> String {
        (0..lines)
            .map(|i| format!("    total += {i};\n"))
            .collect::<String>()
    }

    #[test]
    fn a_long_python_body_is_elided_and_the_result_still_parses() {
        let source = python_function("accumulate", 40);
        let out = run(&source, Language::Python).expect("a 40-line body is worth eliding");
        assert!(out.starts_with("def accumulate(value):\n"), "{out}");
        assert!(out.contains("body lines omitted from accumulate"), "{out}");
        assert!(
            out.contains("    ..."),
            "the definition keeps a body: {out}"
        );
        assert!(
            !out.contains("value = value + 39"),
            "the body is gone: {out}"
        );
        assert!(
            parses(&out, Language::Python),
            "the rewritten file must still parse: {out}"
        );
    }

    #[test]
    fn a_short_python_body_is_left_alone() {
        let source = python_function("small", 3);
        assert!(run(&source, Language::Python).is_none());
    }

    #[test]
    fn a_file_with_nothing_to_elide_is_declined_rather_than_rebuilt() {
        // Rebuilding would drop the trailing newline and pass a naive
        // "shorter than the input" test with no marker to show for it.
        let source = "import os\nimport sys\n\nVALUE = os.environ.get('X', sys.argv[0])\n";
        assert!(run(source, Language::Python).is_none());
    }

    #[test]
    fn the_harness_preamble_survives_verbatim() {
        let preamble =
            "Chunk ID: abc\nWall time: 1.2 seconds\nProcess exited with code 0\nOutput:\n";
        let source = format!("{preamble}{}", python_function("framed", 40));
        let out = run(&source, Language::Python).expect("the frame must not block the parse");
        assert!(out.starts_with(preamble), "{out}");
        assert!(out.contains("body lines omitted from framed"), "{out}");
    }

    #[test]
    fn a_mid_file_indented_slice_is_compressed_rather_than_refused() {
        // what `sed -n '40,90p' app.py` hands back: every line indented, which
        // is what `ast` rejected at line 1. This grammar reads it as a module
        // whose base indent happens to be 4, so it never reaches the synthetic
        // scope — the payload is recovered either way, which is the guarantee.
        let mut source = String::from("    self.count += 1\n    total = total + 1\n\n");
        source.push_str("    def helper(self):\n");
        for i in 0..40 {
            source.push_str(&format!("        step = step + {i}\n"));
        }
        source.push_str("        return step\n");

        let out = run(&source, Language::Python).expect("an indented slice is still Python");
        assert!(out.starts_with("    self.count += 1\n"), "{out}");
        assert!(out.contains("body lines omitted from helper"), "{out}");
        // no column moved, so the marker lands one level inside the def
        assert!(out.contains("\n        # [... "), "{out}");
    }

    #[test]
    fn a_slice_that_opens_mid_construct_is_salvaged_under_the_synthetic_scope() {
        // a chunk cut out of the middle of a module-level if/elif chain: the
        // grammar refuses a leading `elif`, and wrapping the same bytes in a
        // scope makes it parse without moving a single column
        let mut source = String::from("elif mode == \"fast\":\n    total = compute(total)\n\n\n");
        source.push_str("def helper(step):\n");
        for i in 0..40 {
            source.push_str(&format!("    step = step + {i}\n"));
        }
        source.push_str("    return step\n");

        assert!(
            !parses(&source, Language::Python),
            "the fixture must actually need the scope"
        );
        assert!(parses(&format!("if True:\n{source}"), Language::Python));

        let out = run(&source, Language::Python).expect("the scope rescues the slice");
        assert!(out.starts_with("elif mode == \"fast\":\n"), "{out}");
        assert!(out.contains("body lines omitted from helper"), "{out}");
        // the scope shifts line numbers by exactly one and nothing else: get
        // that wrong and the marker lands on the wrong line
        assert!(out.contains("\n    # [... "), "{out}");
        assert!(!out.contains("step = step + 39"), "{out}");
    }

    #[test]
    fn a_truncated_python_file_is_salvaged_by_prefix_with_the_tail_verbatim() {
        let mut source = python_function("complete", 40);
        source.push_str("\n\ndef cut_off():\n    \"\"\"the harness stopped mid-docstring\n");
        let tail = "def cut_off():\n    \"\"\"the harness stopped mid-docstring\n";

        // neither whole-module nor the synthetic scope can read this, so only
        // the prefix rung of the ladder can recover it
        assert!(!parses(&source, Language::Python));
        assert!(!parses(&format!("if True:\n{source}"), Language::Python));
        let out = run(&source, Language::Python).expect("the head is ordinary Python");
        assert!(out.contains("body lines omitted from complete"), "{out}");
        assert!(
            out.ends_with(tail),
            "the unparsed tail comes back untouched: {out}"
        );
    }

    #[test]
    fn a_body_after_a_middle_truncation_is_still_elided() {
        // What codex delivers when a tool result runs over its output cap: the
        // two halves spliced together inside a line, with the join marked. The
        // spliced line is unreadable, and a prefix salvage stops there — which
        // used to leave every body after it in the clear. Measured on the
        // retrieval fixture: 12 of 23 bodies and 45.5% removed, against 92.0%
        // once the splice was repaired by hand.
        let mut source = python_function("before_the_cut", 40);
        source.push_str("\n\n    step_24 = compute(x, 24, offset=264");
        source.push_str("\u{2026}1112 tokens truncated\u{2026}offset=140)\n\n");
        source.push_str(&python_function("after_the_cut", 40));

        let out = run(&source, Language::Python).expect("both halves are ordinary Python");
        assert!(
            out.contains("body lines omitted from before_the_cut"),
            "the half before the splice still compresses: {out}"
        );
        assert!(
            out.contains("body lines omitted from after_the_cut"),
            "the half after the splice is reached rather than emitted whole: {out}"
        );
        // Nothing is dropped: the line the parser could not read goes back as
        // it arrived.
        assert!(
            out.contains("\u{2026}1112 tokens truncated\u{2026}"),
            "the splice marker survives verbatim: {out}"
        );
    }

    #[test]
    fn a_method_inside_an_elided_class_is_not_marked_twice() {
        let mut source = String::from("class Big:\n");
        source.push_str("    def method(self):\n");
        for i in 0..40 {
            source.push_str(&format!("        x = {i}\n"));
        }
        let out = run(&source, Language::Python).expect("the class body is long");
        assert_eq!(
            out.matches("body lines omitted").count(),
            1,
            "the outermost span claims the nested one: {out}"
        );
        assert!(out.contains("omitted from Big"), "{out}");
    }

    #[test]
    fn a_c_function_body_is_elided_with_the_name_read_through_a_pointer_declarator() {
        let source = format!(
            "#include <stdlib.h>\n\nint *accumulate(int total) {{\n{}    return NULL;\n}}\n",
            c_body(40)
        );
        let out = run(&source, Language::C).expect("a 41-line body is worth eliding");
        assert!(
            out.contains("omitted from accumulate"),
            "the declarator chain must yield the name, not \"body\": {out}"
        );
        assert!(out.contains("/* [... "), "the marker is a C comment: {out}");
        assert!(
            out.contains("#include <stdlib.h>"),
            "structure survives: {out}"
        );
        assert!(!out.contains("total += 39"), "{out}");
    }

    #[test]
    fn a_cpp_method_name_resolves_through_a_qualified_identifier() {
        let source = format!(
            "#include <vector>\n\nvoid Widget::render(int total) {{\n{}}}\n",
            c_body(40)
        );
        let out = run(&source, Language::Cpp).expect("a 41-line body is worth eliding");
        assert!(out.contains("omitted from Widget::render"), "{out}");
    }

    #[test]
    fn a_file_that_already_fails_to_parse_keeps_its_clean_functions_elided() {
        // a line no C grammar can read, followed by a function that is fine
        let source = format!(
            "@@@ not c at all @@@\n\nint clean(int total) {{\n{}    return total;\n}}\n",
            c_body(40)
        );
        assert!(
            probe_error_count(&source, Language::C) > 0,
            "the fixture must actually defeat the grammar"
        );
        let out = run(&source, Language::C)
            .expect("pre-existing damage must not veto a provably clean subtree");
        assert!(out.contains("omitted from clean"), "{out}");
        assert!(
            out.contains("@@@ not c at all @@@"),
            "the bad line is untouched: {out}"
        );
    }

    #[test]
    fn an_edit_that_would_add_a_parse_error_is_refused() {
        // A body whose braces the grammar cannot balance is skipped rather than
        // replaced, so the payload either comes back clean or not at all.
        let source = format!(
            "int broken(int total) {{\n{}    if (total) {{ /* unclosed\n}}\n",
            c_body(40)
        );
        if let Some(out) = run(&source, Language::C) {
            assert!(
                probe_error_count(&out, Language::C) <= probe_error_count(&source, Language::C),
                "an accepted edit never adds a parse error: {out}"
            );
        }
    }

    #[test]
    fn every_brace_language_elides_its_own_function_shape() {
        let cases: [(Language, String); 4] = [
            (
                Language::Go,
                format!(
                    "package main\n\nfunc Accumulate(total int) int {{\n{}\treturn total\n}}\n",
                    (0..40)
                        .map(|i| format!("\ttotal += {i}\n"))
                        .collect::<String>()
                ),
            ),
            (
                Language::Rust,
                format!(
                    "pub fn accumulate(mut total: i64) -> i64 {{\n{}    total\n}}\n",
                    (0..40)
                        .map(|i| format!("    total += {i};\n"))
                        .collect::<String>()
                ),
            ),
            (
                Language::Java,
                format!(
                    "class A {{\n  int accumulate(int total) {{\n{}    return total;\n  }}\n}}\n",
                    (0..40)
                        .map(|i| format!("    total += {i};\n"))
                        .collect::<String>()
                ),
            ),
            (
                Language::JavaScript,
                format!(
                    "function accumulate(total) {{\n{}  return total;\n}}\n",
                    (0..40)
                        .map(|i| format!("  total += {i};\n"))
                        .collect::<String>()
                ),
            ),
        ];
        for (language, source) in cases {
            let out = run(&source, language)
                .unwrap_or_else(|| panic!("{} should elide", language.as_str()));
            assert!(
                out.contains("omitted from accumulate") || out.contains("omitted from Accumulate"),
                "{}: {out}",
                language.as_str()
            );
        }
    }

    #[test]
    fn a_language_with_no_grammar_is_left_alone() {
        let opts = options();
        let mut refs = opts.collector();
        let source = python_function("accumulate", 40);
        assert!(crush_code(&source, &opts, &Context::default(), &mut refs, None).is_none());
    }

    #[test]
    fn lossless_mode_declines_rather_than_dropping_a_body() {
        let mut opts = options();
        opts.mode = Mode::Lossless;
        let mut refs = opts.collector();
        let source = python_function("accumulate", 40);
        assert!(
            crush_code(
                &source,
                &opts,
                &Context::default(),
                &mut refs,
                Some(Language::Python)
            )
            .is_none(),
            "the one mode that promises to drop nothing must drop nothing"
        );

        // …and with the check switched off bodies are elided again, which is
        // the behaviour the recorded vectors were captured with.
        opts.strict_lossless = false;
        let mut refs = opts.collector();
        assert!(
            crush_code(
                &source,
                &opts,
                &Context::default(),
                &mut refs,
                Some(Language::Python)
            )
            .is_some()
        );
    }

    #[test]
    fn a_payload_over_the_byte_cap_is_declined() {
        let mut opts = options();
        opts.max_code_bytes = 64;
        let mut refs = opts.collector();
        let source = python_function("accumulate", 40);
        assert!(
            crush_code(
                &source,
                &opts,
                &Context::default(),
                &mut refs,
                Some(Language::Python)
            )
            .is_none()
        );
    }

    #[test]
    fn a_recoverable_run_mints_a_handle_and_an_aggressive_one_does_not() {
        let source = python_function("accumulate", 40);
        let out = run(&source, Language::Python).unwrap();
        assert!(out.contains("; retrieve: ref:k_"), "{out}");

        let mut opts = options();
        opts.mode = Mode::Aggressive;
        let mut refs = opts.collector();
        let out = crush_code(
            &source,
            &opts,
            &Context::default(),
            &mut refs,
            Some(Language::Python),
        )
        .unwrap();
        assert!(
            !out.contains("ref:"),
            "a disabled collector mints nothing: {out}"
        );
        assert!(out.contains("body lines omitted from accumulate"), "{out}");
    }

    #[test]
    fn the_output_is_never_longer_than_the_input() {
        let c = format!("int f(int t) {{\n{}    return t;\n}}\n", c_body(40));
        let payloads: [(Language, String); 4] = [
            (Language::Python, python_function("accumulate", 40)),
            (Language::Python, python_function("tiny", 1)),
            (Language::C, c),
            (Language::C, "int f(void) { return 0; }\n".to_string()),
        ];
        for (language, source) in payloads {
            if let Some(out) = run(&source, language) {
                assert!(
                    chars_len(&out) < chars_len(&source),
                    "{}: {} >= {}",
                    language.as_str(),
                    chars_len(&out),
                    chars_len(&source)
                );
            }
        }
    }
}
