//! Which agent CLIs can be wrapped, and how each one is pointed at the proxy.
//!
//! Every coding agent already knows how to talk to OpenAI or Anthropic. None of
//! them knows what a compression proxy is, and none of them needs to: each one
//! reads a base-URL environment variable, so wrapping is a matter of setting
//! that variable to this process's listener and launching the agent normally.
//!
//! The catalogue is deliberately short. An entry here is a claim that a
//! particular variable name works for a particular tool, and a wrong claim
//! fails in the worst possible way — the agent talks to the provider directly,
//! nothing is compressed, and no error is printed anywhere. So a tool earns an
//! entry only when its variable is unambiguous, and `Agent::generic` exists so
//! that anything not listed can still be wrapped by naming the variable at the
//! command line rather than by guessing on the user's behalf.
//!
//! # Config-file providers
//!
//! An entry claims the variable works, not that it works for every way the tool
//! can be configured. A provider whose base URL lives in the tool's own config
//! file beats any variable, so a wrap that only sets one routes nothing for it.
//! That is the silent failure above, reached by a different road, and it is why
//! `opencode` sets no variable at all: its providers are pointed at the proxy
//! through opencode's configuration instead — see [`opencode`].
//!
//! [`opencode`]: super::opencode

use kompressor_core::config::{SubscriptionUpstream, Upstream};

/// The shape of the URL an agent expects.
///
/// The difference is whose `/v1` it is. An Anthropic-style client is configured
/// with a host and appends `/v1/messages` itself; an OpenAI-style client is
/// configured with a URL that already ends in `/v1` and appends
/// `/chat/completions`. Handing either one the other's shape produces `/v1/v1/…`
/// or a bare `/messages`, and both 404 at the upstream rather than here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Endpoint {
    /// `http://127.0.0.1:PORT`
    Root,
    /// `http://127.0.0.1:PORT/v1`
    V1,
}

impl Endpoint {
    pub fn url(self, port: u16) -> String {
        match self {
            Endpoint::Root => format!("http://127.0.0.1:{port}"),
            Endpoint::V1 => format!("http://127.0.0.1:{port}/v1"),
        }
    }
}

/// A provider the proxy has to be able to reach for a wrapped agent to work.
///
/// `upstreams` is empty in the default configuration, and an unmatched path is a
/// 404 — so a wrap that did not add these would start a proxy that refuses every
/// request the agent makes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Provider {
    Anthropic,
    OpenAi,
    /// OpenCode Zen and OpenCode Go: one host, `/zen/v1` and `/zen/go/v1`.
    OpenCodeZen,
}

impl Provider {
    fn upstream(self) -> Upstream {
        match self {
            // Ordered before OpenAI by `upstreams_for`: `/v1` as a prefix also
            // matches `/v1/messages`, and first match wins.
            Provider::Anthropic => Upstream {
                name: "anthropic".into(),
                path_prefix: "/v1/messages".into(),
                base_url: "https://api.anthropic.com".into(),
                api_family: "anthropic".into(),
                forward_auth: true,
                ..Default::default()
            },
            Provider::OpenAi => Upstream {
                name: "openai".into(),
                path_prefix: "/v1".into(),
                base_url: "https://api.openai.com".into(),
                api_family: "openai".into(),
                forward_auth: true,
                // A ChatGPT plan puts the same Responses body on a different
                // host under a different prefix, and only the credential says
                // which. Forwarding a subscriber to api.openai.com 404s, and
                // Codex reads a 404 there as an invalid session and stops
                // using the subscription at all — so the mount has to be able
                // to follow the caller.
                subscription: Some(SubscriptionUpstream {
                    base_url: "https://chatgpt.com".into(),
                    rewrite_prefix: "/backend-api/codex".into(),
                }),
                ..Default::default()
            },
            // One mount for both tiers and every wire shape behind them. Zen
            // serves Chat Completions, Responses and Messages under the same
            // prefix, and the family is read off the path's tail, so nothing
            // here has to know which model a session picked. The client's
            // `x-opencode-session` header rides through with the rest: the relay
            // answers 400 without it.
            Provider::OpenCodeZen => Upstream {
                name: "opencode-zen".into(),
                path_prefix: "/zen".into(),
                base_url: "https://opencode.ai".into(),
                api_family: "auto".into(),
                forward_auth: true,
                ..Default::default()
            },
        }
    }
}

/// A config file an agent must have edited because no variable reaches it.
///
/// An entry here is a much stronger claim than an environment variable: the
/// wrap edits a file the user owns, outside this repository. It is reserved
/// for tools that leave no alternative — see [`configfile`] for how the edit
/// is made reversible.
///
/// [`configfile`]: super::configfile
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConfigFile {
    /// `$CODEX_HOME/config.toml`, or `~/.codex/config.toml`.
    Codex,
    /// opencode's configuration: `OPENCODE_CONFIG_CONTENT` for a session this
    /// command launches, the global `opencode.json` for `--attach`.
    OpenCode,
}

/// How a tool is pointed at the proxy.
///
/// Three of the four ways an agent can be wired up are automatic: set a
/// variable, edit a config file, or both. The fourth is not automatable at all,
/// and pretending otherwise is the failure this enum exists to prevent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wiring {
    /// Set the base-URL variables and run the binary. The default.
    Launch,
    /// Nothing to launch and nothing to edit: the tool is a GUI application
    /// configured by hand, so the wrap prints what to paste and holds the proxy
    /// up until Ctrl-C. `where_` names the settings page, because an
    /// instruction that does not say where to type it is not an instruction.
    ///
    /// No entry uses it since the Cursor editor moved to hook mode — its
    /// base-URL override is called from Cursor's servers and never reaches a
    /// local proxy. Cline and Continue, which do call a local URL, use it.
    /// For a manual entry the names in [`Agent::env`] are labels printed
    /// beside each URL, never variables that are set.
    Manual { where_: &'static str },
    /// This proxy cannot compress the tool, and an entry that says so is worth
    /// more than no entry at all.
    ///
    /// Without one, `wrap` answers "unknown agent" and points at
    /// `--env NAME=root|v1`, which for a tool that speaks another protocol sets
    /// a variable that either does nothing or breaks it — the silent failure
    /// this catalogue exists to avoid, reached by the helpful road.
    Unsupported { why: &'static str },
    /// The tool is not pointed at the proxy at all: it is handed hooks that
    /// call this binary, and no proxy is started. `flag` is how one session is
    /// handed them — Cursor's `--plugin-dir` — so nothing the user owns is
    /// written. See [`hooks`](super::hooks) and docs/design/CURSOR.md.
    Hooks { flag: &'static str },
}

/// How an agent is offered the retrieval tools for the length of a session.
///
/// The proxy answers `optscale_kompressor_retrieve` in band, but only while a request is
/// passing through it. An agent holding a tool-server connection can resolve a
/// handle at any point in the session — see [`mcp`] for why that matters and
/// why this is deliberately not a durable registration.
///
/// [`mcp`]: super::mcp
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum McpWiring {
    /// The agent accepts a file of MCP server definitions on the command line,
    /// so the offer lasts exactly as long as the process does.
    ///
    /// An entry here is a claim that the flag exists and takes a path. A tool
    /// that does not have one gets `None`: registering it durably instead would
    /// trade a session-scoped offer for permanent residue in a file this
    /// command does not own, which is not a trade the user asked for.
    Flag { flag: &'static str },
}

/// One wrappable agent.
pub struct Agent {
    /// The name given to `kompressor wrap`.
    pub name: &'static str,
    /// The executable launched. Looked up on `PATH`.
    pub binary: &'static str,
    /// One line for `kompressor wrap --list`.
    pub about: &'static str,
    /// Base-URL variables to set, and the URL shape each wants.
    pub env: &'static [(&'static str, Endpoint)],
    /// Variables that are not URLs but that the tool needs set *because* it is
    /// being pointed at a proxy.
    ///
    /// Only ever set when absent from the environment: a value the user chose
    /// is a decision, and a wrap that silently overrode it would be changing
    /// behaviour it was not asked to change.
    pub fixed_env: &'static [(&'static str, &'static str)],
    /// Providers the proxy must be able to reach.
    pub providers: &'static [Provider],
    /// A config file to edit, for a tool no variable reaches.
    pub config: Option<ConfigFile>,
    /// How this tool is offered the retrieval tools, when it can be.
    pub mcp: Option<McpWiring>,
    /// How this tool reaches the proxy.
    pub wiring: Wiring,
    /// Printed after the variables, when setting them is known not to be
    /// enough for every version of the tool.
    ///
    /// The catalogue's promise is that a variable works. When a release breaks
    /// that promise the honest options are to drop the entry or to say so, and
    /// dropping it would take the versions that *do* work with it down too.
    pub note: Option<&'static str>,
}

/// The built-in catalogue.
///
/// Each variable below is the one the tool documents for pointing itself at an
/// API-compatible endpoint. Where a tool supports several providers, every
/// variable it might read is set, because which one it uses depends on the
/// model the user picked and the wrapper cannot see that choice.
pub const AGENTS: &[Agent] = &[
    Agent {
        name: "cursor-agent",
        // Also found as `agent`: the installer puts both on PATH and the
        // build names itself `agent`.
        binary: "cursor-agent",
        about: "Cursor Agent CLI — hook mode: tool output compressed by Cursor's own hooks, no proxy",
        env: &[],
        providers: &[],
        fixed_env: &[],
        config: None,
        mcp: None,
        // The wire is Connect RPC with protobuf bodies to Cursor's own
        // backend, and the prompt is assembled server-side: measured against
        // 2026.09.23-86fc751, every path 404s at a proxy. What Cursor does
        // offer is hooks, run with each tool result before the model sees it,
        // and `--plugin-dir` to hand them to one session. Measured against
        // 2026.09.26-dd393fe: docs/design/CURSOR.md, "Test results".
        wiring: Wiring::Hooks {
            flag: "--plugin-dir",
        },
        note: Some(
            "No proxy runs: the session is started with --plugin-dir naming a directory of \
             hooks that call kompressor, removed when it exits. MCP tool results are \
             compressed; shell output only with [hooks.cursor.shell] enabled; built-in Read, \
             Grep and Fetch output cannot be. For the editor: `kompressor hook install cursor`.",
        ),
    },
    Agent {
        name: "cursor",
        // Never executed: `Unsupported` short-circuits before any launch.
        binary: "cursor",
        about: "Cursor (editor) — use `kompressor hook install cursor`; a base-URL override cannot reach a local proxy",
        env: &[],
        providers: &[],
        fixed_env: &[],
        config: None,
        mcp: None,
        wiring: Wiring::Unsupported {
            why: "Cursor's editor is compressed through its hooks, not this proxy: run \
                  `kompressor hook install cursor`, then `kompressor hook status cursor`. \
                  The OpenAI base-URL override in Settings is called from Cursor's servers, \
                  not from the editor, so an address on this machine is never reached — which \
                  is why no request ever arrived here that way. Exposing the proxy on a public \
                  tunnel would put it, and your key, on the internet.",
        },
        note: None,
    },
    Agent {
        name: "claude",
        binary: "claude",
        about: "Claude Code (Anthropic Messages)",
        env: &[("ANTHROPIC_BASE_URL", Endpoint::Root)],
        providers: &[Provider::Anthropic],
        // Claude Code stops deferring MCP and system tool schemas the moment
        // `ANTHROPIC_BASE_URL` is a custom host, and materialises every one of
        // them into its context window instead. For a compression proxy that is
        // the worst possible side effect: wrapping a session to make the prompt
        // smaller would make it larger, break sub-agents and force compaction.
        // Reported upstream by headroom as GH #746; the cure is to ask for the
        // server-side tool search explicitly.
        fixed_env: &[("ENABLE_TOOL_SEARCH", "true")],
        config: None,
        // Claude Code reads `--mcp-config FILE` in addition to its own
        // configuration, which is what lets the retrieval tools be offered for
        // the length of one session without writing to `~/.claude.json`.
        // Measured against 2.1.278. `--strict-mcp-config` is deliberately not
        // passed: that would suppress the user's own servers, and this command
        // is here to add a tool, not to take theirs away.
        mcp: Some(McpWiring::Flag {
            flag: "--mcp-config",
        }),
        wiring: Wiring::Launch,
        note: Some(
            "Claude Code pins its prompt with a trailing cache_control breakpoint. The \
             blocks after that breakpoint are still compressed — the frozen floor is per \
             block, not per message — but everything before it is cache-hot and is left \
             alone, so expect less than from a client that pins nothing. Two side effects \
             of the custom base URL, neither of them kompressor's doing: Claude Code \
             v2.1.196 and newer disable `/remote-control` while it is set, and \
             ENABLE_TOOL_SEARCH is set to true on your behalf so it keeps deferring tool \
             schemas rather than inlining every one.",
        ),
    },
    Agent {
        name: "codex",
        binary: "codex",
        about: "OpenAI Codex CLI (OpenAI Responses), via a -c flag or its config file",
        // Deliberately empty. Codex ignores OPENAI_BASE_URL — measured on
        // 0.155.1, where a wrapped session sent every request straight to the
        // provider — and setting a variable nothing reads is how this command
        // fails without saying so. The config file does the work instead.
        env: &[],
        providers: &[Provider::OpenAi],
        fixed_env: &[],
        config: Some(ConfigFile::Codex),
        mcp: None,
        wiring: Wiring::Launch,
        note: Some(
            "codex ignores OPENAI_BASE_URL, so it is pointed at the proxy with \
             `-c openai_base_url` under a ChatGPT login, and with a block in its \
             config file under an API key. That block is removed when this session \
             exits; `kompressor unwrap codex` removes one left behind.",
        ),
    },
    Agent {
        name: "opencode",
        binary: "opencode",
        about: "opencode CLI/TUI and Desktop (openai, anthropic, OpenCode Zen and Go), via its config",
        // Deliberately empty, like codex. Measured on 2.2.4: OPENAI_BASE_URL or
        // ANTHROPIC_BASE_URL in the environment makes every model of that
        // provider "unavailable", and nothing is sent at all.
        env: &[],
        providers: &[Provider::Anthropic, Provider::OpenAi, Provider::OpenCodeZen],
        fixed_env: &[],
        config: Some(ConfigFile::OpenCode),
        mcp: None,
        wiring: Wiring::Launch,
        note: Some(
            "opencode is pointed at the proxy through its own configuration, not the \
             environment. A launched session runs with --standalone so its private server \
             sees the overlay. OpenCode Desktop, and a TUI already attached to the background \
             service, are reached with `kompressor wrap opencode --attach`, which edits the \
             global opencode.json and reloads the service; `kompressor unwrap opencode` \
             restores a file left behind. A provider whose baseURL you set yourself is left \
             alone and not compressed.",
        ),
    },
    Agent {
        name: "copilot",
        binary: "copilot",
        about: "GitHub Copilot CLI with your own provider key (BYOK: Anthropic by default)",
        // Copilot CLI's bring-your-own-key mode reads a provider type, a base
        // URL and a key. Anthropic takes the bare host; a user on an OpenAI key
        // sets COPILOT_PROVIDER_TYPE=openai and wraps with
        // `--env COPILOT_PROVIDER_BASE_URL=v1`, which the note says.
        env: &[("COPILOT_PROVIDER_BASE_URL", Endpoint::Root)],
        providers: &[Provider::Anthropic, Provider::OpenAi],
        fixed_env: &[("COPILOT_PROVIDER_TYPE", "anthropic")],
        config: None,
        mcp: None,
        wiring: Wiring::Launch,
        note: Some(
            "Only Copilot CLI's bring-your-own-key mode is reached: set \
             COPILOT_PROVIDER_API_KEY (or ANTHROPIC_API_KEY). A session signed in with a \
             Copilot subscription talks to GitHub's own API and is not compressed. For an \
             OpenAI key: COPILOT_PROVIDER_TYPE=openai kompressor wrap copilot \
             --env COPILOT_PROVIDER_BASE_URL=v1.",
        ),
    },
    Agent {
        name: "goose",
        binary: "goose",
        about: "Block's goose (Anthropic or OpenAI provider)",
        // goose reads a host per provider and appends its own path:
        // `OPENAI_BASE_PATH` defaults to `v1/chat/completions`, Anthropic's
        // client appends `/v1/messages`. Both want the bare origin.
        env: &[
            ("ANTHROPIC_HOST", Endpoint::Root),
            ("OPENAI_HOST", Endpoint::Root),
        ],
        providers: &[Provider::Anthropic, Provider::OpenAi],
        fixed_env: &[],
        config: None,
        mcp: None,
        wiring: Wiring::Launch,
        note: Some(
            "goose reads the provider from its own config (`goose configure`); the anthropic \
             and openai providers are reached. A custom OPENAI_BASE_PATH must still end in \
             v1/chat/completions or v1/responses for the request to be compressed.",
        ),
    },
    Agent {
        name: "cline",
        binary: "cline",
        about: "Cline (VS Code extension) — prints the base URLs to paste and holds the proxy up",
        // Labels, not variables: a manual entry sets nothing.
        env: &[
            ("Anthropic Base URL", Endpoint::Root),
            ("OpenAI Compatible Base URL", Endpoint::V1),
        ],
        providers: &[Provider::Anthropic, Provider::OpenAi],
        fixed_env: &[],
        config: None,
        mcp: None,
        wiring: Wiring::Manual {
            where_: "Cline › settings (gear) › API Provider › “Use custom base URL”",
        },
        note: Some(
            "Cline calls the provider from the VS Code extension host on this machine, so a \
             local base URL is reached. Pick the Anthropic provider, or OpenAI Compatible, \
             and paste the matching URL below.",
        ),
    },
    Agent {
        name: "continue",
        binary: "cn",
        about: "Continue (VS Code / JetBrains) — prints the apiBase to put in config.yaml",
        env: &[
            ("apiBase for provider: anthropic", Endpoint::Root),
            ("apiBase for provider: openai", Endpoint::V1),
        ],
        providers: &[Provider::Anthropic, Provider::OpenAi],
        fixed_env: &[],
        config: None,
        mcp: None,
        wiring: Wiring::Manual {
            where_: "~/.continue/config.yaml › models › each model › apiBase",
        },
        note: Some(
            "Continue reads a per-model apiBase from ~/.continue/config.yaml; the file is not \
             edited here. Add apiBase to each anthropic or openai model, reload the window, \
             and remove it when you are done.",
        ),
    },
    Agent {
        name: "aider",
        binary: "aider",
        about: "aider (OpenAI Chat or Anthropic Messages, by model)",
        env: &[
            ("OPENAI_API_BASE", Endpoint::V1),
            ("ANTHROPIC_API_BASE", Endpoint::Root),
        ],
        providers: &[Provider::Anthropic, Provider::OpenAi],
        fixed_env: &[],
        config: None,
        mcp: None,
        wiring: Wiring::Launch,
        note: None,
    },
];

impl Agent {
    /// Look up an agent by name.
    pub fn find(name: &str) -> Option<&'static Agent> {
        AGENTS.iter().find(|agent| agent.name == name)
    }

    /// The catalogue as a `name — about` listing.
    pub fn listing() -> String {
        let width = AGENTS.iter().map(|a| a.name.len()).max().unwrap_or(0);
        AGENTS
            .iter()
            .map(|a| format!("  {:width$}  {}", a.name, a.about, width = width))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The upstreams a set of providers needs, in match order.
///
/// Anthropic first, always: its `/v1/messages` is a strict extension of the
/// OpenAI mount's `/v1`, so the order is what keeps a Messages request from
/// being forwarded to OpenAI.
pub fn upstreams_for(providers: &[Provider]) -> Vec<Upstream> {
    let mut out = Vec::new();
    for provider in [Provider::Anthropic, Provider::OpenAi, Provider::OpenCodeZen] {
        if providers.contains(&provider) {
            out.push(provider.upstream());
        }
    }
    out
}

/// Parse a `--env NAME=root|v1` override into a catalogue-shaped entry.
pub fn parse_env_override(spec: &str) -> Result<(String, Endpoint), String> {
    let (name, shape) = spec
        .split_once('=')
        .ok_or_else(|| format!("expected NAME=root or NAME=v1, got {spec:?}"))?;
    let name = name.trim();
    if name.is_empty() {
        return Err(format!("no variable name in {spec:?}"));
    }
    let shape = match shape.trim() {
        "root" => Endpoint::Root,
        "v1" => Endpoint::V1,
        other => {
            return Err(format!(
                "unknown URL shape {other:?} in {spec:?}: use `root` for a host \
                 the client appends /v1/… to, or `v1` for a URL that already ends in /v1"
            ));
        }
    };
    Ok((name.to_string(), shape))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_editor_points_at_hook_install_and_prints_no_loopback_url() {
        // The base-URL override is called from Cursor's servers, so a URL on
        // this machine is never reached (docs/design/CURSOR.md, "Bring-your-own-key"). An
        // entry that printed one would send the user to paste something that
        // cannot work.
        let cursor = Agent::find("cursor").expect("cursor is in the catalogue");
        let Wiring::Unsupported { why } = cursor.wiring else {
            panic!("the editor is not wrapped; it is installed into");
        };
        assert!(why.contains("hook install cursor"));
        assert!(!why.contains("127.0.0.1"));
        assert!(cursor.env.is_empty() && cursor.providers.is_empty());
    }

    #[test]
    fn the_cursor_cli_is_handed_hooks_and_no_proxy() {
        // Connect RPC with protobuf bodies: no proxy could read it. Hooks see
        // each tool result before the model does, and `--plugin-dir` hands
        // them to one session.
        let cli = Agent::find("cursor-agent").expect("the CLI is in the catalogue");
        assert_eq!(
            cli.wiring,
            Wiring::Hooks {
                flag: "--plugin-dir"
            }
        );
        assert!(cli.env.is_empty(), "no variable is set");
        assert!(cli.providers.is_empty(), "no upstream is mounted");
        assert!(!cli.about.contains("cannot be compressed"));
    }

    #[test]
    fn the_editor_and_the_cli_are_different_entries() {
        let editor = Agent::find("cursor").unwrap();
        let cli = Agent::find("cursor-agent").unwrap();
        assert!(matches!(editor.wiring, Wiring::Unsupported { .. }));
        assert!(matches!(cli.wiring, Wiring::Hooks { .. }));
    }

    #[test]
    fn claude_is_told_to_keep_deferring_tool_schemas() {
        // The custom base URL is what triggers it: Claude Code inlines every
        // MCP and system tool schema into its context window when it sees one
        // and ENABLE_TOOL_SEARCH is unset. A compression proxy that inflates
        // the prompt it was pointed at is worse than no proxy.
        let claude = Agent::find("claude").expect("claude is in the catalogue");
        assert_eq!(claude.fixed_env, &[("ENABLE_TOOL_SEARCH", "true")]);
        let note = claude.note.expect("the side effects must be stated");
        assert!(
            note.contains("remote-control"),
            "the feature the base URL silently disables has to be named"
        );
        assert!(
            note.contains("per block, not per message"),
            "and the note must not still say a wrapped session compresses nothing"
        );
    }

    #[test]
    fn only_claude_and_copilot_carry_a_fixed_variable() {
        // `fixed_env` is for a variable a tool needs *because* it is proxied,
        // not a convenient place to set defaults. Claude Code needs tool
        // search kept on; Copilot CLI's BYOK base URL means nothing without a
        // provider type beside it.
        for agent in AGENTS
            .iter()
            .filter(|a| a.name != "claude" && a.name != "copilot")
        {
            assert!(
                agent.fixed_env.is_empty(),
                "{} sets a fixed variable with no reason recorded",
                agent.name
            );
        }
    }

    #[test]
    fn a_tool_server_is_only_offered_to_an_agent_that_can_be_handed_one() {
        // The claim in `McpWiring::Flag` is that the flag exists and takes a
        // path. Getting it wrong is not a silent failure — the agent refuses an
        // unknown argument and never starts — but it is a failure in the one
        // command whose job is to launch it, so the catalogue keeps the claim
        // narrow: today exactly one entry has been measured.
        for agent in AGENTS.iter().filter(|a| a.name != "claude") {
            assert!(
                agent.mcp.is_none(),
                "{} offers a tool server on a flag nobody measured",
                agent.name
            );
        }
        assert_eq!(
            Agent::find("claude").unwrap().mcp,
            Some(McpWiring::Flag {
                flag: "--mcp-config"
            })
        );
    }

    #[test]
    fn an_unsupported_agent_is_never_handed_a_tool_server() {
        // An entry that cannot be wrapped at all must not acquire a launch-time
        // side effect through this field: `Unsupported` short-circuits before
        // anything runs, so a registration written for one would be a file
        // written for a session that never happens.
        for agent in AGENTS.iter().filter(|a| {
            matches!(
                a.wiring,
                Wiring::Unsupported { .. } | Wiring::Manual { .. } | Wiring::Hooks { .. }
            )
        }) {
            assert!(
                agent.mcp.is_none(),
                "{} is never launched, so it can carry no command-line offer",
                agent.name
            );
        }
    }

    #[test]
    fn every_other_agent_still_launches() {
        // The new enum defaults nothing: an entry that forgot to say how it is
        // wired would not compile, but one that said `Manual` by accident would
        // silently stop launching. This is the guard for the second.
        const MANUAL: [&str; 2] = ["cline", "continue"];
        for agent in AGENTS
            .iter()
            .filter(|a| !a.name.starts_with("cursor") && !MANUAL.contains(&a.name))
        {
            assert_eq!(
                agent.wiring,
                Wiring::Launch,
                "{} should still be launched",
                agent.name
            );
        }
    }

    #[test]
    fn the_two_url_shapes_differ_only_in_the_version_segment() {
        assert_eq!(Endpoint::Root.url(8817), "http://127.0.0.1:8817");
        assert_eq!(Endpoint::V1.url(8817), "http://127.0.0.1:8817/v1");
    }

    #[test]
    fn anthropic_is_matched_before_openai() {
        // `/v1` as a prefix also matches `/v1/messages`, and the proxy takes the
        // first upstream that matches — so this ordering is the whole reason a
        // Messages request does not end up at OpenAI.
        let both = upstreams_for(&[Provider::OpenAi, Provider::Anthropic]);
        assert_eq!(both.len(), 2);
        assert_eq!(both[0].name, "anthropic");
        assert_eq!(both[0].path_prefix, "/v1/messages");
        assert_eq!(both[1].name, "openai");
        assert!(
            both[1].path_prefix.len() < both[0].path_prefix.len(),
            "the broader prefix must come second"
        );
    }

    #[test]
    fn a_single_provider_agent_mounts_only_what_it_needs() {
        let one = upstreams_for(&[Provider::Anthropic]);
        assert_eq!(one.len(), 1, "no upstream the agent cannot reach");
        assert_eq!(one[0].name, "anthropic");
    }

    #[test]
    fn every_catalogue_entry_declares_an_upstream_for_every_variable_it_sets() {
        // An agent pointed at the proxy for a provider the proxy cannot route is
        // the silent failure this catalogue exists to prevent: the request
        // arrives and 404s instead of reaching the model.
        //
        // An `Unsupported` entry is exempt and has to be: it mounts nothing on
        // purpose, because the whole claim it makes is that no mount would
        // help. It is refused before a proxy is started at all. A `Hooks`
        // entry starts no proxy either.
        for agent in AGENTS
            .iter()
            .filter(|a| !matches!(a.wiring, Wiring::Unsupported { .. } | Wiring::Hooks { .. }))
        {
            assert!(
                !agent.providers.is_empty(),
                "{} mounts no upstream",
                agent.name
            );
            // A config-file agent sets no variable by design, so the count can
            // only be checked where variables are the mechanism.
            // At most one variable per provider: Copilot CLI sets one URL and
            // mounts both, because which provider it is depends on a type the
            // user may switch.
            if agent.config.is_none() {
                assert!(
                    !agent.env.is_empty() && agent.env.len() <= agent.providers.len(),
                    "{}: every base URL needs a provider behind it",
                    agent.name
                );
            }
        }
    }

    #[test]
    fn every_catalogue_entry_is_wired_to_the_proxy_somehow() {
        // The one thing an entry must never be is inert. An agent with neither
        // a variable nor a config file would start a proxy, run the tool, and
        // let it talk to the provider directly — which is precisely the
        // outcome this command exists to prevent, and it prints no error.
        //
        // Two wirings are inert by design and say so: `Manual` is wired by the
        // user's own hands, `Unsupported` is not wired because it cannot be.
        // The invariant is about entries that *claim* to route traffic.
        for agent in AGENTS {
            match agent.wiring {
                // `Hooks` routes nothing through the proxy: the agent calls
                // this binary from its own hook points.
                Wiring::Manual { .. } | Wiring::Unsupported { .. } | Wiring::Hooks { .. } => {
                    continue;
                }
                Wiring::Launch => assert!(
                    !agent.env.is_empty() || agent.config.is_some(),
                    "{} is wired to the proxy by nothing at all",
                    agent.name
                ),
            }
        }
    }

    #[test]
    fn opencode_is_configured_and_never_handed_a_base_url_variable() {
        // Measured on 2.2.4: either variable makes that provider's models
        // "unavailable" and nothing is sent. The old entry set one.
        let opencode = Agent::find("opencode").expect("opencode is in the catalogue");
        assert!(opencode.env.is_empty());
        assert_eq!(opencode.config, Some(ConfigFile::OpenCode));
        assert_eq!(
            opencode.providers,
            &[Provider::Anthropic, Provider::OpenAi, Provider::OpenCodeZen]
        );
        let note = opencode.note.expect("the wiring must be explained");
        assert!(note.contains("--attach") && note.contains("Desktop"));
    }

    #[test]
    fn the_zen_mount_serves_both_tiers_without_shadowing_openai() {
        let ups = upstreams_for(&[Provider::Anthropic, Provider::OpenAi, Provider::OpenCodeZen]);
        let names: Vec<&str> = ups.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, ["anthropic", "openai", "opencode-zen"]);
        let zen = &ups[2];
        assert_eq!(zen.base_url, "https://opencode.ai");
        assert!(!zen.strip_prefix, "the relay expects /zen/... unchanged");
        let proxy = kompressor_core::config::ProxyConfig {
            upstreams: ups.clone(),
            ..Default::default()
        };
        let pick = |path: &str| {
            kompressor_proxy::route::match_upstream(&proxy, path).map(|u| u.name.clone())
        };
        assert_eq!(
            pick("/zen/go/v1/chat/completions").as_deref(),
            Some("opencode-zen")
        );
        assert_eq!(pick("/zen/v1/messages").as_deref(), Some("opencode-zen"));
        assert_eq!(pick("/v1/responses").as_deref(), Some("openai"));
        assert_eq!(pick("/v1/messages").as_deref(), Some("anthropic"));
    }

    #[test]
    fn the_openai_mount_can_follow_a_subscription_caller() {
        // Codex under a ChatGPT plan sends the same body to a different host.
        // Without this the request is forwarded to api.openai.com, 404s, and
        // Codex concludes the subscription session is invalid.
        let openai = Provider::OpenAi.upstream();
        let over = openai
            .subscription
            .expect("the OpenAI mount must know where a subscriber's traffic goes");
        assert_eq!(over.base_url, "https://chatgpt.com");
        assert_eq!(over.rewrite_prefix, "/backend-api/codex");
    }

    #[test]
    fn env_overrides_parse_or_explain_themselves() {
        assert_eq!(
            parse_env_override("MY_BASE=v1").unwrap(),
            ("MY_BASE".to_string(), Endpoint::V1)
        );
        assert_eq!(
            parse_env_override(" SPACED = root ").unwrap(),
            ("SPACED".to_string(), Endpoint::Root)
        );
        assert!(parse_env_override("NOSHAPE").is_err());
        assert!(parse_env_override("=root").is_err());
        let message = parse_env_override("X=https://example").unwrap_err();
        assert!(
            message.contains("root") && message.contains("v1"),
            "the error has to say what the valid shapes are: {message}"
        );
    }

    /// A wrong binary name fails in the loudest possible way — `wrap` aborts
    /// with "not on PATH" before the agent ever starts — and is invisible to
    /// every other test here, because they all check the catalogue against
    /// itself. `opencode` shipped as `opencode-cli` for exactly that reason.
    ///
    /// Every agent's executable is currently its own name. If you add one where
    /// that is not true, put it in `RENAMED` *and check the spelling against
    /// the tool you are naming*, because nothing else will.
    #[test]
    fn an_agents_binary_is_its_name_unless_it_is_listed_otherwise() {
        // Continue's CLI is `cn`; the entry is manual and never launched, but
        // the name is still the one a user would look for.
        const RENAMED: &[(&str, &str)] = &[("continue", "cn")];
        for agent in AGENTS {
            let expected = RENAMED
                .iter()
                .find(|(name, _)| *name == agent.name)
                .map(|(_, binary)| *binary)
                .unwrap_or(agent.name);
            assert_eq!(
                agent.binary, expected,
                "{}: the catalogue launches {:?}; add it to RENAMED if that is \
                 deliberate",
                agent.name, agent.binary
            );
        }
    }

    #[test]
    fn a_binary_name_is_a_bare_executable_not_a_path_or_a_command_line() {
        for agent in AGENTS {
            assert!(
                !agent.binary.is_empty()
                    && !agent.binary.contains('/')
                    && !agent.binary.contains(char::is_whitespace),
                "{}: {:?} is not something PATH lookup can find",
                agent.name,
                agent.binary
            );
        }
    }

    #[test]
    fn names_are_unique_so_lookup_is_unambiguous() {
        let mut names: Vec<&str> = AGENTS.iter().map(|a| a.name).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate agent name");
        assert!(Agent::find("claude").is_some());
        assert!(Agent::find("opencode").is_some());
        for name in ["copilot", "goose", "cline", "continue"] {
            assert!(Agent::find(name).is_some(), "{name}");
        }
        assert!(Agent::find("nope").is_none());
    }
}
