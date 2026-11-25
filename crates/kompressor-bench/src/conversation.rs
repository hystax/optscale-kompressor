//! Conversation-shaped replay, for the compressor the payload corpus cannot reach.
//!
//! # Why this exists separately from [`crate::corpus`]
//!
//! `cross_turn` folds a message against *earlier messages in the same
//! conversation*. The payload corpus is one payload per line and
//! [`mod@crate::replay`] compresses each independently, so the `SeenText` that
//! cross-turn folding reads is always empty there — the compressor is
//! unreachable by construction, not merely unrepresented.
//!
//! That mattered more than it sounded. Pooled over 67 Harbor runs, `cross_turn`
//! is the **second largest real compressor** (13,820 compressions), and on the
//! 2026-10-09 run it was the largest outright at 427. The thing doing most of
//! the work had no quality floor at all, while the shipped corpus spent 16
//! payloads each on `diff` and `html` — types that together account for a few
//! dozen compressions in thousands.
//!
//! So a conversation is the unit here: an ordered list of messages, replayed
//! through [`compress_messages`] so the fold sees what it would see in
//! production.
//!
//! # What this does not do
//!
//! It scores the conversation as a whole, not each message, because that is the
//! shape of the thing being measured: a fold's saving belongs to the pair, and
//! attributing it to the second message would make the first look free.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use kompressor_engine::messages::{Message, RequestOptions, compress_messages};
use kompressor_engine::{EngineOptions, RefCollector};
use serde::{Deserialize, Serialize};

use crate::corpus::BenchError;

/// Format of `conversations.jsonl.gz`, bumped when the shape changes.
pub const CONVERSATION_VERSION: u32 = 1;

/// How many conversations `fixtures/conversations.jsonl.gz` holds.
///
/// Stated once, for the same reason [`crate::COMMITTED_CORPUS_PAYLOADS`] is:
/// changing the fixture without noticing fails here rather than quietly
/// measuring a different thing under the same name.
pub const COMMITTED_CONVERSATIONS: usize = 3;

/// Cross-turn folds the committed fixture is known to produce.
///
/// A floor, not a target. It exists because "the corpus exercises cross-turn"
/// is the one claim this whole module makes, and a change that quietly stops it
/// folding would otherwise still pass every test here — the conversations would
/// replay, characters would come off by other compressors, and nothing would
/// say the thing being measured had gone. Measured 2026-10-09 at 42 (19/13/10).
pub const COMMITTED_CROSS_TURN_FOLDS: u32 = 42;

/// One message as the conversation recorded it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Turn {
    pub role: String,
    pub text: String,
}

/// An ordered conversation, as one request carried it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Conversation {
    /// The proxy's conversation identity, so a conversation can be followed
    /// across runs.
    pub id: String,
    /// Task it was captured from, for stratification.
    #[serde(default)]
    pub task: String,
    /// The provider the conversation was sent to (`anthropic`, `openai`),
    /// where the capture said. What [`crate::economics`] prices it at.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    /// The model it was sent to, where the capture said.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    pub turns: Vec<Turn>,
}

#[derive(Serialize, Deserialize)]
struct Header {
    conversation_version: u32,
}

/// Read a conversation corpus.
pub fn load_conversations(path: &Path) -> Result<Vec<Conversation>, BenchError> {
    let file = std::fs::File::open(path)
        .map_err(|source| BenchError::Read(path.display().to_string(), source))?;
    let reader: Box<dyn BufRead> = if path.extension().is_some_and(|e| e == "gz") {
        Box::new(BufReader::new(GzDecoder::new(file)))
    } else {
        Box::new(BufReader::new(file))
    };

    let mut out = Vec::new();
    for (number, line) in reader.lines().enumerate() {
        let line = line.map_err(|source| BenchError::Read(path.display().to_string(), source))?;
        if line.trim().is_empty() {
            continue;
        }
        // The first line is the version header, and a file without one is a
        // file from before this format existed rather than a corrupt one.
        if number == 0
            && let Ok(header) = serde_json::from_str::<Header>(&line)
        {
            if header.conversation_version != CONVERSATION_VERSION {
                return Err(BenchError::Empty(format!(
                    "{}: conversation format {}, expected {CONVERSATION_VERSION}",
                    path.display(),
                    header.conversation_version
                )));
            }
            continue;
        }
        out.push(
            serde_json::from_str(&line).map_err(|source| BenchError::Line {
                path: path.display().to_string(),
                line: number + 1,
                source,
            })?,
        );
    }
    Ok(out)
}

/// Write a conversation corpus, gzipped when the name says so.
pub fn write_conversations(path: &Path, conversations: &[Conversation]) -> Result<(), BenchError> {
    let file = std::fs::File::create(path)
        .map_err(|source| BenchError::Write(path.display().to_string(), source))?;
    let mut out: Box<dyn Write> = if path.extension().is_some_and(|e| e == "gz") {
        Box::new(GzEncoder::new(file, Compression::default()))
    } else {
        Box::new(file)
    };
    let header = serde_json::to_string(&Header {
        conversation_version: CONVERSATION_VERSION,
    })
    .unwrap_or_else(|_| format!("{{\"conversation_version\":{CONVERSATION_VERSION}}}"));
    let write = |out: &mut Box<dyn Write>, line: &str| -> Result<(), BenchError> {
        writeln!(out, "{line}")
            .map_err(|source| BenchError::Write(path.display().to_string(), source))
    };
    write(&mut out, &header)?;
    for conversation in conversations {
        let line = serde_json::to_string(conversation).map_err(|source| BenchError::Line {
            path: path.display().to_string(),
            line: 0,
            source,
        })?;
        write(&mut out, &line)?;
    }
    out.flush()
        .map_err(|source| BenchError::Write(path.display().to_string(), source))
}

/// What replaying one conversation did.
#[derive(Clone, Debug)]
pub struct ConversationResult {
    pub id: String,
    pub turns: usize,
    pub chars_in: usize,
    pub chars_out: usize,
    /// Messages the engine folded against an earlier one.
    pub cross_turn: u32,
    /// Every compressor that fired, so a change that trades cross-turn folding
    /// for something else is visible rather than hidden in the total.
    pub by_type: Vec<(String, u32)>,
}

impl ConversationResult {
    /// Share of characters removed across the whole conversation.
    pub fn removed(&self) -> f64 {
        if self.chars_in == 0 {
            return 0.0;
        }
        1.0 - (self.chars_out as f64 / self.chars_in as f64)
    }
}

/// Replay conversations through the message loop, where cross-turn folding lives.
pub fn replay_conversations(
    conversations: &[Conversation],
    options: &EngineOptions,
) -> Vec<ConversationResult> {
    conversations
        .iter()
        .map(|conversation| replay_one(conversation, options))
        .collect()
}

fn replay_one(conversation: &Conversation, options: &EngineOptions) -> ConversationResult {
    let mut messages: Vec<Message> = conversation
        .turns
        .iter()
        .map(|turn| Message::text(turn.role.clone(), turn.text.clone()))
        .collect();
    let mut refs = RefCollector::new(options.max_ref_bytes);
    let stats = compress_messages(
        &mut messages,
        options,
        &RequestOptions::default(),
        &mut refs,
    );
    let mut by_type: Vec<(String, u32)> = stats
        .by_type
        .iter()
        .map(|(kind, count)| (kind.clone(), *count))
        .collect();
    by_type.sort();
    ConversationResult {
        id: conversation.id.clone(),
        turns: conversation.turns.len(),
        chars_in: stats.chars_in as usize,
        chars_out: stats.chars_out as usize,
        cross_turn: stats.by_type.get("cross_turn").copied().unwrap_or(0),
        by_type,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<Conversation> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/conversations.jsonl.gz");
        load_conversations(&path).expect("the committed conversation fixture")
    }

    /// The compressor the payload corpus cannot reach, actually reached.
    ///
    /// This is the whole point of the conversation corpus: `cross_turn` folds a
    /// message against earlier ones, so replaying payloads independently leaves
    /// it with nothing to fold against and it never fires. If this count is
    /// ever zero, the corpus has stopped exercising the thing it exists for —
    /// which is a defect in the fixture or the replay, not a free pass.
    #[test]
    fn the_conversation_corpus_exercises_cross_turn_folding() {
        let conversations = fixture();
        assert!(
            !conversations.is_empty(),
            "the fixture has to hold conversations"
        );
        let options = EngineOptions::default();
        let results = replay_conversations(&conversations, &options);

        assert_eq!(
            conversations.len(),
            COMMITTED_CONVERSATIONS,
            "the fixture changed; update COMMITTED_CONVERSATIONS and the fold floor with it"
        );

        let folded: u32 = results.iter().map(|result| result.cross_turn).sum();
        assert!(
            folded >= COMMITTED_CROSS_TURN_FOLDS,
            "cross-turn folding dropped from {COMMITTED_CROSS_TURN_FOLDS} to {folded}; \
             the corpus exists to catch exactly this: {:?}",
            results
                .iter()
                .map(|r| (&r.id, r.cross_turn))
                .collect::<Vec<_>>()
        );
    }

    /// And the saving is real, not an artifact of counting folds.
    #[test]
    fn folding_removes_characters_from_a_real_conversation() {
        let options = EngineOptions::default();
        let results = replay_conversations(&fixture(), &options);
        let chars_in: usize = results.iter().map(|r| r.chars_in).sum();
        let chars_out: usize = results.iter().map(|r| r.chars_out).sum();
        assert!(chars_in > 0, "the fixture has to carry text");
        assert!(
            chars_out < chars_in,
            "replaying a conversation removed nothing: {chars_in} -> {chars_out}"
        );
    }

    #[test]
    #[ignore = "reporting, not a gate: run with --ignored --nocapture"]
    fn show_what_the_conversation_corpus_covers() {
        let options = EngineOptions::default();
        for result in replay_conversations(&fixture(), &options) {
            println!(
                "{} turns={} {} -> {} ({:.1}% removed) cross_turn={} types={:?}",
                result.id,
                result.turns,
                result.chars_in,
                result.chars_out,
                result.removed() * 100.0,
                result.cross_turn,
                result.by_type
            );
        }
    }

    #[test]
    fn a_conversation_corpus_round_trips() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("conversations.jsonl.gz");
        let written = vec![Conversation {
            id: "c1".into(),
            task: "t".into(),
            provider: String::new(),
            model: String::new(),
            turns: vec![Turn {
                role: "tool".into(),
                text: "hello".into(),
            }],
        }];
        write_conversations(&path, &written).expect("write");
        let read = load_conversations(&path).expect("read");
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].id, "c1");
        assert_eq!(read[0].turns[0].text, "hello");
    }
}
