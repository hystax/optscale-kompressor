//! A CONNECT listener that compresses clients which pin their own base URL.
//!
//! Everything else in this crate assumes the client was pointed at the proxy.
//! Some cannot be: Claude Desktop sets `ANTHROPIC_BASE_URL` itself when it
//! spawns a session, overwriting the inherited environment and `settings.json`
//! alike, so there is no value to set. It does honour a proxy setting and a
//! certificate authority, and that is what this serves.
//!
//! The shape is deliberately small. One listener, one decision, two outcomes:
//!
//! ```text
//! CONNECT host:443
//!   ├─ host not on the allow list ... 200, then bytes are copied, never read
//!   └─ host on the allow list ....... 200, TLS terminated with a minted leaf,
//!                                     the decrypted request served by the
//!                                     router this binary already builds
//! ```
//!
//! The second branch ends in [`axum::Router`] — the same one `axum::serve`
//! feeds on the ordinary listener. Nothing downstream knows the difference,
//! which is why no mount, route or handler changes to support this.

mod allow;
mod ca;
mod connect;
mod tunnel;

pub use allow::AllowList;
pub use ca::{Authority, CaError};
pub use connect::{Refusal, Target};

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ServerBuilder;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

/// Why the listener could not start. Failures once it is running are logged
/// per connection and never stop the loop.
#[derive(Debug, thiserror::Error)]
pub enum InterceptError {
    #[error("intercept listen address {address:?} is not an address: {source}")]
    Address {
        address: String,
        #[source]
        source: std::net::AddrParseError,
    },
    #[error("could not bind the intercept listener on {address}: {source}")]
    Bind {
        address: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Ca(#[from] CaError),
}

/// Everything the accept loop needs, resolved once at startup.
pub struct Intercept {
    listener: TcpListener,
    address: SocketAddr,
    allow: AllowList,
    authority: Arc<Authority>,
}

impl Intercept {
    /// Bind the listener and load or create the authority.
    ///
    /// Both happen before serving so a misconfiguration is a startup failure
    /// rather than a connection that fails mysteriously later.
    pub async fn bind(
        address: &str,
        hosts: &[String],
        ca_dir: &std::path::Path,
        alpn: &[String],
        leaf_cache_entries: usize,
    ) -> Result<Self, InterceptError> {
        let address: SocketAddr = address.parse().map_err(|source| InterceptError::Address {
            address: address.to_string(),
            source,
        })?;
        let authority = Authority::load_or_create(ca_dir, alpn, leaf_cache_entries)?;
        let listener = TcpListener::bind(address)
            .await
            .map_err(|source| InterceptError::Bind { address, source })?;
        // What the socket got, not what was asked for. Port 0 means "pick
        // one", and reporting the request back would log `:0` and hand callers
        // an address nothing is listening on.
        let address = listener.local_addr().unwrap_or(address);
        Ok(Intercept {
            listener,
            address,
            allow: AllowList::new(hosts),
            authority: Arc::new(authority),
        })
    }

    /// Where it is listening, for logging.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// The certificate an operator has to trust.
    pub fn certificate_path(&self) -> std::path::PathBuf {
        self.authority.certificate_path()
    }

    /// The authority's fingerprint, for recognising it in a trust store.
    pub fn fingerprint(&self) -> String {
        self.authority.fingerprint()
    }

    /// Accept until `shutdown` resolves.
    ///
    /// One task per connection, and a failing connection never takes the loop
    /// with it: a proxy that stops accepting because one client misbehaved
    /// takes every other client down too.
    pub async fn serve<F>(self, router: Router, shutdown: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let Intercept {
            listener,
            allow,
            authority,
            ..
        } = self;

        if allow.is_empty() {
            // Worth saying out loud: this is a working listener that will
            // tunnel everything, which looks identical to a broken one.
            tracing::warn!(
                "intercept is listening with no intercept_hosts; every connection will be tunnelled"
            );
        }

        tokio::pin!(shutdown);
        loop {
            let accepted = tokio::select! {
                _ = &mut shutdown => break,
                accepted = listener.accept() => accepted,
            };
            let (stream, peer) = match accepted {
                Ok(pair) => pair,
                Err(error) => {
                    tracing::warn!(%error, "intercept accept failed");
                    continue;
                }
            };
            let allow = allow.clone();
            let authority = Arc::clone(&authority);
            let router = router.clone();
            tokio::spawn(async move {
                if let Err(error) = handle(stream, allow, authority, router).await {
                    tracing::debug!(%peer, %error, "intercept connection ended");
                }
            });
        }
    }
}

/// One connection, from its request line to whichever of the two ends it
/// reaches.
async fn handle(
    stream: TcpStream,
    allow: AllowList,
    authority: Arc<Authority>,
    router: Router,
) -> std::io::Result<()> {
    let (target, mut reader) = match connect::read_connect(stream).await {
        Ok(pair) => pair,
        Err((refusal, mut reader)) => {
            connect::refuse(reader.get_mut(), &refusal).await;
            return Ok(());
        }
    };

    connect::accept(reader.get_mut()).await?;

    if !allow.intercepts(&target.host) {
        let authority = target.to_string();
        let copied = tunnel::relay(&mut reader, &authority).await?;
        tracing::debug!(host = %target.host, sent = copied.0, received = copied.1, "tunnelled");
        return Ok(());
    }

    let config = match authority.server_config(&target.host) {
        Ok(config) => config,
        Err(error) => {
            // Minting failed, so the handshake cannot happen. Tunnelling
            // instead would be worse than failing: the operator asked for this
            // host to be compressed and would see it silently not be.
            tracing::warn!(host = %target.host, %error, "could not mint a leaf; refusing");
            return Ok(());
        }
    };

    let alpn_offered: Vec<String> = config
        .alpn_protocols
        .iter()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    let tls = match TlsAcceptor::from(config).accept(reader).await {
        Ok(tls) => tls,
        Err(error) => {
            // Two causes, and they are indistinguishable from this side: the
            // client rejected the certificate, or it offered an ALPN protocol
            // this listener does not advertise. Both end as a handshake EOF.
            // An earlier version of this named only the certificate, and sent
            // the first person to hit the ALPN case looking at trust stores
            // for an hour.
            tracing::warn!(
                host = %target.host,
                %error,
                alpn = ?alpn_offered,
                "TLS handshake failed: either this client does not trust the kompressor CA, \
                 or it offered a protocol absent from proxy.intercept.alpn"
            );
            return Ok(());
        }
    };

    tracing::debug!(host = %target.host, "intercepted");
    let service = hyper_util::service::TowerToHyperService::new(router);
    if let Err(error) = ServerBuilder::new(TokioExecutor::new())
        .serve_connection_with_upgrades(TokioIo::new(tls), service)
        .await
    {
        tracing::debug!(host = %target.host, %error, "intercepted connection ended");
    }
    Ok(())
}
