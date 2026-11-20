//! Recording the bodies of a live session, for the one question summary
//! statistics cannot answer.
//!
//! The provider's cached prefix has been seen to shrink mid-conversation across
//! eighteen benchmark runs (DESIGN §20, open question 8), and five candidate
//! causes have been cleared by experiment without finding the real one. Every
//! one of those experiments worked from aggregates — token counts, handle
//! counts, message counts — and the remaining question is not answerable from
//! any of them: *which byte changed*. A hermetic reproduction was built and
//! cleared two suspects, but it has never reproduced the break itself, so the
//! fixtures differ from live traffic in some way that is itself unidentified.
//!
//! So this records what actually crosses the wire. Both directions, because the
//! discriminator is whether a changed byte came from the agent or from this
//! proxy: if turn N+1's *incoming* body already differs from turn N's at the
//! position the cache fell back to, the agent rewrote its own history and
//! nothing here is at fault.
//!
//! Off unless `KOMPRESSOR_BODY_CAPTURE` names a file, and best-effort when on —
//! a diagnostic that can break the session it is observing is worse than no
//! diagnostic. Bodies are written whole and uncompressed: a run is roughly
//! seventy requests of 15–80 KB each way, so a few megabytes, and truncating
//! them would throw away the only thing the file is for.
//!
//! Correlate with `proxy_requests` on `request_id` for the conversation, the
//! cache reads and the turn ordering; this file deliberately records none of
//! them rather than deriving a second copy that could disagree.
//!
//! Two things a replay of the file needs are recorded beside the bodies,
//! because nothing else in it implies them:
//!
//! * `beta` on a `client->proxy` line: a fingerprint of the request's beta
//!   tokens, sorted and de-duplicated ([`crate::drift::beta_fingerprint`]).
//!   The tokens are part of what the provider keys its cache on and are in no
//!   body. A fingerprint and not the header: it says when the set changed,
//!   which is the question, without recording what a client sent in a header.
//! * A `keep-warm` line for every ping the proxy sent: when, for which
//!   request (`request_id` is that request's, suffixed `.keepwarmN`), and
//!   what the provider read. A ping's body is the upstream body of the
//!   request it follows, already in the file, so none is written. Without
//!   these a replay cannot know which pauses the original run bridged.

use std::path::PathBuf;
use std::sync::Mutex;

/// Serializes writes to the capture file.
///
/// A proxy serves requests concurrently, and `writeln!` is not one syscall: two
/// requests capturing at once interleave mid-line and the file stops being
/// JSON. Found by the test that drives this through the real request path --
/// three other tests in the same binary were issuing requests while the capture
/// was armed, and the first line came back unparseable 21,649 characters in. A
/// lock rather than a single `write_all` because a short write would split a
/// line just as well, and this is a diagnostic where correctness beats speed.
static WRITING: Mutex<()> = Mutex::new(());

/// Where to record bodies, when `KOMPRESSOR_BODY_CAPTURE` names a file.
pub(crate) fn path() -> Option<PathBuf> {
    std::env::var_os("KOMPRESSOR_BODY_CAPTURE").map(PathBuf::from)
}

/// Append one body, if capturing.
///
/// `direction` is `client->proxy` or `proxy->upstream`. Lossy UTF-8 on purpose:
/// a body that is not valid UTF-8 is worth seeing rather than worth dropping,
/// and nothing here should be able to abort a capture.
///
/// `beta` is the fingerprint of the beta tokens the body travelled with,
/// where the caller has headers to take it from.
pub(crate) fn record(
    path: &Option<PathBuf>,
    request_id: &str,
    direction: &str,
    body: &[u8],
    beta: Option<u64>,
) {
    let Some(path) = path else { return };
    let mut record = serde_json::json!({
        "at_ms": now_ms(),
        "request_id": request_id,
        "dir": direction,
        "bytes": body.len(),
        "body": String::from_utf8_lossy(body),
    });
    if let Some(beta) = beta {
        record["beta"] = serde_json::json!(format!("{beta:016x}"));
    }
    append_line(path, &record.to_string());
}

/// Append one keep-warm ping, if capturing: the request it was sent for, and
/// what the provider answered. `read` is `None` when it reported no usage.
pub(crate) fn record_ping(
    path: &Option<PathBuf>,
    request_id: &str,
    status: Option<u16>,
    read: Option<u32>,
) {
    let Some(path) = path else { return };
    let record = serde_json::json!({
        "at_ms": now_ms(),
        "request_id": request_id,
        "dir": "keep-warm",
        "status": status,
        "read": read,
    });
    append_line(path, &record.to_string());
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

/// Append one line to a capture file, serialized against every other writer.
///
/// Shared with the websocket relay, which captures frames rather than bodies
/// but writes them the same way and has the same hazard twice over: its two
/// pump directions run concurrently by construction, so an unserialized append
/// interleaves a client frame with an upstream one and the file stops being
/// JSON.
/// **Created 0600 where the platform has file modes.** The operator picks where
/// this goes; they do not get to pick who can read it, and the default would
/// otherwise be the process umask — 0022 under the shipped systemd unit, so
/// 0644. What lands here is whole request and response bodies: prompts, tool
/// output, the model's replies. That is strictly more sensitive than `refs.db`,
/// whose exposure the unit file already reasons about explicitly.
///
/// The mode applies on creation only, so a file that already exists keeps the
/// permissions it has — deliberately, because an operator who has widened them
/// on purpose should not have that silently undone on the next append.
pub(crate) fn append_line(path: &std::path::Path, line: &str) {
    use std::io::Write;
    let line = format!("{line}\n");
    let _held = WRITING.lock();
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    if let Ok(mut file) = options.open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capture holds prompts and tool output, so it is not world-readable.
    ///
    /// Created at the ambient umask it was 0644 under the shipped systemd unit
    /// (`UMask=0022`). The operator chooses the path; the permissions are not
    /// theirs to get wrong by default.
    #[cfg(unix)]
    #[test]
    fn a_capture_file_is_created_private_to_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        // `tempfile`, like every other test here: a panic before the cleanup
        // line leaves a hand-rolled directory behind for ever.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("bodies.jsonl");

        append_line(&path, "{\"hello\":\"world\"}");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "capture created {mode:o}, want 600");
    }

    #[test]
    fn a_body_is_one_line_of_json_carrying_its_own_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let file = Some(dir.path().join("bodies.jsonl"));
        record(
            &file,
            "kp-1",
            "proxy->upstream",
            br#"{"model":"gpt-5"}"#,
            None,
        );
        record(&file, "kp-1", "client->proxy", b"second", Some(0xbe7a));
        record_ping(&file, "kp-1.keepwarm1", Some(200), Some(4_096));

        let written = std::fs::read_to_string(file.as_ref().unwrap()).unwrap();
        let lines: Vec<&str> = written.lines().collect();
        assert_eq!(lines.len(), 3, "one line per record, appended: {written}");
        // What a replay needs and no body holds: the beta set a request
        // travelled with, as a fingerprint, and each ping the proxy sent.
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["beta"], "000000000000be7a");
        let ping: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(
            (&ping["dir"], &ping["request_id"], &ping["read"]),
            (
                &serde_json::json!("keep-warm"),
                &serde_json::json!("kp-1.keepwarm1"),
                &serde_json::json!(4_096)
            )
        );
        assert!(
            ping.get("body").is_none(),
            "a ping's body is already in the file"
        );

        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["request_id"], "kp-1");
        assert_eq!(first["dir"], "proxy->upstream");
        assert_eq!(first["bytes"], 17);
        assert!(
            first.get("beta").is_none(),
            "only where there were headers to read"
        );
        // The body round-trips exactly. Diffing two of these is the whole point,
        // so a body that came back re-escaped or truncated would be useless.
        assert_eq!(first["body"], r#"{"model":"gpt-5"}"#);
    }

    #[test]
    fn capturing_nowhere_writes_nothing_and_cannot_fail() {
        record(&None, "kp-1", "proxy->upstream", b"ignored", None);
        record_ping(&None, "kp-1.keepwarm1", None, None);
        // A path that cannot be opened is the failure mode that must stay
        // silent: the session being observed is more important than the
        // observation.
        record(
            &Some(PathBuf::from("/does/not/exist/bodies.jsonl")),
            "kp-1",
            "proxy->upstream",
            b"ignored",
            None,
        );
    }
}
