//! The MCP surface: compression and, above all, retrieval, as tools.
//!
//! # Why this surface exists
//!
//! Recoverable mode replaces what it elides with a marker carrying a handle, and
//! that trade is only worth making if something can turn the handle back into
//! the bytes. On the proxy the app itself is that something. Here the model is:
//! an agent runtime connects, reads the server's instructions and the tool
//! descriptions, and calls [`optscale_kompressor_retrieve`] when it meets a marker.
//!
//! Which makes the tool descriptions part of the mechanism rather than
//! documentation around it. A model that has not been told the marker grammar
//! reads `[... 412 similar items omitted [ref:k_AbC123] ...]` as a statement
//! that data is missing, not as an offer to fetch it — and recoverable mode
//! quietly becomes aggressive mode that also pays for storage. The strings in
//! [`guidance`] and on each tool are therefore written to teach three things:
//! the shapes a handle appears in, that a range is cheaper than a whole
//! fragment, and that a miss is regenerated and never invented.
//!
//! # Tenancy
//!
//! Every retained fragment is scoped by tenant, and the tenant is resolved from
//! the connection's credential before any tool code runs. A tool's arguments
//! cannot name a tenant: there is no argument that carries one and no code path
//! that would read it. This matters more here than on the other surfaces,
//! because a tool call's arguments are written by a model that may itself have
//! been steered by the payload it is compressing.
//!
//! # Mounting it
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use kompressor_core::{AppCore, Config, build_ref_store};
//! # fn main() -> anyhow::Result<()> {
//! let config = Config::default();
//! let refs = build_ref_store(&config)?;
//! let core = Arc::new(AppCore::new(
//!     config,
//!     refs,
//!     kompressor_stats::writer::null_sink(),
//! ));
//!
//! // One router to merge with the other surfaces on a shared listener.
//! let router = kompressor_mcp::mcp_router(core.clone());
//! # let _ = router;
//! # Ok(())
//! # }
//! ```
//!
//! [`optscale_kompressor_retrieve`]: guidance::SERVER_INSTRUCTIONS

#![forbid(unsafe_code)]

pub mod guidance;
pub mod handler;
pub mod http;
pub mod identity;
pub mod limit;
pub mod proxy;
pub mod stdio;
pub mod wire;

pub use guidance::SERVER_INSTRUCTIONS;
pub use handler::KompressorTools;
pub use http::mcp_router;
pub use identity::IdentitySource;
pub use limit::{Decision, RetrievalLimiter};
pub use stdio::{serve_pipe, serve_proxy_pipe, serve_stdio};
