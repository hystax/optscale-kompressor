//! The blind relay, for everything this listener does not intercept.
//!
//! A tunnelled connection is copied byte for byte and never decrypted. That is
//! the whole point: a client configured to use this proxy sends all its
//! traffic here, and all of it that is not on the allow list has to reach its
//! origin unexamined.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use super::connect::Target;

/// How long resolving and connecting to an origin may take, all addresses
/// together.
///
/// Without a bound the operating system decides, and on macOS that is about
/// 75 s *per address*. The client cannot tell a slow dial from a dead proxy,
/// and Claude Code reports a token refresh that never connected as an
/// authentication failure.
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one attempt runs alone before the next address is tried beside
/// it: RFC 8305's recommended "Connection Attempt Delay".
const ATTEMPT_DELAY: Duration = Duration::from_millis(250);

/// Connect to `target`, racing its addresses the way RFC 8305 describes.
///
/// `TcpStream::connect` tries the resolved addresses one after another, and a
/// network that publishes AAAA records but drops IPv6 — what most VPNs do to
/// stop leaks — turns every IPv6 address into a full operating-system connect
/// timeout before IPv4 is tried. Every Anthropic host has an IPv6 address.
/// Node, which the client runs on, falls back by itself, and so does the
/// `reqwest` client on the intercepted path; this tunnel was the one hop that
/// did not, so the integration failed on networks where the client alone
/// worked.
///
/// The host is resolved as a `(host, port)` pair rather than a `host:port`
/// string, so an IPv6 literal is not split at its last colon.
pub async fn dial(target: &Target) -> io::Result<TcpStream> {
    let attempt = async {
        let addresses: Vec<SocketAddr> =
            tokio::net::lookup_host((target.host.as_str(), target.port))
                .await?
                .collect();
        race(addresses, ATTEMPT_DELAY, TcpStream::connect).await
    };
    match tokio::time::timeout(DIAL_TIMEOUT, attempt).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("no address answered within {}s", DIAL_TIMEOUT.as_secs()),
        )),
    }
}

/// Copy in both directions until either side closes.
///
/// Errors are returned rather than logged here so the caller can report them
/// with the connection's context attached.
pub async fn relay<S>(client: &mut S, upstream: &mut TcpStream) -> io::Result<(u64, u64)>
where
    S: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    tokio::io::copy_bidirectional(client, upstream).await
}

/// Start an attempt per address, `delay` apart, and keep the first to connect.
///
/// A failed attempt starts the next address at once rather than waiting out
/// the delay. The connector is a parameter so the race can be tested without
/// a network that drops packets.
async fn race<T, F, Fut>(addresses: Vec<SocketAddr>, delay: Duration, connect: F) -> io::Result<T>
where
    F: Fn(SocketAddr) -> Fut,
    Fut: Future<Output = io::Result<T>>,
{
    let mut queue = interleave(addresses).into_iter();
    let mut attempts = FuturesUnordered::new();
    match queue.next() {
        Some(address) => attempts.push(connect(address)),
        None => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the host resolved to no address",
            ));
        }
    }
    loop {
        let more = queue.len() > 0;
        tokio::select! {
            // `attempts` is never empty here: it is refilled or the function
            // returns whenever an attempt ends.
            Some(result) = attempts.next() => match result {
                Ok(stream) => return Ok(stream),
                Err(error) => match queue.next() {
                    Some(address) => attempts.push(connect(address)),
                    None if attempts.is_empty() => return Err(error),
                    None => {}
                },
            },
            _ = tokio::time::sleep(delay), if more => {
                if let Some(address) = queue.next() {
                    attempts.push(connect(address));
                }
            }
        }
    }
}

/// Alternate address families, starting with the resolver's first choice, so
/// a family that is unreachable costs one attempt delay rather than every
/// address it has.
fn interleave(addresses: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let Some(first) = addresses.first() else {
        return addresses;
    };
    let preferred_v6 = first.is_ipv6();
    let (mut preferred, mut other): (Vec<_>, Vec<_>) = addresses
        .into_iter()
        .partition(|address| address.is_ipv6() == preferred_v6);
    let mut out = Vec::with_capacity(preferred.len() + other.len());
    preferred.reverse();
    other.reverse();
    loop {
        match (preferred.pop(), other.pop()) {
            (None, None) => break,
            (a, b) => out.extend(a.into_iter().chain(b)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn copies_both_directions_without_looking() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // An echo that also greets, so both directions carry bytes.
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket.write_all(b"hello").await.unwrap();
            let mut buf = [0u8; 5];
            socket.read_exact(&mut buf).await.unwrap();
            socket.write_all(&buf).await.unwrap();
        });

        let (mut client, mut proxy_side) = tokio::io::duplex(1024);
        let target = Target {
            host: "127.0.0.1".to_string(),
            port: addr.port(),
        };
        let relayed = tokio::spawn(async move {
            let mut upstream = dial(&target).await.unwrap();
            relay(&mut proxy_side, &mut upstream).await
        });

        let mut greeting = [0u8; 5];
        client.read_exact(&mut greeting).await.unwrap();
        assert_eq!(&greeting, b"hello");
        client.write_all(b"world").await.unwrap();
        let mut echoed = [0u8; 5];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"world");

        drop(client);
        let (from_client, to_client) = relayed.await.unwrap().unwrap();
        assert_eq!(from_client, 5);
        assert_eq!(to_client, 10);
    }

    #[tokio::test]
    async fn a_refused_origin_is_an_error_not_a_hang() {
        // Port 1 on loopback has nothing on it.
        let target = Target {
            host: "127.0.0.1".to_string(),
            port: 1,
        };
        assert!(dial(&target).await.is_err());
    }

    fn v4(last: u8) -> SocketAddr {
        SocketAddr::from(([192, 0, 2, last], 443))
    }

    fn v6(last: u16) -> SocketAddr {
        SocketAddr::from(([0x2001, 0xdb8, 0, 0, 0, 0, 0, last], 443))
    }

    #[test]
    fn families_alternate_from_the_resolvers_first_choice() {
        assert_eq!(
            interleave(vec![v6(1), v6(2), v6(3), v4(1)]),
            vec![v6(1), v4(1), v6(2), v6(3)]
        );
        assert_eq!(
            interleave(vec![v4(1), v4(2), v6(1)]),
            vec![v4(1), v6(1), v4(2)]
        );
        assert!(interleave(Vec::new()).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_ipv6_address_costs_one_attempt_delay_not_a_timeout() {
        // The VPN case: the AAAA address never answers and the A address does.
        let started = tokio::time::Instant::now();
        let won = race(vec![v6(1), v4(1)], ATTEMPT_DELAY, |address| async move {
            if address.is_ipv6() {
                std::future::pending::<()>().await;
            }
            Ok::<_, io::Error>(address)
        })
        .await
        .unwrap();
        assert_eq!(won, v4(1));
        assert_eq!(started.elapsed(), ATTEMPT_DELAY);
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_address_moves_on_without_waiting() {
        let started = tokio::time::Instant::now();
        let won = race(vec![v6(1), v4(1)], ATTEMPT_DELAY, |address| async move {
            if address.is_ipv6() {
                return Err(io::Error::from(io::ErrorKind::ConnectionRefused));
            }
            Ok(address)
        })
        .await
        .unwrap();
        assert_eq!(won, v4(1));
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn every_address_failing_reports_the_last_error() {
        let error = race(vec![v6(1), v4(1)], ATTEMPT_DELAY, |_| async {
            Err::<(), _>(io::Error::from(io::ErrorKind::ConnectionRefused))
        })
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[tokio::test]
    async fn no_address_is_an_error_not_a_hang() {
        let error = race(Vec::new(), ATTEMPT_DELAY, |address| async move {
            Ok::<_, io::Error>(address)
        })
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
