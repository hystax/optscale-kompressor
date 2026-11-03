//! The intercept listener, end to end.
//!
//! The unit tests cover parsing, matching and the authority. What they cannot
//! show is the thing the module exists for: that a client which believes it is
//! talking to a provider reaches our router instead, and that a client talking
//! to anything else is not touched. Both are checked here against a real
//! socket, a real handshake and a real `CONNECT`.

use std::path::PathBuf;

use axum::Router;
use axum::routing::get;
use kompressor_proxy::intercept::Intercept;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kompressor-intercept-{name}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A router that says who answered, so a response cannot be mistaken for one
/// from somewhere else.
fn router() -> Router {
    Router::new().route("/v1/messages", get(|| async { "served by the router" }))
}

#[tokio::test]
async fn an_intercepted_host_reaches_our_router() {
    let ca_dir = temp_dir("router");
    let intercept = Intercept::bind(
        "127.0.0.1:0",
        &["api.anthropic.com".to_string()],
        &ca_dir,
        &["http/1.1".to_string()],
        8,
    )
    .await
    .expect("bind");
    let proxy = intercept.address();
    let ca_pem = std::fs::read(intercept.certificate_path()).expect("ca pem");

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(intercept.serve(router(), async move {
        let _ = stop_rx.await;
    }));

    // A client that believes it is talking to the provider: it resolves and
    // trusts nothing of ours except the CA, and names the provider's host.
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("http://{proxy}")).expect("proxy"))
        .add_root_certificate(reqwest::Certificate::from_pem(&ca_pem).expect("cert"))
        // Nothing listens on the real address in this test, and nothing needs
        // to: an intercepted connection never reaches an origin.
        .danger_accept_invalid_certs(false)
        .build()
        .expect("client");

    let response = client
        .get("https://api.anthropic.com/v1/messages")
        .send()
        .await
        .expect("request");
    assert!(response.status().is_success(), "{:?}", response.status());
    assert_eq!(response.text().await.unwrap(), "served by the router");

    let _ = stop_tx.send(());
    let _ = std::fs::remove_dir_all(&ca_dir);
}

#[tokio::test]
async fn any_other_host_is_tunnelled_untouched() {
    // An origin that is not HTTP at all, so anything that reads or rewrites
    // the stream would corrupt it.
    let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin_addr = origin.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = origin.accept().await.unwrap();
        let mut buf = [0u8; 4];
        socket.read_exact(&mut buf).await.unwrap();
        socket.write_all(b"pong").await.unwrap();
    });

    let ca_dir = temp_dir("tunnel");
    let intercept = Intercept::bind(
        "127.0.0.1:0",
        &["api.anthropic.com".to_string()],
        &ca_dir,
        &["http/1.1".to_string()],
        8,
    )
    .await
    .expect("bind");
    let proxy = intercept.address();

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(intercept.serve(router(), async move {
        let _ = stop_rx.await;
    }));

    let mut stream = TcpStream::connect(proxy).await.unwrap();
    stream
        .write_all(
            format!("CONNECT {origin_addr} HTTP/1.1\r\nHost: {origin_addr}\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();

    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    assert!(
        String::from_utf8_lossy(&head).starts_with("HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&head)
    );

    // Bytes that are not TLS and not HTTP cross unchanged.
    stream.write_all(b"ping").await.unwrap();
    let mut pong = [0u8; 4];
    stream.read_exact(&mut pong).await.unwrap();
    assert_eq!(&pong, b"pong");

    let _ = stop_tx.send(());
    let _ = std::fs::remove_dir_all(&ca_dir);
}

#[tokio::test]
async fn a_request_that_is_not_connect_is_refused_in_http() {
    let ca_dir = temp_dir("refuse");
    let intercept = Intercept::bind(
        "127.0.0.1:0",
        &["api.anthropic.com".to_string()],
        &ca_dir,
        &["http/1.1".to_string()],
        8,
    )
    .await
    .expect("bind");
    let proxy = intercept.address();

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(intercept.serve(router(), async move {
        let _ = stop_rx.await;
    }));

    // Somebody opened the port in a browser. A closed socket would read as a
    // dead process; a status line says what happened.
    let mut stream = TcpStream::connect(proxy).await.unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).await.unwrap();
    assert!(answer.starts_with("HTTP/1.1 405"), "{answer}");

    let _ = stop_tx.send(());
    let _ = std::fs::remove_dir_all(&ca_dir);
}
