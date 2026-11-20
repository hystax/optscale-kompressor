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

/// What this process authenticates with, which a fronted server is not given.
///
/// A spawned server inherits the environment, and the runtime that launched
/// us put our credential there: the key or secret that says which tenant's
/// fragments this process may read. A fronted server is somebody else's code
/// — often a package `npx -y` fetched a moment ago — and has no use for it.
/// One that is meant to have one, a kompressor of its own, is given it by
/// name in its entry's `env`.
const WITHHELD: [&str; 3] = [
    crate::stdio::ENV_API_KEY,
    crate::stdio::ENV_SECRET,
    crate::stdio::ENV_TENANT,
];

/// The command that starts the server `config` names with `command`.
fn spawn_command(command: &str, config: &McpServerConfig) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(&config.args);
    // Before the configured `env`, so an entry can still pass one on purpose.
    for name in WITHHELD {
        cmd.env_remove(name);
    }
    for (key, value) in &config.env {
        cmd.env(key, value);
    }
    // The recursion guard. Set last so a configured `env` cannot unset it.
    cmd.env(FRONTED_MARKER, "1");
    cmd
}

/// Why a fronted server could not be reached.
#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error("mcp server {name:?} names neither command nor url")]
    Unconfigured { name: String },
    #[error("mcp server {name:?} names both command and url; pick one")]
    Overconfigured { name: String },
    /// Fronting a server over HTTP needs rmcp's reqwest client transport, which
    /// requires `reqwest >= 0.13.2`; this workspace pins 0.12 for the proxy's
    /// own client. Bumping that for a secondary transport is not a trade worth
    /// making mid-feature, so `url` is refused with this rather than silently
    /// ignored. A spawned server covers the case this mode was built for.
    #[error("mcp server {name:?}: url is not supported yet; use command")]
    UrlUnsupported { name: String },
    #[error("mcp server {name:?} has no name")]
    Unnamed { name: String },
    /// A name holding the separator makes routing depend on the order of the
    /// list: with servers `fs` and `fs__x`, `fs__x__read` is claimed by `fs`.
    #[error("mcp server {name:?}: a name may not contain {separator:?}", separator = super::route::SEPARATOR)]
    Ambiguous { name: String },
    /// Two entries with one name would list the same tools twice and send
    /// every call to the first.
    #[error("mcp server {name:?} is named twice; the later entry is skipped")]
    Duplicate { name: String },
    #[error("could not start mcp server {name:?}: {source}")]
    Start {
        name: String,
        #[source]
        source: std::io::Error,
    },
    #[error("mcp server {name:?} failed to initialize: {reason}")]
    Initialize { name: String, reason: String },
    #[error("mcp server {name:?} did not answer initialize within {timeout:?}")]
    Timeout {
        name: String,
        timeout: std::time::Duration,
    },
}

/// One fronted server, and whether its results are compressed.
pub struct Upstream {
    pub name: String,
    pub compress: bool,
    /// Not behind a lock. Requests are matched to answers by id, so calls to
    /// one server run side by side; a lock held across a call made each wait
    /// for the one before it, and made `tools/list` — which asks every server
    /// — wait on any call that never returned.
    service: Arc<RunningService<RoleClient, ()>>,
}

impl Upstream {
    /// Validate the configuration and connect.
    ///
    /// Validation is separate from connecting and happens first, so a bad entry
    /// fails with an error naming the server before anything is spawned. The
    /// caller decides what to do with it; [`super::ProxyTools::connect`] skips
    /// the entry.
    pub async fn connect(config: &McpServerConfig) -> Result<Self, UpstreamError> {
        let name = config.name.trim().to_string();
        if name.is_empty() {
            return Err(UpstreamError::Unnamed { name });
        }
        if name.contains(super::route::SEPARATOR) {
            return Err(UpstreamError::Ambiguous { name });
        }
        match (&config.command, &config.url) {
            (None, None) => return Err(UpstreamError::Unconfigured { name }),
            (Some(_), Some(_)) => return Err(UpstreamError::Overconfigured { name }),
            _ => {}
        }

        let service = if let Some(command) = &config.command {
            let cmd = spawn_command(command, config);
            let transport = rmcp::transport::TokioChildProcess::new(cmd).map_err(|source| {
                UpstreamError::Start {
                    name: name.clone(),
                    source,
                }
            })?;
            // Dropping the handshake on timeout drops the transport, which
            // kills the child: nothing is left running for a server we gave up on.
            tokio::time::timeout(config.startup_timeout, ().serve(transport))
                .await
                .map_err(|_| UpstreamError::Timeout {
                    name: name.clone(),
                    timeout: config.startup_timeout,
                })?
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
            service: Arc::new(service),
        })
    }

    /// This server's tools, or none if it has died.
    ///
    /// A dead server contributes an empty list rather than failing the whole
    /// `tools/list`: the client keeps every other server's tools and ours.
    pub async fn tools(&self) -> Vec<Tool> {
        match self.service.list_all_tools().await {
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
        // `CallToolRequestParams` is `#[non_exhaustive]`, so it is built by
        // assignment rather than as a literal; new protocol fields then default
        // rather than failing this build.
        let mut params = CallToolRequestParams::default();
        params.name = tool.to_string().into();
        params.arguments = arguments;
        self.service
            .call_tool(params)
            .await
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(name: &str, command: Option<&str>, url: Option<&str>) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            command: command.map(str::to_string),
            url: url.map(str::to_string),
            ..Default::default()
        }
    }

    /// The error, since `Upstream` has no `Debug` for `unwrap_err`.
    async fn refused(config: McpServerConfig) -> UpstreamError {
        match Upstream::connect(&config).await {
            Ok(upstream) => panic!("{:?} was fronted", upstream.name),
            Err(error) => error,
        }
    }

    /// What the child's environment is told, name by name: `Some` to set,
    /// `None` to remove from what it would inherit.
    fn told(config: &McpServerConfig) -> std::collections::BTreeMap<String, Option<String>> {
        spawn_command("server", config)
            .as_std()
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    /// Our credential is ours: a fronted server does not inherit it.
    #[test]
    fn a_spawned_server_is_not_handed_our_credentials() {
        let env = told(&server("fs", Some("server"), None));
        for name in [
            "KOMPRESSOR_API_KEY",
            "KOMPRESSOR_SECRET",
            "KOMPRESSOR_TENANT",
        ] {
            assert_eq!(env.get(name), Some(&None), "{name} is removed");
        }
        assert_eq!(env.get(FRONTED_MARKER), Some(&Some("1".to_string())));
        assert_eq!(
            env.len(),
            4,
            "everything else is inherited as it is: {env:?}"
        );
    }

    /// An entry that names one in its `env` means it: a kompressor fronted on
    /// purpose is given the key it is configured with, and still cannot
    /// clear the recursion marker.
    #[test]
    fn a_credential_an_entry_names_is_passed() {
        let mut config = server("inner", Some("kompressor"), None);
        config.args = vec!["mcp".to_string(), "serve".to_string()];
        config.env = [
            ("KOMPRESSOR_API_KEY", "its-own-key"),
            ("FS_ROOT", "/srv"),
            (FRONTED_MARKER, ""),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
        let env = told(&config);
        assert_eq!(
            env.get("KOMPRESSOR_API_KEY"),
            Some(&Some("its-own-key".to_string()))
        );
        assert_eq!(env.get("KOMPRESSOR_SECRET"), Some(&None));
        assert_eq!(env.get("FS_ROOT"), Some(&Some("/srv".to_string())));
        assert_eq!(env.get(FRONTED_MARKER), Some(&Some("1".to_string())));
        let command = spawn_command("kompressor", &config);
        let args: Vec<_> = command.as_std().get_args().collect();
        assert_eq!(args, ["mcp", "serve"]);
    }

    #[tokio::test]
    async fn a_blank_name_is_refused() {
        // Whitespace counts as blank: the name is trimmed before it is used.
        for name in ["", "   "] {
            let error = refused(server(name, Some("/bin/true"), None)).await;
            assert!(matches!(error, UpstreamError::Unnamed { .. }), "{error}");
        }
    }

    #[tokio::test]
    async fn a_name_holding_the_separator_is_refused() {
        let error = refused(server("fs__x", Some("/bin/true"), None)).await;
        assert!(matches!(error, UpstreamError::Ambiguous { .. }), "{error}");
    }

    #[tokio::test]
    async fn a_server_that_never_answers_initialize_is_given_up_on() {
        let mut config = server("slow", Some("sleep"), None);
        config.args = vec!["30".to_string()];
        config.startup_timeout = std::time::Duration::from_millis(200);
        let started = std::time::Instant::now();
        let error = refused(config).await;
        assert!(matches!(error, UpstreamError::Timeout { .. }), "{error}");
        assert_eq!(
            error.to_string(),
            r#"mcp server "slow" did not answer initialize within 200ms"#
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "waited {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn neither_command_nor_url_is_refused() {
        let error = refused(server("fs", None, None)).await;
        assert!(
            matches!(error, UpstreamError::Unconfigured { .. }),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            r#"mcp server "fs" names neither command nor url"#
        );
    }

    #[tokio::test]
    async fn both_command_and_url_is_refused_before_anything_is_spawned() {
        // A command that would fail to start if it were run: refusing on the
        // configuration first means this never gets that far.
        let config = server(
            "fs",
            Some("/nonexistent/mcp-server"),
            Some("http://localhost/mcp"),
        );
        let error = refused(config).await;
        assert!(
            matches!(error, UpstreamError::Overconfigured { .. }),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_url_is_refused_by_name() {
        let error = refused(server("remote", None, Some("https://example.invalid/mcp"))).await;
        assert!(
            matches!(error, UpstreamError::UrlUnsupported { .. }),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .contains("url is not supported yet; use command"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_command_that_cannot_start_names_the_server() {
        let error = refused(server(" fs ", Some("/nonexistent/mcp-server"), None)).await;
        assert!(matches!(error, UpstreamError::Start { .. }), "{error}");
        assert!(
            error
                .to_string()
                .starts_with(r#"could not start mcp server "fs": "#),
            "the trimmed name is the one reported: {error}"
        );
    }

    #[tokio::test]
    async fn a_command_that_is_not_an_mcp_server_fails_to_initialize() {
        // Starts, then exits without answering `initialize`.
        let error = refused(server("fs", Some("/bin/true"), None)).await;
        assert!(matches!(error, UpstreamError::Initialize { .. }), "{error}");
    }
}
