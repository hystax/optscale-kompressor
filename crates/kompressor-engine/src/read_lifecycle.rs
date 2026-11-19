//! Which file Reads in a conversation are still showing what is on disk.
//!
//! An agent reads a file, edits it, and then carries the pre-edit bytes in its
//! context for the rest of the session — often the largest single payload there
//! and, worse, a wrong one: the model reasons about lines that no longer exist.
//! Replacing a superseded Read with a marker is therefore a correction as much
//! as a saving.
//!
//! The classification is purely positional: it reads tool names and paths out
//! of the message list and touches no filesystem. That keeps it deterministic
//! and correct in the case that matters, a sandbox whose files the service
//! cannot see at all.
//!
//! # It is deterministic but **not** prefix-monotonic
//!
//! An earlier version of this comment claimed the determinism was "what the
//! provider prompt cache depends on". It is not, and the two must not be
//! conflated. Determinism is *same input, same output*. What a prompt cache
//! depends on is the stronger property `crossturn` states and every other
//! decision in this engine respects: the output of messages `0..k` is
//! byte-identical whether or not message `k+1` exists.
//!
//! This module is the one place that breaks it. Marking a read `Stale` requires
//! a *later* write, so appending a turn rewrites an earlier turn — and a
//! rewrite inside a prefix the provider has already cached costs a re-read of
//! everything after it. That is why `proxy.frozen_prefix = "first_seen"`, which
//! lowers the frozen floor precisely so history can be compressed, withholds
//! this feature: at Opus rates, re-writing a 20,000-token suffix to remove a
//! 2,000-token stale read needs ~125 further turns to break even, against
//! sessions that run 20 to 70.
//!
//! Left to decide on every run, the replacement in
//! `messages::compress_messages` handles `MessageContent::Text` only, so it
//! cannot fire for tool results that walk to `Blocks` — which is most of
//! Anthropic's. The reach is narrower than the rule suggests.
//!
//! # Deciding once: the ledger
//!
//! [`decide_reads`] is the same forward look, for a caller that records what
//! it decides and replays it (`messages::ReadLedger`). A run takes decisions
//! only when told it may — on the one turn where the provider's cache is
//! already lost — and every other run acts on the record alone, which makes
//! the lifecycle prefix-monotonic between those turns. Under a ledger it also
//! reaches a tool result delivered as one block, and it is what lets
//! `first_seen` stop withholding the feature (`proxy.cold_recompact`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Whether a Read still reflects the file.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadState {
    Fresh,
    /// A later tool call in the same conversation wrote to the same path.
    Stale,
}

impl ReadState {
    pub fn as_str(self) -> &'static str {
        match self {
            ReadState::Fresh => "fresh",
            ReadState::Stale => "stale",
        }
    }
}

/// The per-message metadata the classifier needs, so it does not depend on the
/// full message type.
pub struct ReadMessage<'a> {
    pub tool_name: Option<&'a str>,
    pub path: Option<&'a str>,
    pub protected: bool,
    /// The tool reported a failure, so a write here changed nothing.
    pub failed: bool,
}

/// Tools whose result is a file's contents.
const READ_TOOLS: [&str; 2] = ["read", "read_file"];
/// Tools that change a file's contents.
const WRITE_TOOLS: [&str; 4] = ["edit", "write", "write_file", "apply_patch"];

/// How a write tool's result opens when the write did not happen.
///
/// The OpenAI families carry no error flag on a tool result, so the result's
/// own first line is the only evidence. Claude Code's `<tool_use_error>` and
/// codex's `apply_patch verification failed` are the spellings measured; the
/// rest are the generic ones harnesses prefix an exception with.
const FAILURE_PREFIXES: [&str; 6] = [
    "error",
    "<tool_use_error>",
    "failed",
    "failure",
    "fatal",
    "apply_patch verification failed",
];

/// Whether a tool result says the call failed.
///
/// Read off the first non-blank line only: a successful write that echoes
/// file content containing the word "error" further down is still a write.
pub fn reports_failure(result: &str) -> bool {
    let Some(first) = result.lines().map(str::trim).find(|line| !line.is_empty()) else {
        return false;
    };
    let first = first.to_ascii_lowercase();
    FAILURE_PREFIXES
        .iter()
        .any(|prefix| first.starts_with(prefix))
}

/// Compare two paths the way an agent harness spells them.
///
/// The same file arrives as `./src/a.rs`, `src/a.rs`, `src\a.rs` and
/// `src//a.rs` within one conversation, depending on which tool reported it, so
/// a byte comparison would miss most write-after-read pairs.
///
/// Only spellings of the same path are folded: separators, empty components
/// and `.` components. A leading dot is part of a name (`.env` is not `env`),
/// `..` leaves the directory, and a leading `/` is the root, not the working
/// directory — stripping any of those made a write to one file mark a read of
/// another stale. `..` is kept rather than resolved, because through a symlink
/// `a/..` is not where `a` came from.
pub fn normalize_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    let absolute = path.starts_with('/');
    let parts: Vec<&str> = path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// Classify every Read in `messages` as fresh or stale.
///
/// The returned map holds only Read messages, keyed by their index.
///
/// A Read inside the frozen prefix, or one the caller flagged protected, stays
/// fresh no matter what follows it: those bytes are what the provider's prompt
/// cache is holding, and rewriting them invalidates that prefix and everything
/// after it — a cache miss on the whole conversation costs far more than the
/// stale payload does.
pub fn classify_reads(
    messages: &[ReadMessage],
    frozen_message_count: usize,
) -> BTreeMap<usize, ReadState> {
    let mut reads: Vec<(usize, String, bool)> = Vec::new();
    let mut writes: Vec<(usize, String)> = Vec::new();

    for (index, message) in messages.iter().enumerate() {
        let Some(path) = message.path else { continue };
        let tool = message.tool_name.unwrap_or("").to_ascii_lowercase();
        let path = normalize_path(path);
        if READ_TOOLS.contains(&tool.as_str()) {
            let protected = index < frozen_message_count || message.protected;
            reads.push((index, path, protected));
        } else if WRITE_TOOLS.contains(&tool.as_str()) && !message.failed {
            // A failed write left the file as it was: the read before it is
            // still the truth, and the one the agent needs to retry.
            writes.push((index, path));
        }
    }

    reads
        .into_iter()
        .map(|(index, path, protected)| {
            let stale = !protected
                && writes
                    .iter()
                    .any(|(write_index, write_path)| *write_index > index && *write_path == path);
            (
                index,
                if stale {
                    ReadState::Stale
                } else {
                    ReadState::Fresh
                },
            )
        })
        .collect()
}

/// What a ledger says became of a read. Ordered by which wins when more than
/// one applies: a read a later edit invalidated is wrong, which matters more
/// than its being repeated, which matters more than its being old.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadDecision {
    /// Nothing has touched the file for a while: compress the read as source.
    Matured,
    /// A later read of the same file contains this one: replace it.
    Superseded,
    /// A later write changed the file: replace it.
    Stale,
}

impl ReadDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            ReadDecision::Matured => "matured",
            ReadDecision::Superseded => "superseded",
            ReadDecision::Stale => "stale",
        }
    }
}

/// When an untouched read counts as finished with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Maturity {
    /// Assistant turns since anything read or wrote the file.
    pub quiesce_turns: u32,
    /// Assistant turns since the read itself, however recently the file was
    /// touched again.
    pub max_hold_turns: u32,
    /// The smallest read worth the decision.
    pub min_bytes: usize,
}

/// What a deciding run may decide, beyond staleness.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ReadPolicy {
    /// Replace a read a later read of the same file contains.
    pub superseded: bool,
    /// Compress a read nothing has touched for this long.
    pub matured: Option<Maturity>,
}

/// One message, as [`decide_reads`] needs it.
pub struct LedgerMessage<'a> {
    pub tool_name: Option<&'a str>,
    pub path: Option<&'a str>,
    /// The caller said not to touch it.
    pub protected: bool,
    /// The tool reported a failure: a write that changed nothing, or a read
    /// that returned no file.
    pub failed: bool,
    /// The lines a read asked for, `[first, past)`.
    pub range: Option<(u64, u64)>,
    /// The model's own turn, which is what time is counted in.
    pub assistant: bool,
    /// The size of the result.
    pub bytes: usize,
}

/// Decide, over a whole conversation, which reads are stale, superseded or
/// matured. Only reads with a decision are returned, keyed by index.
///
/// Like [`classify_reads`] this looks *forward* from each read, so its answer
/// for a message changes as the conversation grows. It is for a run that is
/// allowed to rewrite history — the one turn on which the provider's cache is
/// already lost — and its answers are then recorded and replayed, never taken
/// again on a turn where they would cost a cached prefix.
///
/// A read is **superseded** when a later read of the same file, itself
/// successful, asked for a range that contains this one's. Both ranges must be
/// known: a read whose selection is not understood covers nothing and is
/// covered by nothing. With no write in between, the later result holds every
/// line this one does. (With a write after it, the read is stale, which wins.)
///
/// A read is **matured** when it is large enough and either the file has gone
/// untouched — not read, not written — for `quiesce_turns` assistant turns, or
/// the read itself is `max_hold_turns` old.
pub fn decide_reads(
    messages: &[LedgerMessage],
    policy: &ReadPolicy,
) -> BTreeMap<usize, ReadDecision> {
    struct Read {
        index: usize,
        path: String,
        range: Option<(u64, u64)>,
        bytes: usize,
        protected: bool,
    }
    let mut reads: Vec<Read> = Vec::new();
    let mut writes: Vec<(usize, String)> = Vec::new();
    // Assistant turns strictly before each message.
    let mut turns_before: Vec<u32> = Vec::with_capacity(messages.len());
    let mut turns = 0u32;

    for (index, message) in messages.iter().enumerate() {
        turns_before.push(turns);
        if message.assistant {
            turns += 1;
        }
        let Some(path) = message.path else { continue };
        if message.failed {
            continue;
        }
        let tool = message.tool_name.unwrap_or("").to_ascii_lowercase();
        let path = normalize_path(path);
        if READ_TOOLS.contains(&tool.as_str()) {
            reads.push(Read {
                index,
                path,
                range: message.range,
                bytes: message.bytes,
                protected: message.protected,
            });
        } else if WRITE_TOOLS.contains(&tool.as_str()) {
            writes.push((index, path));
        }
    }

    let mut decided = BTreeMap::new();
    for read in reads.iter().filter(|read| !read.protected) {
        if writes
            .iter()
            .any(|(index, path)| *index > read.index && *path == read.path)
        {
            decided.insert(read.index, ReadDecision::Stale);
            continue;
        }
        let later = || {
            reads
                .iter()
                .filter(|other| other.index > read.index && other.path == read.path)
        };
        let contained = |other: &Read| match (read.range, other.range) {
            (Some((first, past)), Some((other_first, other_past))) => {
                other_first <= first && past <= other_past
            }
            _ => false,
        };
        if policy.superseded && later().any(contained) {
            decided.insert(read.index, ReadDecision::Superseded);
            continue;
        }
        let Some(maturity) = policy.matured else {
            continue;
        };
        if read.bytes < maturity.min_bytes {
            continue;
        }
        // No write follows it, or it would be stale: the last touch is a read.
        let last_touch = later().map(|other| other.index).max().unwrap_or(read.index);
        let quiet = turns - turns_before[last_touch];
        let held = turns - turns_before[read.index];
        if quiet >= maturity.quiesce_turns || held >= maturity.max_hold_turns {
            decided.insert(read.index, ReadDecision::Matured);
        }
    }
    decided
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg<'a>(tool: &'a str, path: &'a str) -> ReadMessage<'a> {
        ReadMessage {
            tool_name: Some(tool),
            path: Some(path),
            protected: false,
            failed: false,
        }
    }

    /// An ordinary conversation turn: no tool, no path.
    fn plain<'a>() -> ReadMessage<'a> {
        ReadMessage {
            tool_name: None,
            path: None,
            protected: false,
            failed: false,
        }
    }

    #[test]
    fn a_read_followed_by_a_write_to_the_same_file_is_stale() {
        let messages = [msg("Read", "src/a.rs"), msg("Edit", "src/a.rs")];
        let states = classify_reads(&messages, 0);
        assert_eq!(states.get(&0), Some(&ReadState::Stale));
        assert_eq!(states.len(), 1, "only reads are classified");
    }

    #[test]
    fn a_write_before_the_read_leaves_it_fresh() {
        let messages = [msg("Write", "src/a.rs"), msg("Read", "src/a.rs")];
        assert_eq!(
            classify_reads(&messages, 0).get(&1),
            Some(&ReadState::Fresh)
        );
    }

    #[test]
    fn a_write_to_a_different_file_leaves_the_read_fresh() {
        let messages = [msg("read", "src/a.rs"), msg("edit", "src/b.rs")];
        assert_eq!(
            classify_reads(&messages, 0).get(&0),
            Some(&ReadState::Fresh)
        );
    }

    #[test]
    fn every_writing_tool_invalidates_a_read() {
        for tool in ["edit", "write", "write_file", "apply_patch", "Apply_Patch"] {
            let messages = [msg("read_file", "a.rs"), msg(tool, "a.rs")];
            assert_eq!(
                classify_reads(&messages, 0).get(&0),
                Some(&ReadState::Stale),
                "{tool} should invalidate"
            );
        }
    }

    #[test]
    fn a_tool_that_is_neither_read_nor_write_is_ignored() {
        let messages = [msg("Bash", "src/a.rs"), msg("grep", "src/a.rs")];
        assert!(classify_reads(&messages, 0).is_empty());
    }

    #[test]
    fn a_message_without_a_path_is_ignored() {
        let messages = [
            plain(),
            ReadMessage {
                tool_name: Some("read"),
                path: None,
                protected: false,
                failed: false,
            },
        ];
        assert!(classify_reads(&messages, 0).is_empty());
    }

    #[test]
    fn a_read_inside_the_frozen_prefix_is_never_rewritten() {
        let messages = [msg("read", "src/a.rs"), msg("edit", "src/a.rs")];
        // index 0 is inside the first message of the frozen prefix
        assert_eq!(
            classify_reads(&messages, 1).get(&0),
            Some(&ReadState::Fresh)
        );
        assert_eq!(
            classify_reads(&messages, 0).get(&0),
            Some(&ReadState::Stale)
        );
    }

    #[test]
    fn a_protected_read_is_never_rewritten() {
        let messages = [
            ReadMessage {
                tool_name: Some("read"),
                path: Some("src/a.rs"),
                protected: true,
                failed: false,
            },
            msg("edit", "src/a.rs"),
        ];
        assert_eq!(
            classify_reads(&messages, 0).get(&0),
            Some(&ReadState::Fresh)
        );
    }

    #[test]
    fn the_same_file_spelled_differently_is_still_the_same_file() {
        let messages = [msg("Read", "./src/a.rs"), msg("Edit", "src\\a.rs")];
        assert_eq!(
            classify_reads(&messages, 0).get(&0),
            Some(&ReadState::Stale)
        );
    }

    #[test]
    fn only_the_reads_before_the_write_go_stale() {
        let messages = [
            msg("read", "a.rs"),
            msg("edit", "a.rs"),
            msg("read", "a.rs"),
        ];
        let states = classify_reads(&messages, 0);
        assert_eq!(states.get(&0), Some(&ReadState::Stale));
        assert_eq!(
            states.get(&2),
            Some(&ReadState::Fresh),
            "the re-read is current"
        );
    }

    #[test]
    fn normalisation_folds_the_spellings_a_harness_produces() {
        assert_eq!(normalize_path("src\\a.rs"), "src/a.rs");
        assert_eq!(normalize_path("./src/a.rs"), "src/a.rs");
        assert_eq!(normalize_path("././src/a.rs"), "src/a.rs");
        assert_eq!(normalize_path("src//a.rs"), "src/a.rs");
        assert_eq!(normalize_path("src/./a.rs"), "src/a.rs");
        assert_eq!(normalize_path("/src//a.rs"), "/src/a.rs");
        assert_eq!(normalize_path("src/a.rs"), "src/a.rs");
    }

    #[test]
    fn normalisation_keeps_what_names_a_different_file() {
        // A leading dot is part of the name, `..` leaves the directory, and a
        // leading slash is the root rather than the working directory.
        assert_eq!(normalize_path(".env"), ".env");
        assert_eq!(normalize_path("./.env"), ".env");
        assert_eq!(normalize_path("../src/a.rs"), "../src/a.rs");
        assert_eq!(normalize_path("/etc/config"), "/etc/config");
        // Not resolved: through a symlink, `a/..` is not where `a` came from.
        assert_eq!(normalize_path("a/../b.rs"), "a/../b.rs");
    }

    #[test]
    fn a_write_to_a_different_file_with_a_similar_spelling_leaves_the_read_fresh() {
        for (read, write) in [
            (".env", "env"),
            ("src/a.rs", "../src/a.rs"),
            ("/etc/config", "etc/config"),
        ] {
            let messages = [msg("Read", read), msg("Write", write)];
            assert_eq!(
                classify_reads(&messages, 0).get(&0),
                Some(&ReadState::Fresh),
                "a write to {write} says nothing about {read}"
            );
        }
    }

    #[test]
    fn a_failed_write_leaves_the_read_fresh() {
        let messages = [
            msg("read", "src/a.rs"),
            ReadMessage {
                tool_name: Some("edit"),
                path: Some("src/a.rs"),
                protected: false,
                failed: true,
            },
        ];
        assert_eq!(
            classify_reads(&messages, 0).get(&0),
            Some(&ReadState::Fresh)
        );
    }

    #[test]
    fn a_failure_is_read_off_the_first_line_only() {
        assert!(reports_failure("ERROR: permission denied; file unchanged"));
        assert!(reports_failure(
            "\n  <tool_use_error>File has not been read yet."
        ));
        assert!(reports_failure(
            "apply_patch verification failed: no such hunk"
        ));
        assert!(!reports_failure(
            "The file a.rs has been updated.\nerror: kept"
        ));
        assert!(!reports_failure("ok"));
        assert!(!reports_failure(""));
    }

    #[test]
    fn classification_is_stable_across_runs() {
        let messages = [
            msg("read", "a.rs"),
            msg("read", "b.rs"),
            msg("edit", "b.rs"),
        ];
        let once = classify_reads(&messages, 0);
        let twice = classify_reads(&messages, 0);
        assert_eq!(once, twice);
        assert_eq!(
            once.keys().copied().collect::<Vec<_>>(),
            vec![0, 1],
            "keys come back in message order"
        );
    }

    fn turn<'a>() -> LedgerMessage<'a> {
        LedgerMessage {
            tool_name: None,
            path: None,
            protected: false,
            failed: false,
            range: None,
            assistant: true,
            bytes: 0,
        }
    }

    fn call<'a>(tool: &'a str, path: &'a str, range: Option<(u64, u64)>) -> LedgerMessage<'a> {
        LedgerMessage {
            tool_name: Some(tool),
            path: Some(path),
            protected: false,
            failed: false,
            range,
            assistant: false,
            bytes: 10_000,
        }
    }

    const WHOLE: Option<(u64, u64)> = Some((1, 2_001));
    const BOTH: ReadPolicy = ReadPolicy {
        superseded: true,
        matured: Some(Maturity {
            quiesce_turns: 5,
            max_hold_turns: 25,
            min_bytes: 2_048,
        }),
    };
    const SUPERSEDED_ONLY: ReadPolicy = ReadPolicy {
        superseded: true,
        matured: None,
    };

    fn first(messages: &[LedgerMessage], policy: &ReadPolicy) -> Option<ReadDecision> {
        decide_reads(messages, policy).get(&0).copied()
    }

    #[test]
    fn a_later_read_supersedes_only_what_it_contains() {
        let pair = |earlier, later| [call("Read", "a.rs", earlier), call("Read", "a.rs", later)];
        let superseded = Some(ReadDecision::Superseded);
        // Whole covers whole and any part; a part covers a part inside it.
        assert_eq!(first(&pair(WHOLE, WHOLE), &SUPERSEDED_ONLY), superseded);
        assert_eq!(
            first(&pair(Some((40, 60)), WHOLE), &SUPERSEDED_ONLY),
            superseded
        );
        assert_eq!(
            first(&pair(Some((40, 60)), Some((30, 80))), &SUPERSEDED_ONLY),
            superseded
        );
        assert_eq!(
            first(&pair(Some((40, 60)), Some((40, 60))), &SUPERSEDED_ONLY),
            superseded
        );
        // A part does not cover the whole, nor a range it only overlaps.
        assert_eq!(first(&pair(WHOLE, Some((40, 60))), &SUPERSEDED_ONLY), None);
        assert_eq!(
            first(&pair(Some((40, 60)), Some((50, 90))), &SUPERSEDED_ONLY),
            None
        );
        // A selection that is not understood covers nothing, and is covered
        // by nothing.
        assert_eq!(first(&pair(Some((40, 60)), None), &SUPERSEDED_ONLY), None);
        assert_eq!(first(&pair(None, WHOLE), &SUPERSEDED_ONLY), None);
        // The later read is never the superseded one.
        assert_eq!(decide_reads(&pair(WHOLE, WHOLE), &SUPERSEDED_ONLY).len(), 1);
    }

    #[test]
    fn superseding_needs_the_same_file_a_successful_read_and_the_setting() {
        let other_file = [call("Read", "a.rs", WHOLE), call("Read", "b.rs", WHOLE)];
        assert_eq!(first(&other_file, &SUPERSEDED_ONLY), None);

        let mut failed = [call("Read", "a.rs", WHOLE), call("Read", "a.rs", WHOLE)];
        failed[1].failed = true;
        assert_eq!(
            first(&failed, &SUPERSEDED_ONLY),
            None,
            "the second read returned no file"
        );

        let again = [call("Read", "a.rs", WHOLE), call("Read", "a.rs", WHOLE)];
        assert_eq!(
            first(&again, &ReadPolicy::default()),
            None,
            "off unless asked for"
        );

        let mut protected = [call("Read", "a.rs", WHOLE), call("Read", "a.rs", WHOLE)];
        protected[0].protected = true;
        assert_eq!(first(&protected, &SUPERSEDED_ONLY), None);

        // The spellings findings.md #4 found colliding stay different files.
        for (read, reread) in [
            (".env", "env"),
            ("src/a.rs", "../src/a.rs"),
            ("/etc/x", "etc/x"),
        ] {
            let pair = [call("Read", read, WHOLE), call("Read", reread, WHOLE)];
            assert_eq!(first(&pair, &BOTH), None, "{read} and {reread}");
        }
    }

    #[test]
    fn a_write_makes_the_read_before_it_stale_whatever_else_is_true() {
        // Read, edit, read again: the first is stale, not superseded.
        let between = [
            call("Read", "a.rs", WHOLE),
            call("Edit", "a.rs", None),
            call("Read", "a.rs", WHOLE),
        ];
        let decided = decide_reads(&between, &BOTH);
        assert_eq!(decided.get(&0), Some(&ReadDecision::Stale));
        assert_eq!(decided.get(&2), None, "the read after the edit is current");

        // A write that failed changes nothing: the re-read still supersedes.
        let mut failed = [
            call("Read", "a.rs", WHOLE),
            call("Edit", "a.rs", None),
            call("Read", "a.rs", WHOLE),
        ];
        failed[1].failed = true;
        assert_eq!(first(&failed, &BOTH), Some(ReadDecision::Superseded));
    }

    /// A read followed by `quiet` assistant turns in which nothing touches it.
    fn left_alone(quiet: usize) -> Vec<LedgerMessage<'static>> {
        let mut messages = vec![call("Read", "a.rs", WHOLE)];
        messages.extend((0..quiet).map(|_| turn()));
        messages
    }

    #[test]
    fn a_read_matures_once_its_file_has_gone_quiet() {
        assert_eq!(first(&left_alone(4), &BOTH), None);
        assert_eq!(first(&left_alone(5), &BOTH), Some(ReadDecision::Matured));

        let mut small = left_alone(9);
        small[0].bytes = 2_047;
        assert_eq!(
            first(&small, &BOTH),
            None,
            "too small to be worth a decision"
        );
        assert_eq!(
            first(&left_alone(9), &SUPERSEDED_ONLY),
            None,
            "off unless asked for"
        );

        // Only assistant turns count: tool results and user text are not time.
        let mut padded = left_alone(4);
        padded.extend((0..10).map(|_| call("Bash", "", None)));
        assert_eq!(first(&padded, &BOTH), None);
    }

    #[test]
    fn touching_the_file_again_restarts_the_wait_until_the_hold_runs_out() {
        // Read, four turns, a partial re-read, four more turns: quiet is four.
        let mut touched = left_alone(4);
        touched.push(call("Read", "a.rs", Some((10, 20))));
        touched.extend((0..4).map(|_| turn()));
        let decided = decide_reads(&touched, &BOTH);
        assert_eq!(decided.get(&0), None, "the file was touched four turns ago");
        assert_eq!(decided.get(&5), None, "and the re-read is small and fresh");

        // One more turn and both have been quiet for five.
        touched.push(turn());
        assert_eq!(first(&touched, &BOTH), Some(ReadDecision::Matured));

        // Touched every third turn it never goes quiet, and is released
        // anyway once the read is twenty-five turns old.
        let mut busy = vec![call("Read", "a.rs", WHOLE)];
        for round in 0..8 {
            busy.extend((0..3).map(|_| turn()));
            busy.push(call("Read", "a.rs", Some((round, round + 5))));
        }
        assert_eq!(
            first(&busy, &BOTH),
            None,
            "twenty-four turns held, three quiet"
        );
        busy.push(turn());
        assert_eq!(first(&busy, &BOTH), Some(ReadDecision::Matured));
    }

    #[test]
    fn the_stronger_decision_wins() {
        assert!(ReadDecision::Stale > ReadDecision::Superseded);
        assert!(ReadDecision::Superseded > ReadDecision::Matured);
        // Old enough to mature and contained by a later read: superseded.
        let mut both = left_alone(6);
        both.push(call("Read", "a.rs", WHOLE));
        assert_eq!(first(&both, &BOTH), Some(ReadDecision::Superseded));
    }
}
