//! Reading the `CONNECT` request a proxy-configured client opens with.
//!
//! Only enough HTTP to answer one method. Everything this listener serves
//! arrives as `CONNECT host:port HTTP/1.1`, and anything else is refused
//! rather than guessed at: this is not a general proxy and should not look
//! like one.

use std::fmt;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// The authority a client asked to reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            // an IPv6 literal, which needs its brackets back to be an authority
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

/// Why a request line was not a CONNECT this listener will serve.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A method other than CONNECT. A browser pointed here by accident, or a
    /// client that thinks this is an origin server.
    NotConnect,
    /// CONNECT with an authority that does not parse.
    BadAuthority,
    /// The client hung up, or sent a header block longer than any real one.
    Unreadable,
    /// The origin of a tunnelled host could not be reached. Answered before
    /// the tunnel opens, so the client sees a proxy error instead of a
    /// handshake that dies after "200 Connection Established".
    Unreachable,
}

impl Refusal {
    /// The status line to answer with. A refusal is still an HTTP response:
    /// closing the socket silently makes a client retry rather than report.
    pub fn status_line(&self) -> &'static str {
        match self {
            Refusal::NotConnect => "HTTP/1.1 405 Method Not Allowed\r\n\r\n",
            Refusal::BadAuthority => "HTTP/1.1 400 Bad Request\r\n\r\n",
            Refusal::Unreadable => "HTTP/1.1 400 Bad Request\r\n\r\n",
            Refusal::Unreachable => "HTTP/1.1 502 Bad Gateway\r\n\r\n",
        }
    }
}

/// Header blocks larger than this are a client doing something other than
/// opening a tunnel.
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// Parse one request line. Split out from the reading so it can be tested
/// without a socket, which is most of what is worth testing here.
pub fn parse_request_line(line: &str) -> Result<Target, Refusal> {
    let mut parts = line.split_whitespace();
    let method = parts.next().ok_or(Refusal::Unreadable)?;
    if !method.eq_ignore_ascii_case("CONNECT") {
        return Err(Refusal::NotConnect);
    }
    let authority = parts.next().ok_or(Refusal::BadAuthority)?;
    // CONNECT carries `host:port`, always with the port: RFC 9110 requires it,
    // and a bare host here means something other than a tunnel is happening.
    let (host, port) = authority.rsplit_once(':').ok_or(Refusal::BadAuthority)?;
    let host = host.trim_matches(|c| c == '[' || c == ']');
    if host.is_empty() {
        return Err(Refusal::BadAuthority);
    }
    let port: u16 = port.parse().map_err(|_| Refusal::BadAuthority)?;
    Ok(Target {
        host: host.to_ascii_lowercase(),
        port,
    })
}

/// Read the request line and drain the headers, leaving the stream positioned
/// at the client's first TLS byte.
///
/// Returns the reader as well: it may have buffered bytes past the header
/// block, and dropping it would drop the beginning of the handshake.
pub async fn read_connect<S>(stream: S) -> Result<(Target, BufReader<S>), (Refusal, BufReader<S>)>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut left = MAX_HEADER_BYTES;

    let line = match read_line_within(&mut reader, &mut left).await {
        Some(line) if !line.is_empty() => line,
        _ => return Err((Refusal::Unreadable, reader)),
    };
    let target = match parse_request_line(line.trim_end()) {
        Ok(t) => t,
        Err(e) => return Err((e, reader)),
    };

    // Drain to the blank line. A CONNECT has no body, so whatever follows the
    // headers belongs to the tunnel.
    loop {
        match read_line_within(&mut reader, &mut left).await {
            Some(header) if header.is_empty() => return Err((Refusal::Unreadable, reader)),
            Some(header) if header.trim().is_empty() => break,
            Some(_) => {}
            None => return Err((Refusal::Unreadable, reader)),
        }
    }

    Ok((target, reader))
}

/// One line, newline included, taken out of the `left` bytes the whole header
/// block may still use. `Some("")` is end of input; `None` is a read error, a
/// line that is not UTF-8, or one that does not end within the allowance.
///
/// `read_line` cannot be used for this: it has no limit, so checking the total
/// *after* it returns let a client that never sends a newline grow the buffer
/// without bound, on a listener that reads before it authenticates anything.
async fn read_line_within<S>(reader: &mut BufReader<S>, left: &mut usize) -> Option<String>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await.ok()?;
        if available.is_empty() {
            break;
        }
        let (take, ended) = match available.iter().position(|byte| *byte == b'\n') {
            Some(at) => (at + 1, true),
            None => (available.len(), false),
        };
        if take > *left {
            return None;
        }
        *left -= take;
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if ended {
            break;
        }
    }
    String::from_utf8(line).ok()
}

/// Tell the client its tunnel is open. Everything after this is its own bytes.
pub async fn accept<S>(stream: &mut S) -> std::io::Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    stream
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    stream.flush().await
}

/// Answer a refusal, best effort: the client may already be gone.
pub async fn refuse<S>(stream: &mut S, refusal: &Refusal)
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let _ = stream.write_all(refusal.status_line().as_bytes()).await;
    let _ = stream.flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    /// A reader that counts what is pulled out of it, so a test can say how
    /// much of an endless input was actually consumed.
    struct Counted<R> {
        inner: R,
        pulled: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Counted<R> {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let before = buf.filled().len();
            let poll = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
            self.pulled.fetch_add(
                buf.filled().len() - before,
                std::sync::atomic::Ordering::Relaxed,
            );
            poll
        }
    }

    /// A request line that never ends is refused once the header allowance is
    /// spent — not after the whole of it has been buffered. `read_line` has no
    /// limit of its own, so 64 MiB of `A` used to be read into one `String`
    /// before the size was looked at.
    #[test]
    fn an_endless_request_line_is_refused_at_the_cap() {
        let pulled = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let input = Counted {
            inner: tokio::io::repeat(b'A').take(64 << 20),
            pulled: pulled.clone(),
        };
        let result = block_on(read_connect(input));
        assert!(matches!(result, Err((Refusal::Unreadable, _))));
        let pulled = pulled.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            pulled <= MAX_HEADER_BYTES + 64 * 1024,
            "{pulled} bytes were read before the refusal"
        );
    }

    /// The same for one header with no newline after a valid request line.
    #[test]
    fn an_endless_header_is_refused_at_the_cap() {
        let pulled = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let head: &[u8] = b"CONNECT api.anthropic.com:443 HTTP/1.1\r\nX-Pad: ";
        let input = Counted {
            inner: head.chain(tokio::io::repeat(b'A').take(64 << 20)),
            pulled: pulled.clone(),
        };
        let result = block_on(read_connect(input));
        assert!(matches!(result, Err((Refusal::Unreadable, _))));
        assert!(pulled.load(std::sync::atomic::Ordering::Relaxed) <= MAX_HEADER_BYTES + 64 * 1024);
    }

    /// Many small headers that together pass the allowance are refused too.
    #[test]
    fn headers_that_add_up_past_the_cap_are_refused() {
        let mut request = b"CONNECT api.anthropic.com:443 HTTP/1.1\r\n".to_vec();
        for i in 0..2_000 {
            request.extend_from_slice(format!("X-H{i}: value\r\n").as_bytes());
        }
        request.extend_from_slice(b"\r\n");
        assert!(request.len() > MAX_HEADER_BYTES);
        let result = block_on(read_connect(&request[..]));
        assert!(matches!(result, Err((Refusal::Unreadable, _))));
    }

    /// An ordinary request still reads, and the bytes after the blank line —
    /// the start of the TLS handshake — are left for the tunnel.
    #[test]
    fn a_normal_request_reads_and_leaves_the_handshake_bytes() {
        let request: &[u8] =
            b"CONNECT api.anthropic.com:443 HTTP/1.1\r\nHost: api.anthropic.com:443\r\nProxy-Connection: keep-alive\r\n\r\n\x16\x03\x01hello";
        let (target, mut reader) = match block_on(read_connect(request)) {
            Ok(pair) => pair,
            Err((refusal, _)) => panic!("refused: {refusal:?}"),
        };
        assert_eq!(
            (target.host.as_str(), target.port),
            ("api.anthropic.com", 443)
        );
        let mut rest = Vec::new();
        block_on(reader.read_to_end(&mut rest)).unwrap();
        assert_eq!(rest, b"\x16\x03\x01hello");
    }

    /// Input that ends before the blank line, or is empty, is unreadable.
    #[test]
    fn a_request_cut_short_is_unreadable() {
        for input in [
            &b""[..],
            b"CONNECT a:1 HTTP/1.1\r\n",
            b"CONNECT a:1 HTTP/1.1\r\nHost: a",
        ] {
            assert!(
                matches!(block_on(read_connect(input)), Err((Refusal::Unreadable, _))),
                "{:?}",
                String::from_utf8_lossy(input)
            );
        }
    }

    #[test]
    fn parses_a_normal_connect() {
        let t = parse_request_line("CONNECT api.anthropic.com:443 HTTP/1.1").unwrap();
        assert_eq!(t.host, "api.anthropic.com");
        assert_eq!(t.port, 443);
    }

    #[test]
    fn lowercases_the_host_so_matching_need_not() {
        assert_eq!(
            parse_request_line("CONNECT API.Anthropic.COM:443 HTTP/1.1")
                .unwrap()
                .host,
            "api.anthropic.com"
        );
    }

    #[test]
    fn a_bracketed_ipv6_literal_keeps_its_port() {
        let t = parse_request_line("CONNECT [2001:db8::1]:443 HTTP/1.1").unwrap();
        assert_eq!(t.host, "2001:db8::1");
        assert_eq!(t.port, 443);
        assert_eq!(t.to_string(), "[2001:db8::1]:443");
    }

    #[test]
    fn anything_but_connect_is_405() {
        // A browser opened the port directly. Saying so beats a dead socket.
        assert_eq!(
            parse_request_line("GET / HTTP/1.1").unwrap_err(),
            Refusal::NotConnect
        );
        assert!(Refusal::NotConnect.status_line().contains("405"));
    }

    #[test]
    fn malformed_authorities_are_400() {
        for line in [
            "CONNECT api.anthropic.com HTTP/1.1",
            "CONNECT :443 HTTP/1.1",
            "CONNECT api.anthropic.com:https HTTP/1.1",
            "CONNECT api.anthropic.com:99999 HTTP/1.1",
        ] {
            assert_eq!(
                parse_request_line(line).unwrap_err(),
                Refusal::BadAuthority,
                "{line}"
            );
        }
    }

    #[test]
    fn an_empty_line_is_unreadable_rather_than_a_panic() {
        assert_eq!(parse_request_line("").unwrap_err(), Refusal::Unreadable);
    }

    #[tokio::test]
    async fn reads_the_request_and_stops_at_the_blank_line() {
        let wire = b"CONNECT api.anthropic.com:443 HTTP/1.1\r\nHost: api.anthropic.com:443\r\nProxy-Connection: keep-alive\r\n\r\n\x16\x03\x01";
        let (target, mut reader) = read_connect(&wire[..]).await.unwrap();
        assert_eq!(target.to_string(), "api.anthropic.com:443");
        // The TLS ClientHello's first byte must still be there to be read.
        let mut rest = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut rest)
            .await
            .unwrap();
        assert_eq!(rest, b"\x16\x03\x01");
    }
}
