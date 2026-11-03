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
        write!(f, "{}:{}", self.host, self.port)
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
}

impl Refusal {
    /// The status line to answer with. A refusal is still an HTTP response:
    /// closing the socket silently makes a client retry rather than report.
    pub fn status_line(&self) -> &'static str {
        match self {
            Refusal::NotConnect => "HTTP/1.1 405 Method Not Allowed\r\n\r\n",
            Refusal::BadAuthority => "HTTP/1.1 400 Bad Request\r\n\r\n",
            Refusal::Unreadable => "HTTP/1.1 400 Bad Request\r\n\r\n",
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
    let mut line = String::new();
    let mut read = 0usize;

    if reader.read_line(&mut line).await.is_err() || line.is_empty() {
        return Err((Refusal::Unreadable, reader));
    }
    read += line.len();

    let target = match parse_request_line(line.trim_end()) {
        Ok(t) => t,
        Err(e) => return Err((e, reader)),
    };

    // Drain to the blank line. A CONNECT has no body, so whatever follows the
    // headers belongs to the tunnel.
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header).await {
            Ok(0) => return Err((Refusal::Unreadable, reader)),
            Ok(n) => {
                read += n;
                if header.trim().is_empty() {
                    break;
                }
                if read > MAX_HEADER_BYTES {
                    return Err((Refusal::Unreadable, reader));
                }
            }
            Err(_) => return Err((Refusal::Unreadable, reader)),
        }
    }

    Ok((target, reader))
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
