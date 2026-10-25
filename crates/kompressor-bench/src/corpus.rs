//! Reading a corpus file into payloads.
//!
//! A corpus is JSON lines, optionally gzipped, one payload per line: a tool
//! output exactly as an agent received it, plus the bookkeeping needed to weigh
//! it — how many requests carried it, which task it came from. Only tool output
//! is in there; every other role is protected from compression, so replaying it
//! would measure nothing.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use kompressor_engine::refs::ref_for;
use serde::Deserialize;

/// Anything that can stop the harness.
#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    #[error("could not read {0}: {1}")]
    Read(String, #[source] std::io::Error),

    #[error("could not write {0}: {1}")]
    Write(String, #[source] std::io::Error),

    #[error("{path}: line {line} is not a corpus record: {source}")]
    Line {
        path: String,
        line: usize,
        #[source]
        source: serde_json::Error,
    },

    #[error("{0} holds no payloads")]
    Empty(String),

    /// A baseline written by a different version of this harness.
    ///
    /// Loud on purpose. The alternative — reading an old file field by field
    /// and defaulting whatever is missing — compares this run against zeros and
    /// reports a regression-free improvement in every metric that was added
    /// since.
    #[error("{path}: baseline format {found}, expected {expected}; re-record it with --save")]
    BaselineFormat {
        path: String,
        found: u32,
        expected: u32,
    },

    #[error("could not parse {path}: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
}

/// How many payloads `fixtures/corpus.jsonl.gz` holds.
///
/// Stated once so the corpus, the recorded baseline and the replay tests cannot
/// drift apart: growing the fixture without re-recording the baseline against it
/// fails here rather than quietly comparing against a floor for a corpus that no
/// longer exists.
pub const COMMITTED_CORPUS_PAYLOADS: usize = 894;

/// One payload as the corpus recorded it.
#[derive(Clone, Debug)]
pub struct Payload {
    /// Stable identity of the *content*, so a payload can be followed across
    /// runs even though its position in the corpus is not meaningful.
    pub content_id: String,
    /// Always `tool` in the shipped corpora; carried because the engine's
    /// protection rules key on it.
    pub role: String,
    pub content: String,
    /// The user turn that preceded the tool call.
    ///
    /// Present so the harness can prove the engine does *not* read it: a
    /// payload whose output changes with the question that prompted it has
    /// different bytes every turn and bursts the provider prompt cache.
    pub preceding_user_text: String,
    pub source_tool: String,
    /// Times this exact content appeared in the source runs.
    pub occurrences: u32,
    /// Requests that carried this payload. The weight behind
    /// [`crate::ReplayReport::billed`].
    pub requests: u32,
    /// Task the payload was captured from, for stratification.
    pub task: String,
}

/// One line of the file: the header line and a payload line share a shape.
#[derive(Deserialize)]
struct Row {
    /// Present on the leading header line only.
    #[serde(default)]
    corpus_version: Option<u32>,
    #[serde(default)]
    content_id: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    preceding_user_text: Option<String>,
    #[serde(default)]
    source_tool: Option<String>,
    #[serde(default)]
    occurrences: Option<u32>,
    #[serde(default)]
    requests: Option<u32>,
    #[serde(default)]
    task: Option<String>,
}

/// Read a corpus, gzipped or plain.
///
/// The compression is detected from the first two bytes rather than the file
/// name: a corpus arrives from `gzip`, from a pipe, and from an editor that
/// helpfully decompressed it, and the extension is right in only some of those
/// cases.
///
/// The leading `{"corpus_version": N}` header is skipped, as is any line with
/// no `content` — a corpus is appended to by tooling and a trailing blank line
/// is not a reason to fail a benchmark run.
pub fn load_corpus(path: &Path) -> Result<Vec<Payload>, BenchError> {
    let display = path.display().to_string();
    let file = File::open(path).map_err(|e| BenchError::Read(display.clone(), e))?;
    let reader = decoder(file, &display)?;

    let mut payloads = Vec::new();
    for (index, line) in BufReader::new(reader).lines().enumerate() {
        let line = line.map_err(|e| BenchError::Read(display.clone(), e))?;
        if line.trim().is_empty() {
            continue;
        }
        let row: Row = serde_json::from_str(&line).map_err(|e| BenchError::Line {
            path: display.clone(),
            line: index + 1,
            source: e,
        })?;
        let Some(content) = row.content else {
            // the header, or a record carrying only bookkeeping
            let _ = row.corpus_version;
            continue;
        };
        payloads.push(Payload {
            // Content addressing, matching the engine's own handles, so an id
            // is reproducible from the payload alone when a corpus omits one.
            content_id: row
                .content_id
                .unwrap_or_else(|| ref_for(&content).body().to_string()),
            role: row.role.unwrap_or_else(|| "tool".to_string()),
            content,
            preceding_user_text: row.preceding_user_text.unwrap_or_default(),
            source_tool: row.source_tool.unwrap_or_default(),
            occurrences: row.occurrences.unwrap_or(1),
            requests: row.requests.unwrap_or(0),
            task: row.task.unwrap_or_default(),
        });
    }

    if payloads.is_empty() {
        return Err(BenchError::Empty(display));
    }
    Ok(payloads)
}

/// The corpus format this harness writes.
///
/// Bumped when a reader of an older file would misread it. Readers skip the
/// header line, so an unknown version is not fatal on load — it is stated here
/// so a writer and a baseline cannot silently disagree about the shape.
pub const CORPUS_VERSION: u32 = 1;

/// Write payloads as a corpus, gzipped when `path` ends in `.gz`.
///
/// The counterpart of [`load_corpus`], and the reason corpus *shape* stays in
/// this module: a tool that extracts payloads should not also have to know
/// which fields a replay needs, or it will omit one and the replay will
/// silently weigh every payload the same.
pub fn write_corpus(path: &Path, payloads: &[Payload]) -> Result<(), BenchError> {
    let display = path.display().to_string();
    let mut out = String::new();
    out.push_str(&format!("{{\"corpus_version\": {CORPUS_VERSION}}}\n"));
    for payload in payloads {
        let row = serde_json::json!({
            "content_id": payload.content_id,
            "content": payload.content,
            "role": payload.role,
            "preceding_user_text": payload.preceding_user_text,
            "source_tool": payload.source_tool,
            "occurrences": payload.occurrences,
            "requests": payload.requests,
            "task": payload.task,
        });
        out.push_str(
            &serde_json::to_string(&row).map_err(|source| BenchError::Json {
                path: display.clone(),
                source,
            })?,
        );
        out.push('\n');
    }
    let bytes = if path.extension().is_some_and(|e| e == "gz") {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder
            .write_all(out.as_bytes())
            .and_then(|()| encoder.finish())
            .map_err(|e| BenchError::Write(display.clone(), e))?
    } else {
        out.into_bytes()
    };
    std::fs::write(path, bytes).map_err(|e| BenchError::Write(display, e))
}

#[cfg(test)]
mod write_tests {
    use super::*;

    fn payload(content: &str) -> Payload {
        Payload {
            content_id: ref_for(content).body().to_string(),
            role: "tool".to_string(),
            content: content.to_string(),
            preceding_user_text: String::new(),
            source_tool: "exec".to_string(),
            occurrences: 3,
            requests: 7,
            task: "a-task".to_string(),
        }
    }

    #[test]
    fn a_written_corpus_reads_back_with_its_weights_intact() {
        // `requests` is the weight behind the billed metric. A writer that
        // dropped it would leave every payload weighing the same and the
        // headline would move for a reason nobody could find.
        let dir = std::env::temp_dir().join(format!("kbench-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        for name in ["c.jsonl", "c.jsonl.gz"] {
            let path = dir.join(name);
            let written = vec![payload("alpha beta"), payload("gamma delta")];
            write_corpus(&path, &written).expect("write");
            let read = load_corpus(&path).expect("read");
            assert_eq!(read.len(), 2, "{name}");
            assert_eq!(read[0].content, "alpha beta", "{name}");
            assert_eq!(read[0].occurrences, 3, "{name}");
            assert_eq!(read[0].requests, 7, "{name}");
            assert_eq!(read[0].task, "a-task", "{name}");
            assert_eq!(read[0].content_id, written[0].content_id, "{name}");
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn the_gz_extension_decides_the_encoding() {
        let dir = std::env::temp_dir().join(format!("kbench-enc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let plain = dir.join("p.jsonl");
        let zipped = dir.join("z.jsonl.gz");
        write_corpus(&plain, &[payload("x")]).expect("write");
        write_corpus(&zipped, &[payload("x")]).expect("write");
        assert_ne!(&std::fs::read(&zipped).expect("read")[..2], b"{\"");
        assert_eq!(&std::fs::read(&plain).expect("read")[..2], b"{\"");
        let _ = std::fs::remove_file(&plain);
        let _ = std::fs::remove_file(&zipped);
    }
}

/// Wrap `file` in a gzip decoder when its magic bytes say to.
fn decoder(mut file: File, display: &str) -> Result<Box<dyn Read>, BenchError> {
    let mut magic = [0u8; 2];
    let read = read_upto(&mut file, &mut magic).map_err(|e| BenchError::Read(display.into(), e))?;
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))
        .map_err(|e| BenchError::Read(display.into(), e))?;
    if read == 2 && magic == [0x1f, 0x8b] {
        Ok(Box::new(flate2::read::GzDecoder::new(file)))
    } else {
        Ok(Box::new(file))
    }
}

/// `Read::read` may return fewer bytes than asked for without being at the end.
fn read_upto(file: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &tempfile::TempDir, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut file = File::create(&path).unwrap();
        file.write_all(body.as_bytes()).unwrap();
        path
    }

    #[test]
    fn a_corpus_header_line_is_not_read_as_a_payload() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            "c.jsonl",
            "{\"corpus_version\": 1}\n{\"content_id\":\"a\",\"content\":\"hello\"}\n",
        );
        let payloads = load_corpus(&path).unwrap();
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].content_id, "a");
    }

    #[test]
    fn a_payload_without_a_recorded_id_gets_one_from_its_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "c.jsonl", "{\"content\":\"hello\"}\n");
        let payloads = load_corpus(&path).unwrap();
        assert_eq!(payloads[0].content_id, ref_for("hello").body());
    }

    #[test]
    fn a_gzipped_corpus_is_detected_by_its_magic_bytes_not_its_name() {
        use flate2::write::GzEncoder;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-extension");
        let mut encoder = GzEncoder::new(File::create(&path).unwrap(), flate2::Compression::fast());
        encoder
            .write_all(b"{\"content_id\":\"z\",\"content\":\"gzipped\"}\n")
            .unwrap();
        encoder.finish().unwrap();

        let payloads = load_corpus(&path).unwrap();
        assert_eq!(payloads[0].content, "gzipped");
    }

    #[test]
    fn a_malformed_line_names_its_line_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "c.jsonl", "{\"content\":\"ok\"}\nnot json\n");
        let error = load_corpus(&path).unwrap_err().to_string();
        assert!(error.contains("line 2"), "{error}");
    }

    #[test]
    fn a_corpus_with_no_payloads_is_an_error_rather_than_an_empty_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "c.jsonl", "{\"corpus_version\": 1}\n");
        assert!(matches!(
            load_corpus(&path).unwrap_err(),
            BenchError::Empty(_)
        ));
    }
}
