//! The stdio transport, for agent runtimes that launch the server themselves.
//!
//! A local runtime spawns this process and speaks JSON-RPC over the pipe, so
//! there is no connection to read a credential from. The tenant is therefore
//! fixed for the life of the process and comes from the environment the
//! launching runtime controls.
//!
//! This opens the same SQLite files a running `serve` uses. That is deliberate,
//! not an accident to be defended against: WAL makes concurrent readers and one
//! writer safe across processes, so a local proxy and a stdio tool server share
//! one set of retained fragments and a handle minted by either resolves through
//! the other.

use std::sync::Arc;

use kompressor_core::AppCore;
use kompressor_core::auth::{Credentials, Identity};
use rmcp::transport::{IntoTransport, stdio};
use rmcp::{RoleServer, ServiceExt};

use crate::handler::KompressorTools;
use crate::limit::RetrievalLimiter;

/// The API key a launching runtime may pass in the environment.
const ENV_API_KEY: &str = "KOMPRESSOR_API_KEY";
/// The shared secret, for a trusted local component.
const ENV_SECRET: &str = "KOMPRESSOR_SECRET";
/// The tenant to act for. Honoured only alongside a valid secret.
const ENV_TENANT: &str = "KOMPRESSOR_TENANT";

/// Serve the tools over stdin and stdout until the peer disconnects.
///
/// Returns once the client closes the pipe, which is how an agent runtime signs
/// off; a transport failure is returned as an error so the launching process can
/// report it rather than exiting silently.
pub async fn serve_stdio(core: Arc<AppCore>) -> anyhow::Result<()> {
    let identity = stdio_identity(&core)?;
    // Proxy mode only when something is configured to be fronted: a runtime
    // that launched this expecting our four tools gets exactly those, and a
    // session with no fronted servers does not pay for the machinery.
    if core.config().mcp.servers.is_empty() || crate::proxy::is_fronted() {
        serve_pipe(core, identity, stdio()).await
    } else {
        serve_proxy_pipe(core, identity, stdio()).await
    }
}

/// Serve proxy mode over a pipe: our tools plus every fronted server's.
///
/// Split from [`serve_pipe`] rather than folded into it so the no-server case
/// stays exactly what it was, and so a test can drive this one with a client on
/// the other end of a duplex.
pub async fn serve_proxy_pipe<T, E, A>(
    core: Arc<AppCore>,
    identity: Identity,
    transport: T,
) -> anyhow::Result<()>
where
    T: IntoTransport<RoleServer, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let limiter = Arc::new(RetrievalLimiter::new(
        core.config().mcp.max_retrievals_per_minute,
    ));
    let servers = core.config().mcp.servers.clone();
    let tools = KompressorTools::for_identity(core.clone(), limiter, identity);
    let proxy = crate::proxy::ProxyTools::connect(tools, core, &servers).await;
    let service = rmcp::ServiceExt::serve(proxy, transport).await?;
    service.waiting().await?;
    Ok(())
}

/// Serve the tools over any byte pipe, acting for one fixed identity.
///
/// [`serve_stdio`] is this with the process's own stdin and stdout. It is split
/// out so the transport can be driven over an in-memory pipe, which is the only
/// way to test this path without spawning a process.
pub async fn serve_pipe<T, E, A>(
    core: Arc<AppCore>,
    identity: Identity,
    transport: T,
) -> anyhow::Result<()>
where
    T: IntoTransport<RoleServer, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let limiter = Arc::new(RetrievalLimiter::new(
        core.config().mcp.max_retrievals_per_minute,
    ));
    let tools = KompressorTools::for_identity(core, limiter, identity);

    let service = tools.serve(transport).await?;
    service.waiting().await?;
    Ok(())
}

/// Resolve the one identity this process acts for.
///
/// Resolution goes through the same [`Authenticator`] the HTTP surface uses, so
/// a deployment that configured API keys does not get an anonymous back door
/// just because the transport is a pipe.
///
/// [`Authenticator`]: kompressor_core::auth::Authenticator
fn stdio_identity(core: &AppCore) -> anyhow::Result<Identity> {
    let api_key = std::env::var(ENV_API_KEY).ok();
    let secret = std::env::var(ENV_SECRET).ok();
    let tenant = std::env::var(ENV_TENANT).ok();
    let credentials = Credentials {
        bearer: None,
        api_key: api_key.as_deref(),
        // One environment variable, and it is ours: the stdio server has no
        // provider credential to keep separate from it.
        kompressor_key: None,
        secret: secret.as_deref(),
        requested_tenant: tenant.as_deref(),
    };
    core.auth().resolve(&credentials).map_err(|error| {
        anyhow::anyhow!(
            "cannot start the stdio tool server: {error}. Set {ENV_API_KEY}, or {ENV_SECRET} with \
             {ENV_TENANT}, in the environment of the process that launches it."
        )
    })
}
