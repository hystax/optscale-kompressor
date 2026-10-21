//! Proxy mode, end to end, against a fake MCP server.
//!
//! The unit tests cover prefixing and routing. What they cannot show is the
//! thing the mode exists for: that a large tool result from somebody else's
//! server comes back elided, and that the handle in the marker resolves to the
//! bytes that were taken out.
//!
//! The fronted server here is a real child process speaking JSON-RPC on its
//! pipe — `cat` would not do, because initialize has to be answered.

use std::sync::Arc;

use kompressor_core::AppCore;
use kompressor_core::config::{Config, McpServerConfig};
use kompressor_mcp::handler::KompressorTools;
use kompressor_mcp::limit::RetrievalLimiter;
use kompressor_mcp::proxy::ProxyTools;
use kompressor_store::memory::MemoryRefStore;

/// A server that returns one large, highly compressible text block.
///
/// Written as a shell script rather than a second binary so the test is
/// self-contained: the protocol is small enough to answer by hand.
fn fake_server(dir: &std::path::Path, lines: usize) -> std::path::PathBuf {
    let path = dir.join("fake-mcp-server.sh");
    let script = format!(
        r#"#!/bin/bash
# A minimal MCP server: answers initialize, lists one tool, and returns a
# large result for it. One JSON-RPC message per line, as the transport frames.
emit() {{ printf '%s\n' "$1"; }}
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      emit '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"protocolVersion":"2024-11-05","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"fake","version":"0"}}}}}}' ;;
    *'"method":"notifications/initialized"'*) ;;
    *'"method":"tools/list"'*)
      emit '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"tools":[{{"name":"big_result","description":"returns a lot","inputSchema":{{"type":"object"}}}}]}}}}' ;;
    *'"method":"tools/call"'*)
      body=$(for i in $(seq 1 {lines}); do printf 'WARN worker-%s: connection refused from 10.0.0.%s after %sms\\n' "$i" "$((i % 251))" "$((i * 7))"; done)
      payload=$(printf '%s' "$body" | sed ':a;N;$!ba;s/\n/\\n/g')
      emit '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"content":[{{"type":"text","text":"'"$payload"'"}}],"isError":false}}}}' ;;
    *'"method":"ping"'*)
      emit '{{"jsonrpc":"2.0","id":'"$id"',"result":{{}}}}' ;;
  esac
done
"#
    );
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

fn core() -> Arc<AppCore> {
    let mut config = Config::default();
    config.stats.enabled = false;
    config.engine.mode = "recoverable".parse().unwrap_or_default();
    Arc::new(AppCore::new(
        config,
        Arc::new(MemoryRefStore::new()) as Arc<dyn kompressor_store::types::RefStore>,
        kompressor_stats::writer::null_sink(),
    ))
}

fn tools(core: Arc<AppCore>) -> KompressorTools {
    KompressorTools::for_identity(
        core,
        Arc::new(RetrievalLimiter::new(1_000)),
        kompressor_core::auth::Identity::anonymous(),
    )
}

#[tokio::test]
async fn a_fronted_server_contributes_its_tools_under_a_prefix() {
    let dir = tempdir("list");
    let script = fake_server(&dir, 4);
    let core = core();
    let proxy = ProxyTools::connect(
        tools(core.clone()),
        core,
        &[McpServerConfig {
            name: "fake".to_string(),
            command: Some(script.to_string_lossy().into_owned()),
            compress: true,
            ..Default::default()
        }],
    )
    .await;

    assert_eq!(proxy.fronted(), 1, "the fake server was not fronted");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_server_that_cannot_start_costs_only_its_own_tools() {
    let core = core();
    let proxy = ProxyTools::connect(
        tools(core.clone()),
        core,
        &[McpServerConfig {
            name: "missing".to_string(),
            command: Some("/nonexistent/mcp-server".to_string()),
            ..Default::default()
        }],
    )
    .await;

    // Nothing fronted, but the session exists and our own tools still work.
    assert_eq!(proxy.fronted(), 0);
}

#[tokio::test]
async fn a_url_upstream_is_refused_rather_than_ignored() {
    let core = core();
    let proxy = ProxyTools::connect(
        tools(core.clone()),
        core,
        &[McpServerConfig {
            name: "remote".to_string(),
            url: Some("https://example.invalid/mcp".to_string()),
            ..Default::default()
        }],
    )
    .await;
    assert_eq!(
        proxy.fronted(),
        0,
        "an unsupported transport must not appear to be fronted"
    );
}

fn tempdir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kompressor-mcp-proxy-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The claim the mode rests on: a fronted server's large result arrives
/// elided, and the handle in it resolves to what was taken out.
#[tokio::test]
async fn a_large_fronted_result_comes_back_elided_and_retrievable() {
    let dir = tempdir("elide");
    // Lines that vary, deliberately. Four hundred *identical* lines fold to
    // one line and a count, which is lossless and therefore carries no handle --
    // correct behaviour, and useless for testing retrieval. Varying them forces
    // an elision that has something to give back.
    let script = fake_server(&dir, 400);

    let mut config = Config::default();
    config.stats.enabled = false;
    // Recoverable, or an elision retains nothing and the handle in the marker
    // resolves to an explanation instead of the bytes.
    config.engine.mode = "recoverable".parse().unwrap_or_default();
    config.mcp.servers = vec![McpServerConfig {
        name: "fake".to_string(),
        command: Some(script.to_string_lossy().into_owned()),
        compress: true,
        ..Default::default()
    }];
    let core = Arc::new(AppCore::new(
        config,
        Arc::new(MemoryRefStore::new()) as Arc<dyn kompressor_store::types::RefStore>,
        kompressor_stats::writer::null_sink(),
    ));

    // A client on the other end of a duplex, as an agent runtime would be.
    let (client_side, server_side) = tokio::io::duplex(1 << 20);
    let serving = tokio::spawn(kompressor_mcp::serve_proxy_pipe(
        core.clone(),
        kompressor_core::auth::Identity::anonymous(),
        server_side,
    ));

    let client = rmcp::ServiceExt::serve((), client_side)
        .await
        .expect("client handshake");

    let listed = client.list_all_tools().await.expect("tools/list");
    assert!(
        listed.iter().any(|t| t.name.as_ref() == "fake__big_result"),
        "fronted tool missing: {:?}",
        listed.iter().map(|t| t.name.as_ref()).collect::<Vec<_>>()
    );
    assert!(
        listed
            .iter()
            .any(|t| t.name.as_ref() == "optscale_kompressor_retrieve"),
        "retrieve must be listed beside the results it resolves"
    );

    let mut params = rmcp::model::CallToolRequestParams::default();
    params.name = "fake__big_result".to_string().into();
    let result = client.call_tool(params).await.expect("tools/call");

    let text = result
        .content
        .iter()
        .find_map(|b| match b {
            rmcp::model::ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .expect("a text block");

    assert!(
        text.len() < 4_000,
        "result was not compressed: {} chars",
        text.len()
    );
    assert!(
        text.contains("ref:k_"),
        "no handle in the marker, so nothing can fetch the lines back: {text}"
    );

    // And the handle resolves, through the same process the proxy ran in.
    let reference = text
        .split("ref:k_")
        .nth(1)
        // A handle is [A-Za-z0-9_-]{12}; stopping at '-' truncates it.
        .and_then(|rest| {
            rest.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
                .next()
        })
        .map(|r| format!("k_{r}"))
        .expect("a parseable handle");
    let mut retrieve = rmcp::model::CallToolRequestParams::default();
    retrieve.name = "optscale_kompressor_retrieve".to_string().into();
    retrieve.arguments = Some(
        serde_json::json!({ "ref": reference })
            .as_object()
            .cloned()
            .unwrap(),
    );
    let fetched = client.call_tool(retrieve).await.expect("retrieve");
    let fetched_text = serde_json::to_string(&fetched).unwrap();
    assert!(
        fetched_text.contains("connection refused"),
        "the handle did not resolve to the elided lines: {fetched_text}"
    );

    let _ = client.cancel().await;
    serving.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The same loop against a real rmcp server rather than the hand-rolled fake.
///
/// The fake answers only what this test needs, which is enough to check
/// routing and compression but says nothing about protocol negotiation with a
/// server written to the specification. Fronting kompressor's own stdio server
/// closes that gap without depending on anything outside the repository:
/// a real `initialize`, a real `tools/list`, real schemas.
///
/// Skipped rather than failed when the binary has not been built — `cargo test`
/// on a clean tree has no release binary, and a test that demands one turns a
/// first run red for the wrong reason.
#[tokio::test]
async fn fronting_a_real_rmcp_server() {
    let binary =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/release/kompressor");
    if !binary.exists() {
        eprintln!("skipping: {} is not built", binary.display());
        return;
    }

    let mut config = Config::default();
    config.stats.enabled = false;
    config.engine.mode = "recoverable".parse().unwrap_or_default();
    config.mcp.servers = vec![McpServerConfig {
        name: "inner".to_string(),
        command: Some(binary.to_string_lossy().into_owned()),
        args: vec!["mcp".to_string(), "serve".to_string()],
        compress: true,
        ..Default::default()
    }];
    let core = Arc::new(AppCore::new(
        config,
        Arc::new(MemoryRefStore::new()) as Arc<dyn kompressor_store::types::RefStore>,
        kompressor_stats::writer::null_sink(),
    ));

    let (client_side, server_side) = tokio::io::duplex(1 << 20);
    let serving = tokio::spawn(kompressor_mcp::serve_proxy_pipe(
        core,
        kompressor_core::auth::Identity::anonymous(),
        server_side,
    ));
    let client = rmcp::ServiceExt::serve((), client_side)
        .await
        .expect("client handshake");

    let listed = client.list_all_tools().await.expect("tools/list");
    let names: Vec<&str> = listed.iter().map(|t| t.name.as_ref()).collect();

    // Ours, unprefixed, and the inner server's four under its prefix. The
    // collision is the point: both expose `optscale_kompressor_retrieve`, and
    // only ours may answer to that name.
    assert!(
        names.contains(&"optscale_kompressor_retrieve"),
        "ours missing: {names:?}"
    );
    assert!(
        names.contains(&"inner__optscale_kompressor_retrieve"),
        "the fronted server's own retrieve must be reachable, prefixed: {names:?}"
    );
    assert!(
        names.contains(&"inner__optscale_kompressor_compress"),
        "fronted tools missing: {names:?}"
    );

    // And a call routes to it and comes back.
    let mut params = rmcp::model::CallToolRequestParams::default();
    params.name = "inner__optscale_kompressor_stats".to_string().into();
    params.arguments = Some(serde_json::Map::new());
    let result = client.call_tool(params).await.expect("tools/call");
    assert!(
        !result.is_error.unwrap_or(false),
        "fronted call failed: {result:?}"
    );

    let _ = client.cancel().await;
    serving.abort();
}

/// The guard against the recursion that a self-referential configuration
/// creates.
///
/// A fronted server inherits the environment, including `KOMPRESSOR_CONFIG`. If
/// that configuration fronts this same binary, the child reads it, sees itself,
/// and spawns another — unbounded. This asserts the marker that stops it, which
/// is cheaper to check here than to rediscover at 1,796 processes.
#[test]
fn a_fronted_process_refuses_to_front_further_servers() {
    assert!(
        !kompressor_mcp::proxy::is_fronted(),
        "the test runner is not itself a fronted server"
    );
    // SAFETY: single-threaded test, and the variable is read by this crate only.
    unsafe { std::env::set_var(kompressor_mcp::proxy::FRONTED_MARKER, "1") };
    assert!(
        kompressor_mcp::proxy::is_fronted(),
        "the marker must be observable, or the recursion guard does nothing"
    );
    unsafe { std::env::remove_var(kompressor_mcp::proxy::FRONTED_MARKER) };
}
