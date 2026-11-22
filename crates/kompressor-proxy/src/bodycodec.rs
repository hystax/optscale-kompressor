//! Decoding a request body that arrived compressed, and putting it back.
//!
//! A proxy that reads bodies has to speak the encodings its clients use, and
//! the cost of not doing so is invisible: the body fails to parse, the request
//! is forwarded untouched, every health check stays green and nothing is ever
//! compressed. Measured against codex-cli 0.155.1, which sends
//! `content-encoding: zstd` on **every** request while still declaring
//! `content-type: application/json` — so the content type cannot be used to
//! tell an encoded body from a plain one, and a proxy that reads the body
//! without consulting the encoding compresses nothing at all for that client.
//!
//! # What is decoded, and what is refused
//!
//! An encoding this module does not know is not a failure and not a guess: it
//! is left alone, which lands the request on the same fail-open path it took
//! before. Multiple codings (`gzip, br`) are refused for the same reason —
//! unwrapping the outer layer would produce bytes that are still encoded, and
//! the honest answer is that we do not handle it.
//!
//! # Bombs
//!
//! Decompression is bounded by the same `max_body_bytes` that bounds the read.
//! Without that, a few kilobytes of zstd expands to as much memory as the
//! sender likes, which turns a compression proxy into a way to kill the host.
//! The bound is on the *decoded* size and the decoder is stopped at it rather
//! than asked how big the result will be, because the header that answers that
//! question is written by the sender.

use std::io::Read;

use bytes::Bytes;
use http::HeaderMap;

/// A content coding this proxy can read and write.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BodyEncoding {
    /// Not encoded, or an encoding we leave alone.
    #[default]
    Identity,
    Zstd,
    Gzip,
    Deflate,
}

impl BodyEncoding {
    /// The coding named by `content-encoding`, or [`Identity`] for a header
    /// that is absent, empty, `identity`, or something we do not handle.
    ///
    /// [`Identity`]: BodyEncoding::Identity
    pub fn from_headers(headers: &HeaderMap) -> BodyEncoding {
        let Some(value) = headers
            .get(http::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
        else {
            return BodyEncoding::Identity;
        };
        let value = value.trim();
        // A list means layered codings. Unwrapping one layer leaves bytes that
        // are still encoded, so this declines rather than half-decoding.
        if value.contains(',') {
            return BodyEncoding::Identity;
        }
        match value.to_ascii_lowercase().as_str() {
            "zstd" => BodyEncoding::Zstd,
            "gzip" | "x-gzip" => BodyEncoding::Gzip,
            "deflate" => BodyEncoding::Deflate,
            _ => BodyEncoding::Identity,
        }
    }

    /// Whether this coding actually transforms the bytes.
    pub fn is_encoded(self) -> bool {
        self != BodyEncoding::Identity
    }

    /// The header value to send for this coding.
    pub fn header_value(self) -> Option<&'static str> {
        match self {
            BodyEncoding::Identity => None,
            BodyEncoding::Zstd => Some("zstd"),
            BodyEncoding::Gzip => Some("gzip"),
            BodyEncoding::Deflate => Some("deflate"),
        }
    }

    /// Decode `raw`, refusing to produce more than `limit` bytes.
    ///
    /// `None` means the body could not be decoded — truncated, corrupt, or
    /// over the limit — and the caller forwards what arrived instead.
    pub fn decode(self, raw: &[u8], limit: usize) -> Option<Bytes> {
        if !self.is_encoded() {
            return Some(Bytes::copy_from_slice(raw));
        }
        let reader: Box<dyn Read> = match self {
            BodyEncoding::Zstd => Box::new(zstd::stream::read::Decoder::new(raw).ok()?),
            // `Multi`: a gzip stream may be several members end to end, and
            // `GzDecoder` stops at the end of the first without an error —
            // the body comes back cut short, and the limit below is checked
            // against the part that was read.
            BodyEncoding::Gzip => Box::new(flate2::read::MultiGzDecoder::new(raw)),
            BodyEncoding::Deflate => Box::new(flate2::read::ZlibDecoder::new(raw)),
            BodyEncoding::Identity => unreachable!("handled above"),
        };
        // `limit + 1` so a body that lands exactly on the limit is accepted and
        // one byte over is refused, rather than the boundary being ambiguous.
        let mut out = Vec::new();
        let mut capped = reader.take(limit as u64 + 1);
        capped.read_to_end(&mut out).ok()?;
        if out.len() > limit {
            tracing::warn!(
                limit,
                "a compressed request body expanded past proxy.max_body_bytes"
            );
            return None;
        }
        Some(Bytes::from(out))
    }

    /// Re-encode `plain` in this coding.
    ///
    /// `None` means the encoder failed, and the caller has to decide what to
    /// send — there is no correct fallback here, because a body labelled with
    /// a coding it is not in is worse than either alternative.
    pub fn encode(self, plain: &[u8]) -> Option<Bytes> {
        match self {
            BodyEncoding::Identity => Some(Bytes::copy_from_slice(plain)),
            // Level 3 is the zstd default: this is on the request path, and
            // spending milliseconds to save bytes on a body whose point is to
            // carry fewer tokens is the wrong trade.
            BodyEncoding::Zstd => zstd::stream::encode_all(plain, 3).ok().map(Bytes::from),
            BodyEncoding::Gzip => {
                let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                std::io::Write::write_all(&mut e, plain).ok()?;
                e.finish().ok().map(Bytes::from)
            }
            BodyEncoding::Deflate => {
                let mut e =
                    flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
                std::io::Write::write_all(&mut e, plain).ok()?;
                e.finish().ok().map(Bytes::from)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        if !value.is_empty() {
            h.insert(
                "content-encoding",
                http::HeaderValue::from_str(value).unwrap(),
            );
        }
        h
    }

    #[test]
    fn the_codings_this_proxy_reads_are_recognised_whatever_their_case() {
        assert_eq!(
            BodyEncoding::from_headers(&headers("zstd")),
            BodyEncoding::Zstd
        );
        assert_eq!(
            BodyEncoding::from_headers(&headers("ZSTD")),
            BodyEncoding::Zstd
        );
        assert_eq!(
            BodyEncoding::from_headers(&headers(" gzip ")),
            BodyEncoding::Gzip
        );
        assert_eq!(
            BodyEncoding::from_headers(&headers("deflate")),
            BodyEncoding::Deflate
        );
    }

    #[test]
    fn an_absent_or_identity_header_is_not_an_encoding() {
        assert_eq!(
            BodyEncoding::from_headers(&headers("")),
            BodyEncoding::Identity
        );
        assert_eq!(
            BodyEncoding::from_headers(&headers("identity")),
            BodyEncoding::Identity
        );
        assert!(!BodyEncoding::from_headers(&headers("")).is_encoded());
    }

    #[test]
    fn a_coding_we_do_not_handle_is_left_alone_rather_than_guessed_at() {
        // Brotli is real and we do not read it. Claiming Identity here sends
        // the request down the fail-open path, which is the behaviour that
        // existed before this module and is the right one.
        assert_eq!(
            BodyEncoding::from_headers(&headers("br")),
            BodyEncoding::Identity
        );
        // Layered codings: unwrapping one leaves bytes that are still encoded.
        assert_eq!(
            BodyEncoding::from_headers(&headers("gzip, br")),
            BodyEncoding::Identity
        );
    }

    #[test]
    fn every_coding_round_trips() {
        let plain = br#"{"model":"gpt-5.5","input":[{"role":"user","content":"hello"}]}"#;
        for coding in [
            BodyEncoding::Zstd,
            BodyEncoding::Gzip,
            BodyEncoding::Deflate,
            BodyEncoding::Identity,
        ] {
            let encoded = coding.encode(plain).expect("encodes");
            let decoded = coding.decode(&encoded, 1 << 20).expect("decodes");
            assert_eq!(&decoded[..], &plain[..], "{coding:?} did not round-trip");
        }
    }

    /// RFC 1952 lets a gzip stream be several members end to end, and a client
    /// that compresses in chunks sends exactly that. Read to the first
    /// member's end, the body is cut short without an error: JSON that spans
    /// members does not parse and is forwarded uncompressed, and a first
    /// member that happens to be whole JSON would be rewritten and sent on
    /// without the rest.
    #[test]
    fn gzip_reads_all_members_and_checks_the_combined_limit() {
        let mut encoded = BodyEncoding::Gzip.encode(b"first").unwrap().to_vec();
        encoded.extend_from_slice(&BodyEncoding::Gzip.encode(b"second").unwrap());
        assert_eq!(
            BodyEncoding::Gzip.decode(&encoded, 11).unwrap(),
            "firstsecond"
        );
        // The limit is on what all the members decode to, not on each.
        assert!(BodyEncoding::Gzip.decode(&encoded, 10).is_none());
        // A second member cut short is a body that did not arrive whole.
        let mut truncated = encoded.clone();
        truncated.pop();
        assert!(BodyEncoding::Gzip.decode(&truncated, 100).is_none());
        // And bytes after the last member that are no member at all are not
        // dropped on the floor: the body is forwarded as it came instead.
        encoded.extend_from_slice(b"not gzip");
        assert!(BodyEncoding::Gzip.decode(&encoded, 100).is_none());
    }

    /// The same holds for zstd, which is what codex sends: frames end to end
    /// are one body.
    #[test]
    fn zstd_reads_all_frames_and_checks_the_combined_limit() {
        let mut encoded = BodyEncoding::Zstd.encode(b"first").unwrap().to_vec();
        encoded.extend_from_slice(&BodyEncoding::Zstd.encode(b"second").unwrap());
        assert_eq!(
            BodyEncoding::Zstd.decode(&encoded, 11).unwrap(),
            "firstsecond"
        );
        assert!(BodyEncoding::Zstd.decode(&encoded, 10).is_none());
    }

    #[test]
    fn a_body_that_expands_past_the_limit_is_refused() {
        // The reason the limit is on the decoded size: this is 10 MB of zeroes
        // in a few hundred bytes, and a proxy that trusts the sender's idea of
        // how big a body is has handed them the host's memory.
        let bomb = zstd::stream::encode_all(&vec![0u8; 10 << 20][..], 3).unwrap();
        assert!(bomb.len() < 4096, "the point is that the input is small");
        assert!(BodyEncoding::Zstd.decode(&bomb, 1 << 20).is_none());
        // The same bytes are fine under a limit that permits them.
        assert!(BodyEncoding::Zstd.decode(&bomb, 32 << 20).is_some());
    }

    #[test]
    fn a_body_exactly_on_the_limit_is_accepted() {
        let plain = vec![b'x'; 1000];
        let encoded = BodyEncoding::Zstd.encode(&plain).unwrap();
        assert!(BodyEncoding::Zstd.decode(&encoded, 1000).is_some());
        assert!(BodyEncoding::Zstd.decode(&encoded, 999).is_none());
    }

    #[test]
    fn a_corrupt_body_is_a_decline_not_a_panic() {
        assert!(
            BodyEncoding::Zstd
                .decode(b"not zstd at all", 1 << 20)
                .is_none()
        );
        assert!(
            BodyEncoding::Gzip
                .decode(b"not gzip at all", 1 << 20)
                .is_none()
        );
    }

    #[test]
    fn what_codex_sends_is_read_as_zstd() {
        // The measured shape: zstd body, JSON content type. Reading the content
        // type alone says "plain JSON" and produces an unparseable body.
        let mut h = HeaderMap::new();
        h.insert(
            "content-type",
            http::HeaderValue::from_static("application/json"),
        );
        h.insert("content-encoding", http::HeaderValue::from_static("zstd"));
        assert_eq!(BodyEncoding::from_headers(&h), BodyEncoding::Zstd);
    }
}
