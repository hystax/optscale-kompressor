//! The blind relay, for everything this listener does not intercept.
//!
//! A tunnelled connection is copied byte for byte and never decrypted. That is
//! the whole point: a client configured to use this proxy sends all its
//! traffic here, and all of it that is not on the allow list has to reach its
//! origin unexamined.

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

/// Connect to `target` and copy in both directions until either side closes.
///
/// Errors are returned rather than logged here so the caller can report them
/// with the connection's context attached.
pub async fn relay<S>(client: &mut S, target: &str) -> std::io::Result<(u64, u64)>
where
    S: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    let mut upstream = TcpStream::connect(target).await?;
    tokio::io::copy_bidirectional(client, &mut upstream).await
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
        let target = addr.to_string();
        let relayed = tokio::spawn(async move { relay(&mut proxy_side, &target).await });

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
        let (_, mut proxy_side) = tokio::io::duplex(64);
        // Port 1 on loopback has nothing on it.
        assert!(relay(&mut proxy_side, "127.0.0.1:1").await.is_err());
    }
}
