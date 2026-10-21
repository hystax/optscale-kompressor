//! The four tools.
//!
//! Each one is a thin translation of an [`AppCore`] operation into MCP's
//! request and result shapes. The translation is deliberately thin: policy —
//! which mode runs, what may be compressed, how a retrieval is priced — belongs
//! to the core so that this surface, the HTTP API and the proxy cannot drift
//! apart.

use std::sync::Arc;

use kompressor_core::auth::Identity;
use kompressor_core::{AppCore, RequestInfo};
use kompressor_engine::messages::PrefixCache;
use kompressor_stats::events::Surface;
use kompressor_stats::reader::StatsReader;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, tool, tool_handler, tool_router};

use crate::guidance::SERVER_INSTRUCTIONS;

/// Run synchronous engine or store work off the runtime.
///
/// The HTTP surface has done this since it had a semaphore; this one called
/// straight into the engine from an `async fn`, so compression and a SQLite
/// write ran on a runtime worker and stalled everything else sharing it. It
/// does not share the API's `server.max_inflight_compressions` permit — that
/// bound still does not cover this surface, which is worth closing separately.
async fn blocking<T, F>(work: F) -> Result<T, ErrorData>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ErrorData::internal_error(format!("engine task: {error}"), None))
}
use crate::identity::IdentitySource;
use crate::limit::{Decision, RetrievalLimiter};
use crate::wire::{
    CompressInput, CompressRawInput, RetrieveInput, RetrieveOutput, StatsInput, WireMessage,
    normalize_handle, patch_from,
};

/// A day, in milliseconds: the default statistics window.
const DEFAULT_STATS_WINDOW_MS: i64 = 86_400_000;

/// The tool server.
///
/// Cloned per request by the streamable-HTTP transport, so everything it holds
/// is shared behind an `Arc`. In particular the retrieval limiter must be
/// shared: a limiter created per request would count to one and never trip,
/// which is the failure the whole guard exists to prevent.
#[derive(Clone)]
pub struct KompressorTools {
    core: Arc<AppCore>,
    limiter: Arc<RetrievalLimiter>,
    identity: IdentitySource,
    tool_router: ToolRouter<Self>,
}

impl KompressorTools {
    /// A server whose tenant comes from each request's own credential.
    pub fn from_connection(core: Arc<AppCore>, limiter: Arc<RetrievalLimiter>) -> KompressorTools {
        KompressorTools {
            core,
            limiter,
            identity: IdentitySource::Connection,
            tool_router: Self::tool_router(),
        }
    }

    /// A server acting for one fixed tenant, for a transport that carries no
    /// credential of its own.
    pub fn for_identity(
        core: Arc<AppCore>,
        limiter: Arc<RetrievalLimiter>,
        identity: Identity,
    ) -> KompressorTools {
        KompressorTools {
            core,
            limiter,
            identity: IdentitySource::Fixed(Box::new(identity)),
            tool_router: Self::tool_router(),
        }
    }

    /// The tenant this call acts for.
    ///
    /// Read from the connection, never from the call's arguments. A `tenant`
    /// field in a tool's arguments reaches no code at all.
    pub(crate) fn identity(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Result<Identity, ErrorData> {
        match &self.identity {
            IdentitySource::Fixed(identity) => Ok((**identity).clone()),
            IdentitySource::Connection => context
                .extensions
                .get::<http::request::Parts>()
                .and_then(|parts| parts.extensions.get::<Identity>())
                .cloned()
                .ok_or_else(|| {
                    // Reachable only if the transport were mounted without the
                    // authenticating layer. Refusing is the only safe answer:
                    // inventing a tenant here would hand one caller's retained
                    // fragments to another.
                    ErrorData::internal_error(
                        "this server was mounted without tenant resolution; no request can be \
                         attributed",
                        None,
                    )
                }),
        }
    }

    /// The request identifier recorded against everything this call does.
    ///
    /// The JSON-RPC id is reused so a row in the statistics database can be
    /// matched back to the exact tool call that produced it.
    pub(crate) fn request_id(context: &RequestContext<RoleServer>) -> String {
        format!("mcp-{}", context.id)
    }
}

#[tool_router]
impl KompressorTools {
    /// OptScale Kompressor: fetch back content that optscale_kompressor elided,
    /// in whole or in part.
    ///
    /// Call this when a payload you were given contains a marker carrying a
    /// handle. A handle is the text `ref:` followed by `k_` and exactly 12
    /// url-safe characters (letters, digits, `-`, `_`). The marker shapes it
    /// appears in, with the handle in position:
    ///
    ///   [... 412 similar items omitted [ref:k_AbC123] (fields: id,status) ...]
    ///   …[+1840 chars ref:k_DeF456]
    ///   <line> [×37 ref:k_GhI789]
    ///   [... 87 log lines omitted [ref:k_…]; 3 ERROR, 9 WARN]
    ///   # [... 31 body lines omitted from parse_row; retrieve: ref:k_…]
    ///
    /// A marker with no `ref:` handle has nothing behind it and must not be
    /// passed here.
    ///
    /// Prefer a `range` whenever you need only part of the fragment. Everything
    /// returned is re-injected into your next request and billed again, so an
    /// unnarrowed retrieval loop can cost more than the compression saved. Use
    /// `{"lines":{"start":1,"end":40}}` for a line window,
    /// `{"json_pointer":{"pointer":"/3/items"}}` to reach one record of a JSON
    /// fragment, `{"chars":{"start":0,"end":4096}}` for a character window, or
    /// `{"grep":{"pattern":"ERROR","context":2}}` to keep matching lines with
    /// their context. An answer that came back `truncated` hit the size cap and
    /// should be asked for again with a narrower range.
    ///
    /// If the handle does not resolve, the data is gone: it expired, or it
    /// belongs to another conversation. Regenerate it by re-running the command
    /// or re-reading the file. Never reconstruct elided content from memory, and
    /// never describe what it contained unless a retrieval returned it.
    #[tool(
        name = "optscale_kompressor_retrieve",
        output_schema = rmcp::handler::server::tool::schema_for_output::<RetrieveOutput>(),
    )]
    async fn retrieve(
        &self,
        Parameters(input): Parameters<RetrieveInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let identity = self.identity(&context)?;
        if let Decision::Limited {
            per_minute,
            retry_after_seconds,
        } = self.limiter.check(identity.tenant.as_str())
        {
            // A tool result rather than a transport error: the model is the one
            // that has to change behaviour, and it only reads tool results.
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "Retrieval rate limit reached: this tenant may retrieve {per_minute} times per \
                 minute, and the window resets in about {retry_after_seconds}s. You are most \
                 likely fetching whole fragments in a loop. Work with what you already have, and \
                 when you do retrieve again, pass a narrow `range` so each call returns only the \
                 lines or the record you need."
            ))]));
        }

        let request = RequestInfo {
            request_id: &Self::request_id(&context),
            api_family: Some("mcp"),
            ..Default::default()
        };
        let core = self.core.clone();
        let for_store = identity.clone();
        let request_id = request.request_id.to_string();
        let reference = normalize_handle(&input.reference).to_string();
        // Kept for the note after the move into the blocking closure.
        let handle = reference.clone();
        let range = input.range.map(Into::into);
        let retrieved = blocking(move || {
            core.retrieve(
                &for_store,
                &reference,
                range.as_ref(),
                &RequestInfo {
                    request_id: &request_id,
                    api_family: Some("mcp"),
                    ..Default::default()
                },
                Surface::Mcp,
            )
        })
        .await?;

        // A miss explains itself; so does a range that selected nothing, and
        // the second is the one that otherwise reads as a successful empty
        // answer.
        let hint = match retrieved.found {
            false => Some(retrieved.miss_hint().to_string()),
            true => retrieved.range_hint.clone(),
        };
        let output = RetrieveOutput {
            content: retrieved.content.clone(),
            found: retrieved.found,
            expired: retrieved.expired,
            ranged: retrieved.ranged,
            total_lines: retrieved.total_lines,
            total_chars: retrieved.total_chars,
            truncated: retrieved.truncated,
            hint: hint.clone(),
        };
        // The text block carries the fragment itself rather than the JSON
        // envelope: a client that shows only the text must not show the model a
        // backslash-escaped copy of the very content it asked for.
        let text = match (&output.content, &hint) {
            (Some(content), _) => content.clone(),
            (None, Some(hint)) => format!("Nothing retrieved: {hint}"),
            (None, None) => String::new(),
        };
        let mut result =
            CallToolResult::structured(serde_json::to_value(&output).map_err(internal)?);
        result.content = vec![ContentBlock::text(text)];
        // ...and a second block for what the envelope says, when it says
        // something the model should act on.
        //
        // Not every client surfaces `structuredContent`. Claude Desktop's chat
        // shows the text blocks alone, and a model there reported `found` and
        // `total_lines` as simply absent — so the one field that carries an
        // instruction, `truncated`, was invisible precisely where the tool
        // description tells the model to narrow the range and ask again.
        //
        // Kept out of the first block so a fragment stays quotable verbatim,
        // and omitted entirely when the whole fragment came back, which is the
        // case that needs no commentary.
        if let Some(note) = envelope_note(&output, &handle) {
            result.content.push(ContentBlock::text(note));
        }
        Ok(result)
    }

    /// OptScale Kompressor: compress a conversation, returning the rewritten
    /// messages and what optscale_kompressor cost and saved.
    ///
    /// `messages` is the conversation oldest first; each is
    /// `{"role": ..., "content": ...}` where content is a string, a list of
    /// `{"text": ..., "protected": bool}` blocks, or `null` for something this
    /// engine cannot read.
    ///
    /// `mode` defaults to `recoverable`: every elision is retained behind a
    /// `ref:k_...` handle that `optscale_kompressor_retrieve` can hand back. That default
    /// is only useful if something in your loop will actually retrieve. If
    /// nothing will, pass `aggressive` — it produces the same elisions with no
    /// handles and no retained fragments, rather than paying to keep data
    /// nobody will ask for. `lossless` applies only reversible transforms.
    ///
    /// `options` carries engine knobs for this request alone, such as
    /// `{"min_compress_chars": 2048}`. An unknown key is an error rather than a
    /// silent no-op.
    #[tool(name = "optscale_kompressor_compress")]
    async fn compress(
        &self,
        Parameters(input): Parameters<CompressInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let identity = self.identity(&context)?;
        let patch = patch_from(input.options, input.mode)?;
        let options = self.core.options_for(&identity, patch.as_ref());

        let mut messages: Vec<_> = input.messages.into_iter().map(Into::into).collect();
        // Off the runtime, as the HTTP surface already does. Compression is CPU
        // work and retention can wait on a SQLite mutex or a busy timeout; run
        // on a worker it stalls every other task sharing it, including the
        // timers meant to bound it. Measured on a single-worker runtime with a
        // 200 ms store write: a 10 ms timer started alongside this fired at
        // 250 ms.
        let request_id = Self::request_id(&context);
        let model = input.model.clone();
        let provider = input.provider.clone();
        let core = self.core.clone();
        let for_engine = identity.clone();
        let engine_options = options.clone();
        let (mut messages, outcome) = blocking(move || {
            let request = RequestInfo {
                request_id: &request_id,
                api_family: Some("mcp"),
                model: model.as_deref(),
                provider: provider.as_deref(),
            };
            let outcome = core.compress(
                &for_engine,
                &mut messages,
                &request,
                &engine_options,
                // The caller hands over the messages it wants compressed and
                // nothing else; there is no provider-cached prefix to protect
                // here, because this surface never talks to a provider.
                0,
                "",
                Surface::Mcp,
                PrefixCache::Warm,
                // Same as the API surface: no retrieve loop runs here.
                &[],
                &[],
                // No conversation here, so nothing measured: demand the most.
                1000,
            );
            (messages, outcome)
        })
        .await?;

        let messages: Vec<WireMessage> = messages.drain(..).map(Into::into).collect();
        Ok(CallToolResult::structured(serde_json::json!({
            "messages": messages,
            "stats": outcome.stats,
        })))
    }

    /// OptScale Kompressor: compress one standalone payload with
    /// optscale_kompressor, such as a command's output or a file.
    ///
    /// Routed as a tool result, so the size gate decides whether anything
    /// happens at all: a payload below the threshold comes back byte-identical.
    /// Pass `content_type` only to override detection.
    ///
    /// `mode` defaults to `recoverable`, which retains every elision behind a
    /// `ref:k_...` handle `optscale_kompressor_retrieve` can resolve. Pass `aggressive`
    /// if nothing will ever retrieve.
    #[tool(name = "optscale_kompressor_compress_raw")]
    async fn compress_raw(
        &self,
        Parameters(input): Parameters<CompressRawInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let identity = self.identity(&context)?;
        let patch = patch_from(input.options, input.mode)?;
        let options = self.core.options_for(&identity, patch.as_ref());

        let request = RequestInfo {
            request_id: &Self::request_id(&context),
            api_family: Some("mcp"),
            ..Default::default()
        };
        let core = self.core.clone();
        let for_engine = identity.clone();
        let request_id = request.request_id.to_string();
        let content_in = input.content.clone();
        let content_type = input.content_type.map(Into::into);
        let engine_options = options.clone();
        let (content, outcome) = blocking(move || {
            core.compress_raw(
                &for_engine,
                &content_in,
                content_type,
                &RequestInfo {
                    request_id: &request_id,
                    api_family: Some("mcp"),
                    ..Default::default()
                },
                &engine_options,
                Surface::Mcp,
            )
        })
        .await?;

        Ok(CallToolResult::structured(serde_json::json!({
            "content": content,
            "stats": outcome.stats,
        })))
    }

    /// OptScale Kompressor: report what optscale_kompressor has cost and saved
    /// for the calling tenant.
    ///
    /// `from` and `to` are Unix milliseconds and default to the last 24 hours.
    /// The window covers only the tenant this connection authenticated as; there
    /// is no argument that widens it.
    ///
    /// Read `saved_net`, not `saved_gross`. Gross counts what compression
    /// removed; net subtracts what retrievals put back, and in recoverable mode
    /// it can be negative — which is the signal that fragments are being fetched
    /// whole in a loop instead of with a range.
    #[tool(name = "optscale_kompressor_stats")]
    async fn stats(
        &self,
        Parameters(input): Parameters<StatsInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let identity = self.identity(&context)?;
        let config = self.core.config();
        if !config.stats.enabled {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "Statistics are disabled on this deployment, so there is nothing to report.",
            )]));
        }

        let to = input.to.unwrap_or_else(now_ms);
        let from = input.from.unwrap_or(to - DEFAULT_STATS_WINDOW_MS);
        // Opened per call rather than held: the writer owns the connection that
        // matters, and a reader kept open across a retention sweep would serve
        // a stale page cache.
        //
        // Off the runtime like the other three handlers. This one was missed
        // when they were moved: a SQLite read over a window of raw rows is not
        // free, and it can wait on the same busy timeout a write does.
        let path = config.stats.path.clone();
        let tenant = identity.tenant.as_str().to_string();
        let summary = blocking(move || {
            StatsReader::open(&path)
                .map_err(|error| tool_failure("open the statistics database", error))?
                .summary(&tenant, from, to)
                .map_err(|error| tool_failure("read the statistics database", error))
        })
        .await??;

        Ok(CallToolResult::structured(
            serde_json::to_value(&summary).map_err(internal)?,
        ))
    }
}

// Routing through the stored router rather than the macro's default, which
// rebuilds one — schema generation included — on every `tools/list` and every
// `tools/call`.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for KompressorTools {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(SERVER_INSTRUCTIONS)
            // The default here is the transport library's own name and version,
            // which is what a user sees in an agent's server list. Naming the
            // library rather than this server is worse than unhelpful: the
            // version is the one number a bug report needs, and reporting
            // rmcp's would send it to the wrong project.
            .with_server_info(Implementation::new(
                "optscale_kompressor",
                env!("CARGO_PKG_VERSION"),
            ))
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0)
}

/// What the envelope says, for clients that never show the envelope.
///
/// `None` when the whole fragment came back intact: the content speaks for
/// itself and a note would be noise on every retrieval. Something is returned
/// only when the model is holding part of a fragment, because then "how much is
/// there" and "was this cut" change what it should do next.
fn envelope_note(output: &RetrieveOutput, reference: &str) -> Option<String> {
    if !output.found {
        // The miss already explains itself in the first block.
        return None;
    }
    if output.truncated {
        return Some(format!(
            "[kompressor: TRUNCATED at the size cap. ref:{reference} holds {} lines / {} chars.              Ask again with a narrower range rather than assuming this is all of it.]",
            output.total_lines, output.total_chars
        ));
    }
    if output.ranged {
        return Some(format!(
            "[kompressor: a range of ref:{reference}, which holds {} lines / {} chars in full.]",
            output.total_lines, output.total_chars
        ));
    }
    None
}

fn internal(error: serde_json::Error) -> ErrorData {
    ErrorData::internal_error(format!("could not encode the result: {error}"), None)
}

fn tool_failure(what: &str, error: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(format!("could not {what}: {error}"), None)
}
