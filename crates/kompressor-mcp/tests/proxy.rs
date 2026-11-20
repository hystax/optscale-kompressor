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

/// A small MCP server, written as a shell script rather than a second binary
/// so the test is self-contained: the protocol is small enough to answer by
/// hand.
///
/// Its tools each exercise one path through the proxy:
///
/// * `big_result` — one large, compressible text block of `lines` lines.
/// * `small_result` — one short text block, under any elision threshold.
/// * `with_image` — an image block beside a large text block.
/// * `echo` — the request line it received, as text, so a test can see what
///   was forwarded.
/// * `whoami` — its arguments and two environment variables, so a test can see
///   how it was spawned.
/// * `die` — exits without answering, as a crashed server would.
/// * `slow` — answers two seconds later, and goes on serving meanwhile.
///
/// Spawned as `bash <script>` (see [`spawned`]), never as the script itself.
fn fake_server(dir: &std::path::Path, lines: usize) -> std::path::PathBuf {
    let path = dir.join("fake-mcp-server.sh");
    let script = format!(
        r#"#!/bin/bash
# One JSON-RPC message per line, as the transport frames.
emit() {{ printf '%s\n' "$1"; }}
text() {{ emit '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"content":[{{"type":"text","text":"'"$1"'"}}],"isError":false}}}}'; }}
big() {{ for i in $(seq 1 {lines}); do printf 'WARN worker-%s: connection refused from 10.0.0.%s after %sms\\n' "$i" "$((i % 251))" "$((i * 7))"; done; }}
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      emit '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"protocolVersion":"2024-11-05","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"fake","version":"0"}}}}}}' ;;
    *'"method":"notifications/initialized"'*) ;;
    *'"method":"tools/list"'*)
      tools=''
      for name in big_result small_result with_image echo whoami die slow; do
        tools="$tools"'{{"name":"'"$name"'","description":"fake '"$name"'","inputSchema":{{"type":"object"}}}},'
      done
      emit '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"tools":['"${{tools%,}}"']}}}}' ;;
    *'"method":"tools/call"'*'"name":"big_result"'*)
      text "$(big)" ;;
    *'"method":"tools/call"'*'"name":"small_result"'*)
      text 'all good' ;;
    *'"method":"tools/call"'*'"name":"with_image"'*)
      emit '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"content":[{{"type":"image","data":"iVBORw0KGgo=","mimeType":"image/png"}},{{"type":"text","text":"'"$(big)"'"}}],"isError":false}}}}' ;;
    *'"method":"tools/call"'*'"name":"echo"'*)
      text "$(printf '%s' "$line" | sed 's/\\/\\\\/g; s/"/\\"/g')" ;;
    *'"method":"tools/call"'*'"name":"whoami"'*)
      text "args=$* fronted=${{KOMPRESSOR_MCP_FRONTED-unset}} extra=${{FAKE_EXTRA-unset}}" ;;
    *'"method":"tools/call"'*'"name":"die"'*)
      exit 3 ;;
    *'"method":"tools/call"'*'"name":"slow"'*)
      ( sleep 2; text 'late' ) & ;;
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
        &[{
            let (command, args) = spawned(&script);
            McpServerConfig {
                name: "fake".to_string(),
                command,
                args,
                compress: true,
                ..Default::default()
            }
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

/// The command and arguments that start a [`fake_server`] script.
///
/// Through `bash` rather than by executing the script, because the tests run in
/// parallel threads of one process: a script written by one test is executed
/// while another is spawning, and a child forked in between inherits the
/// writer's descriptor and makes `exec` fail with `ETXTBSY`. The server then
/// fails to start and is dropped — measured once under a full workspace run as
/// `fronted() == 0` in 0.32 s, far inside its 60 s startup timeout. `bash` is
/// never written to, and reads the script as data.
fn spawned(script: &std::path::Path) -> (Option<String>, Vec<String>) {
    (
        Some("bash".to_string()),
        vec![script.to_string_lossy().into_owned()],
    )
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
    let (command, args) = spawned(&script);
    config.mcp.servers = vec![McpServerConfig {
        name: "fake".to_string(),
        command,
        args,
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

/// A client connected to proxy mode fronting `servers`, as an agent runtime
/// would be.
async fn session(
    servers: Vec<McpServerConfig>,
) -> (
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let mut config = Config::default();
    config.stats.enabled = false;
    config.engine.mode = "recoverable".parse().unwrap_or_default();
    config.mcp.servers = servers;
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
    (client, serving)
}

fn fake(script: &std::path::Path, compress: bool) -> McpServerConfig {
    let (command, args) = spawned(script);
    McpServerConfig {
        name: "fake".to_string(),
        command,
        args,
        compress,
        ..Default::default()
    }
}

fn request(name: &str, arguments: Option<serde_json::Value>) -> rmcp::model::CallToolRequestParams {
    let mut params = rmcp::model::CallToolRequestParams::default();
    params.name = name.to_string().into();
    params.arguments = arguments.map(|a| a.as_object().cloned().expect("an object"));
    params
}

/// Every text block of a result, in order.
fn texts(result: &rmcp::model::CallToolResult) -> Vec<String> {
    result
        .content
        .iter()
        .filter_map(|b| match b {
            rmcp::model::ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect()
}

/// `compress = false` is a promise to forward that server's results as they
/// came, however large.
#[tokio::test]
async fn a_server_with_compress_off_is_forwarded_untouched() {
    let dir = tempdir("nocompress");
    let script = fake_server(&dir, 400);
    let (on, on_serving) = session(vec![fake(&script, true)]).await;
    let (off, off_serving) = session(vec![fake(&script, false)]).await;

    let compressed = texts(
        &on.call_tool(request("fake__big_result", None))
            .await
            .unwrap(),
    );
    let forwarded = texts(
        &off.call_tool(request("fake__big_result", None))
            .await
            .unwrap(),
    );

    assert_eq!(forwarded.len(), 1);
    assert_eq!(forwarded[0].lines().count(), 400, "every line arrives");
    assert!(!forwarded[0].contains("ref:k_"), "nothing was elided");
    assert!(
        compressed[0].len() < forwarded[0].len(),
        "the same result is compressed when compress is on, so the comparison means something"
    );

    for client in [on, off] {
        let _ = client.cancel().await;
    }
    on_serving.abort();
    off_serving.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A result too small to be worth eliding comes back byte-identical.
#[tokio::test]
async fn a_small_result_is_byte_identical() {
    let dir = tempdir("small");
    let script = fake_server(&dir, 1);
    let (client, serving) = session(vec![fake(&script, true)]).await;

    let result = client
        .call_tool(request("fake__small_result", None))
        .await
        .unwrap();
    assert_eq!(texts(&result), ["all good"]);
    assert_eq!(result.is_error, Some(false));

    let _ = client.cancel().await;
    serving.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Only text is compressed; an image beside it is forwarded as it came.
#[tokio::test]
async fn a_non_text_block_passes_through_untouched() {
    let dir = tempdir("image");
    let script = fake_server(&dir, 400);
    let (client, serving) = session(vec![fake(&script, true)]).await;

    let result = client
        .call_tool(request("fake__with_image", None))
        .await
        .unwrap();
    assert_eq!(result.content.len(), 2, "no block added or dropped");
    match &result.content[0] {
        rmcp::model::ContentBlock::Image(image) => {
            assert_eq!(image.data, "iVBORw0KGgo=");
            assert_eq!(image.mime_type, "image/png");
        }
        other => panic!("the image block was replaced: {other:?}"),
    }
    let text = &texts(&result)[0];
    assert!(
        text.contains("ref:k_"),
        "the text beside it is still compressed: {text}"
    );

    let _ = client.cancel().await;
    serving.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Arguments reach the fronted server as the model sent them: same keys, same
/// order, same number spelling.
#[tokio::test]
async fn arguments_are_forwarded_as_sent() {
    let dir = tempdir("echo");
    let script = fake_server(&dir, 1);
    // Compression off, so the echoed request is read back exactly.
    let (client, serving) = session(vec![fake(&script, false)]).await;

    let arguments = r#"{"zeta":1,"alpha":"x \"quoted\"","big":12345678901234567890.50,"nested":{"b":[true,null],"a":{}}}"#;
    let result = client
        .call_tool(request(
            "fake__echo",
            Some(serde_json::from_str(arguments).unwrap()),
        ))
        .await
        .unwrap();
    let line = &texts(&result)[0];
    let received: serde_json::Value = serde_json::from_str(line).expect("the echoed request");

    assert_eq!(received["params"]["name"], "echo", "the prefix is stripped");
    assert_eq!(
        serde_json::to_string(&received["params"]["arguments"]).unwrap(),
        arguments,
        "the arguments were rewritten on the way through"
    );

    let _ = client.cancel().await;
    serving.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A spawned server gets its configured `args` and `env`, and the recursion
/// marker, which a configured `env` cannot clear.
#[tokio::test]
async fn a_spawned_server_gets_its_args_env_and_the_marker() {
    let dir = tempdir("whoami");
    let script = fake_server(&dir, 1);
    let mut server = fake(&script, false);
    // After the script, which is `bash`'s first argument.
    server.args.extend(["--one".to_string(), "two".to_string()]);
    server.env = [
        ("FAKE_EXTRA".to_string(), "configured".to_string()),
        (
            kompressor_mcp::proxy::FRONTED_MARKER.to_string(),
            String::new(),
        ),
    ]
    .into_iter()
    .collect();
    let (client, serving) = session(vec![server]).await;

    let result = client
        .call_tool(request("fake__whoami", None))
        .await
        .unwrap();
    assert_eq!(
        texts(&result),
        ["args=--one two fronted=1 extra=configured"]
    );

    let _ = client.cancel().await;
    serving.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A server that dies mid-call costs that call and its own tools, and nothing
/// else: the model gets a tool error it can report, and ours keep working.
#[tokio::test]
async fn a_server_that_dies_mid_call_is_a_tool_error_not_a_dead_session() {
    let dir = tempdir("die");
    let script = fake_server(&dir, 1);
    let (client, serving) = session(vec![fake(&script, true)]).await;

    let result = client.call_tool(request("fake__die", None)).await.unwrap();
    assert_eq!(result.is_error, Some(true));
    let message = &texts(&result)[0];
    assert!(
        message.starts_with(r#"mcp server "fake" could not run "die": "#),
        "{message}"
    );

    let listed = client
        .list_all_tools()
        .await
        .expect("tools/list still answers");
    let names: Vec<&str> = listed.iter().map(|t| t.name.as_ref()).collect();
    assert!(
        names.contains(&"optscale_kompressor_retrieve"),
        "ours survive: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.starts_with("fake__")),
        "a dead server lists nothing: {names:?}"
    );

    let again = client
        .call_tool(request("fake__small_result", None))
        .await
        .unwrap();
    assert_eq!(again.is_error, Some(true), "a later call is an error too");

    let ours = client
        .call_tool(request(
            "optscale_kompressor_compress_raw",
            Some(serde_json::json!({ "content": "still here" })),
        ))
        .await
        .expect("our own tools still answer");
    assert_ne!(ours.is_error, Some(true), "{ours:?}");

    let _ = client.cancel().await;
    serving.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A call that takes its time holds up nothing else: the tool list, another
/// call to the same server and our own tools are all answered while it runs,
/// and it is answered when it is done. Calls to a server used to queue behind
/// a lock held for the whole call, so one that never returned took the
/// server's tools — and with them every `tools/list` — out of the session.
#[tokio::test]
async fn a_slow_call_holds_up_nothing_else() {
    let dir = tempdir("slow");
    let script = fake_server(&dir, 10);
    let (client, serving) = session(vec![fake(&script, true)]).await;

    let started = std::time::Instant::now();
    let slow = client.call_tool(request("fake__slow", None));
    let meanwhile = async {
        // Let the slow call reach the server first.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let listed = client.list_all_tools().await.expect("tools/list");
        assert!(
            listed.iter().any(|tool| tool.name == "fake__slow"),
            "the fronted server's tools are listed while it is busy"
        );
        let quick = client
            .call_tool(request("fake__small_result", None))
            .await
            .expect("a second call to the same server");
        assert_eq!(texts(&quick), ["all good"]);
        let ours = client
            .call_tool(request(
                "optscale_kompressor_retrieve",
                Some(serde_json::json!({"ref": "k_AbC123dEf456"})),
            ))
            .await
            .expect("one of ours");
        assert!(!texts(&ours).is_empty());
        started.elapsed()
    };
    let (slow, answered) = tokio::join!(slow, meanwhile);
    assert!(
        answered < std::time::Duration::from_millis(1_500),
        "everything else waited for the slow call: {answered:?}"
    );
    assert_eq!(texts(&slow.expect("the slow call itself")), ["late"]);
    assert!(started.elapsed() >= std::time::Duration::from_secs(2));

    client.cancel().await.ok();
    let _ = serving.await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A name with no fronted server's prefix is one of ours, and is never
/// forwarded anywhere -- including a prefix that merely looks like one.
#[tokio::test]
async fn an_unknown_prefix_is_answered_locally_not_forwarded() {
    let dir = tempdir("unknown");
    let script = fake_server(&dir, 1);
    let (client, serving) = session(vec![fake(&script, true)]).await;

    let error = client
        .call_tool(request("other__small_result", None))
        .await
        .expect_err("no such tool, here or anywhere");
    // Our own router's refusal, not a forwarded call's tool error.
    match error {
        rmcp::ServiceError::McpError(data) => {
            assert_eq!(data.code, rmcp::model::ErrorCode::INVALID_PARAMS);
            assert_eq!(data.message, "tool not found");
        }
        other => panic!("not refused locally: {other}"),
    }

    let _ = client.cancel().await;
    serving.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A name already used by an earlier entry is skipped, so each prefix belongs
/// to exactly one server and its tools are listed once.
#[tokio::test]
async fn a_second_server_with_the_same_name_is_skipped() {
    let dir = tempdir("duplicate");
    let script = fake_server(&dir, 1);
    let core = core();
    let proxy = ProxyTools::connect(
        tools(core.clone()),
        core,
        &[fake(&script, true), fake(&script, false)],
    )
    .await;
    assert_eq!(proxy.fronted(), 1);

    let (client, serving) = session(vec![fake(&script, true), fake(&script, false)]).await;
    let listed = client.list_all_tools().await.unwrap();
    let small = listed
        .iter()
        .filter(|t| t.name.as_ref() == "fake__small_result")
        .count();
    assert_eq!(small, 1, "listed once, not once per entry");

    let _ = client.cancel().await;
    serving.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Which entry owns a name does not depend on which one managed to start: the
/// first owns it, and if the first cannot start, the name is unserved.
#[tokio::test]
async fn a_duplicate_is_skipped_even_when_the_first_failed() {
    let dir = tempdir("duplicate-failed");
    let script = fake_server(&dir, 1);
    let core = core();
    let broken = McpServerConfig {
        name: "fake".to_string(),
        command: Some("/nonexistent/mcp-server".to_string()),
        ..Default::default()
    };
    let proxy =
        ProxyTools::connect(tools(core.clone()), core, &[broken, fake(&script, true)]).await;
    assert_eq!(proxy.fronted(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A server that starts and never answers costs its own tools after its
/// timeout, not the session.
#[tokio::test]
async fn a_server_that_never_answers_does_not_hold_the_session() {
    let dir = tempdir("hang");
    let script = fake_server(&dir, 1);
    let hang = McpServerConfig {
        name: "hang".to_string(),
        command: Some("sleep".to_string()),
        args: vec!["30".to_string()],
        startup_timeout: std::time::Duration::from_millis(300),
        ..Default::default()
    };

    let started = std::time::Instant::now();
    let (client, serving) = session(vec![hang, fake(&script, true)]).await;
    let listed = client.list_all_tools().await.unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "the session waited {:?} for a server that never answered",
        started.elapsed()
    );
    let names: Vec<&str> = listed.iter().map(|t| t.name.as_ref()).collect();
    assert!(names.contains(&"optscale_kompressor_retrieve"), "{names:?}");
    assert!(
        names.contains(&"fake__small_result"),
        "the server after it: {names:?}"
    );
    assert!(!names.iter().any(|n| n.starts_with("hang__")), "{names:?}");

    let _ = client.cancel().await;
    serving.abort();
    let _ = std::fs::remove_dir_all(&dir);
}
