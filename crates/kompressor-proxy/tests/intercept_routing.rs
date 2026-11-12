//! An intercepted request stays on the host its client asked for.
//!
//! The intercept listener tags each request with the origin from its
//! `CONNECT`; the router then only considers mounts that send to that origin.
//! Without it, Claude Code's `api.anthropic.com/v1/code/sessions` matched
//! Codex's `/v1` mount and reached `chatgpt.com` with an Anthropic token.

mod support;

use axum::body::Body;
use http::{Request, StatusCode};
use kompressor_core::config::Upstream;
use kompressor_proxy::intercept::InterceptedOrigin;
use support::Harness;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

/// The mock is the origin the client asked for, mounted as a catch-all; a
/// longer `/v1` mount points at a host nobody asked for, so path-only routing
/// would choose it.
async fn harness() -> Harness {
    let harness = Harness::with(|config| {
        config.proxy.upstreams.push(Upstream {
            name: "decoy".to_string(),
            path_prefix: "/v1".to_string(),
            base_url: "http://decoy.invalid".to_string(),
            api_family: "passthrough".to_string(),
            forward_auth: true,
            ..Default::default()
        });
    })
    .await;
    Mock::given(method("GET"))
        .and(path("/v1/code/sessions"))
        .respond_with(ResponseTemplate::new(200).set_body_string("from the asked-for host"))
        .mount(&harness.server)
        .await;
    harness
}

fn origin_of(harness: &Harness) -> InterceptedOrigin {
    let address = harness.server.address();
    InterceptedOrigin {
        host: address.ip().to_string(),
        port: address.port(),
    }
}

#[tokio::test]
async fn an_intercepted_request_reaches_the_host_it_named() {
    let harness = harness().await;
    let mut request = Request::get("/v1/code/sessions")
        .header("authorization", "Bearer sk-ant-oat01-EXAMPLE")
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(origin_of(&harness));

    let reply = harness.send(request).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.text(), "from the asked-for host");
    let received = harness.server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(
        received[0].headers["authorization"],
        "Bearer sk-ant-oat01-EXAMPLE"
    );
}

#[tokio::test]
async fn a_request_sent_to_the_proxy_directly_is_still_routed_by_path() {
    // No origin tag: the longer `/v1` mount wins, as it always has, and the
    // decoy host is unreachable.
    let harness = harness().await;
    let request = Request::get("/v1/code/sessions")
        .body(Body::empty())
        .unwrap();

    let reply = harness.send(request).await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY, "{}", reply.text());
    assert!(harness.server.received_requests().await.unwrap().is_empty());
}
