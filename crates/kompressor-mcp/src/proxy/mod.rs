//! Proxy mode: our tools and somebody else's, with their results compressed.
//!
//! The HTTP proxy compresses what an agent's client sends. Some clients cannot
//! be pointed at it — Claude Desktop's Chat tab among them — and intercepting
//! their inference would buy nothing anyway, because their conversation lives
//! on the server and the request carries a message and an id.
//!
//! What such a client does carry in bulk is **tool results**, and those are
//! machine-facing in exactly the way tool output in a coding agent is.
//! Compressing them needs no private API and touches nothing a person reads.
//!
//! The shape:
//!
//! ```text
//! client
//!   ├─ tools/list ... ours first, then each fronted server's, prefixed
//!   └─ tools/call
//!        ├─ ours .... handled locally; retrieve never leaves the process
//!        └─ theirs .. forwarded verbatim, and the result's text blocks
//!                     passed through the engine on the way back
//! ```
//!
//! Only the response direction is touched. Arguments are forwarded byte for
//! byte, because a server that validates them strictly must see what the model
//! actually sent.
//!
//! Server-held history, which is what makes intercepting such a client
//! pointless, works in favour of this: the elided result is what the server
//! stores, so every later turn pays the reduced price.

mod child;
mod merge;
mod route;

pub use child::{FRONTED_MARKER, Upstream, UpstreamError, is_fronted};

use std::sync::Arc;

use kompressor_core::{AppCore, RequestInfo};
use kompressor_stats::events::Surface;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};

use crate::handler::KompressorTools;

/// Our tools plus the servers we front.
pub struct ProxyTools {
    local: KompressorTools,
    core: Arc<AppCore>,
    upstreams: Vec<Upstream>,
    names: Vec<String>,
}

impl ProxyTools {
    /// Connect to every configured server.
    ///
    /// A server that cannot be reached is logged and skipped rather than
    /// refused: one broken entry in a list of five should cost its own tools,
    /// not the other four and ours.
    pub async fn connect(
        local: KompressorTools,
        core: Arc<AppCore>,
        configs: &[kompressor_core::config::McpServerConfig],
    ) -> Self {
        let mut upstreams = Vec::new();
        for config in configs {
            match Upstream::connect(config).await {
                Ok(upstream) => {
                    tracing::info!(server = %upstream.name, compress = upstream.compress, "fronting mcp server");
                    upstreams.push(upstream);
                }
                Err(error) => tracing::warn!(%error, "mcp server not fronted"),
            }
        }
        let names = upstreams.iter().map(|u| u.name.clone()).collect();
        ProxyTools {
            local,
            core,
            upstreams,
            names,
        }
    }

    /// How many servers are actually being fronted.
    pub fn fronted(&self) -> usize {
        self.upstreams.len()
    }

    /// Compress the text blocks of a result, leaving everything else alone.
    ///
    /// Images and binary resources pass through untouched: there is nothing the
    /// engine does to a PNG except make it larger. A block below the
    /// configured threshold comes back byte-identical, so this is safe to run
    /// over every text block rather than guessing which are worth it.
    async fn compress_result(
        &self,
        server: &str,
        mut result: CallToolResult,
        context: &RequestContext<RoleServer>,
    ) -> CallToolResult {
        let Ok(identity) = self.local.identity(context) else {
            // Without an identity there is no tenant to scope fragments to, and
            // an unscoped fragment is worse than an uncompressed result.
            return result;
        };
        let options = self.core.options_for(&identity, None);
        let request_id = KompressorTools::request_id(context);

        for block in result.content.iter_mut() {
            let ContentBlock::Text(text) = block else {
                continue;
            };
            let core = self.core.clone();
            let identity = identity.clone();
            let options = options.clone();
            let request_id = request_id.clone();
            let before = text.text.clone();
            let compressed = tokio::task::spawn_blocking(move || {
                core.compress_raw(
                    &identity,
                    &before,
                    None,
                    &RequestInfo {
                        request_id: &request_id,
                        api_family: Some("mcp-proxy"),
                        ..Default::default()
                    },
                    &options,
                    Surface::Mcp,
                )
            })
            .await;
            match compressed {
                Ok((content, outcome)) => {
                    if content.len() < text.text.len() {
                        tracing::debug!(
                            server = %server,
                            before = text.text.len(),
                            after = content.len(),
                            "compressed a tool result"
                        );
                        let _ = outcome;
                        text.text = content;
                    }
                }
                Err(error) => {
                    // A panicked blocking task must not lose the result the
                    // model asked for.
                    tracing::warn!(server = %server, %error, "compression failed; forwarding as-is");
                }
            }
        }
        result
    }
}

impl ServerHandler for ProxyTools {
    fn get_info(&self) -> ServerInfo {
        self.local.get_info()
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mine = self.local.list_tools(request, context).await?;
        let mut fronted = Vec::new();
        for upstream in &self.upstreams {
            fronted.push(merge::qualify_tools(&upstream.name, upstream.tools().await));
        }
        Ok(ListToolsResult {
            tools: merge::merge(mine.tools, fronted),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some((server, tool)) = route::split(&request.name, &self.names) else {
            // No known prefix: one of ours. Retrieve in particular must never
            // be forwarded -- the fragments live in this process.
            return self.local.call_tool(request, context).await;
        };
        let Some(upstream) = self.upstreams.iter().find(|u| u.name == server) else {
            return Err(ErrorData::invalid_params(
                format!("no mcp server named {server:?}"),
                None,
            ));
        };

        match upstream.call(tool, request.arguments).await {
            Ok(result) => {
                if upstream.compress {
                    Ok(self.compress_result(server, result, &context).await.into())
                } else {
                    Ok(result.into())
                }
            }
            // The server's failure is reported to the model as a tool error
            // rather than as a protocol error, so the conversation continues
            // and the model can say what went wrong.
            Err(reason) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "mcp server {server:?} could not run {tool:?}: {reason}"
            ))])
            .into()),
        }
    }
}
