//! Connecting to a fronted server, and surviving its death.
//!
//! A fronted server is either spawned and spoken to on its pipe, or connected
//! to over streamable HTTP. Either way it is somebody else's process: it can
//! fail to start, exit mid-call, or never have existed. None of that may take
//! the session with it — a supervisor that drops every tool because one wrapped
//! server crashed turns a degraded assistant into a dead one.

use std::sync::Arc;

use kompressor_core::config::McpServerConfig;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, Tool};
use rmcp::service::{RoleClient, RunningService};
use tokio::sync::Mutex;

/// Set on every server this process spawns, and checked before fronting
/// anything.
///
/// Without it, a configuration whose fronted server is this same binary reading
/// this same configuration recurses without bound: the child inherits
/// `KOMPRESSOR_CONFIG`, reads `[[mcp.servers]]`, sees itself, and spawns
/// another. Measured the hard way — 1,796 processes before the machine ran out
/// of forks. Nothing in the configuration looks wrong, which is what makes it
/// worth a guard rather than a warning in the documentation.
///
/// One level is all proxy mode needs: a fronted server serves tools, it does
/// not front further servers.
pub const FRONTED_MARKER: &str = "KOMPRESSOR_MCP_FRONTED";

/// Whether this process was itself spawned as somebody's fronted server.
///
/// True means `[[mcp.servers]]` is ignored and the plain tool server is served
/// instead, which is also the right behaviour on its own terms: a server that
/// was launched to provide tools should provide them, not reorganise its
/// caller's topology.
pub fn is_fronted() -> bool {
    std::env::var_os(FRONTED_MARKER).is_some()
}

/// Why a fronted server could not be reached.
#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error("mcp server {name:?} names neither command nor url")]
    Unconfigured { name: String },
    #[error("mcp server {name:?} names both command and url; pick one")]
    Overconfigured { name: String },
    /// Fronting a server over HTTP needs rmcp's reqwest client transport, which
    /// requires `reqwest >= 0.13.2`; this workspace pins 0.13.1 for the proxy's
    /// own client. Bumping that for a secondary transport is not a trade worth
    /// making mid-feature, so `url` is refused with this rather than silently
    /// ignored. A spawned server covers the case this mode was built for.
    #[error("mcp server {name:?}: url is not supported yet; use command")]
    UrlUnsupported { name: String },
    #[error("mcp server {name:?} has no name")]
    Unnamed { name: String },
    #[error("could not start mcp server {name:?}: {source}")]
    Start {
        name: String,
        #[source]
        source: std::io::Error,
    },
    #[error("mcp server {name:?} failed to initialize: {reason}")]
    Initialize { name: String, reason: String },
}

/// One fronted server, and whether its results are compressed.
pub struct Upstream {
    pub name: String,
    pub compress: bool,
    service: Arc<Mutex<Option<RunningService<RoleClient, ()>>>>,
}

impl Upstream {
    /// Validate the configuration and connect.
    ///
    /// Validation is separate from connecting and happens first, so a typo is a
    /// startup error naming the server rather than a tool that mysteriously
    /// does not appear.
    pub async fn connect(config: &McpServerConfig) -> Result<Self, UpstreamError> {
        let name = config.name.trim().to_string();
        if name.is_empty() {
            return Err(UpstreamError::Unnamed { name });
        }
        match (&config.command, &config.url) {
            (None, None) => return Err(UpstreamError::Unconfigured { name }),
            (Some(_), Some(_)) => return Err(UpstreamError::Overconfigured { name }),
            _ => {}
        }

        let service = if let Some(command) = &config.command {
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(&config.args);
            for (key, value) in &config.env {
                cmd.env(key, value);
            }
            // The recursion guard. Set last so a configured `env` cannot
            // unset it.
            cmd.env(FRONTED_MARKER, "1");
            let transport = rmcp::transport::TokioChildProcess::new(cmd).map_err(|source| {
                UpstreamError::Start {
                    name: name.clone(),
                    source,
                }
            })?;
            ().serve(transport)
                .await
                .map_err(|e| UpstreamError::Initialize {
                    name: name.clone(),
                    reason: e.to_string(),
                })?
        } else {
            return Err(UpstreamError::UrlUnsupported { name });
        };

        Ok(Upstream {
            name,
            compress: config.compress,
            service: Arc::new(Mutex::new(Some(service))),
        })
    }

    /// This server's tools, or none if it has died.
    ///
    /// A dead server contributes an empty list rather than failing the whole
    /// `tools/list`: the client keeps every other server's tools and ours.
    pub async fn tools(&self) -> Vec<Tool> {
        let guard = self.service.lock().await;
        let Some(service) = guard.as_ref() else {
            return Vec::new();
        };
        match service.list_all_tools().await {
            Ok(tools) => tools,
            Err(error) => {
                tracing::warn!(server = %self.name, %error, "mcp server did not list its tools");
                Vec::new()
            }
        }
    }

    /// Forward one call.
    pub async fn call(
        &self,
        tool: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<CallToolResult, String> {
        let guard = self.service.lock().await;
        let Some(service) = guard.as_ref() else {
            return Err(format!("mcp server {:?} is not running", self.name));
        };
        // `CallToolRequestParams` is `#[non_exhaustive]`, so it is built by
        // assignment rather than as a literal; new protocol fields then default
        // rather than failing this build.
        let mut params = CallToolRequestParams::default();
        params.name = tool.to_string().into();
        params.arguments = arguments;
        service.call_tool(params).await.map_err(|e| e.to_string())
    }
}
