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
//! can be configured. `opencode` reads `OPENAI_BASE_URL` for its built-in
//! `openai` provider, but a provider declared in `opencode.json` carries its own
//! `options.baseURL`, and a config value beats an environment one — so wrapping
//! a session that uses a custom provider sets a variable nothing reads. That is
//! the silent failure above, reached by a different road. For those, point the
//! provider's own `baseURL` at the proxy instead; `--attach` exists to hold a
//! proxy up while an externally configured agent uses it.

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
    Manual { where_: &'static str },
    /// This proxy cannot compress the tool, and an entry that says so is worth
    /// more than no entry at all.
    ///
    /// Without one, `wrap` answers "unknown agent" and points at
    /// `--env NAME=root|v1`, which for a tool that speaks another protocol sets
    /// a variable that either does nothing or breaks it — the silent failure
    /// this catalogue exists to avoid, reached by the helpful road.
    Unsupported { why: &'static str },
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
        binary: "cursor-agent",
        about: "Cursor Agent CLI — cannot be compressed (see below)",
        env: &[],
        providers: &[],
        fixed_env: &[],
        config: None,
        mcp: None,
        // Measured against 2026.09.23-86fc751 by pointing `--endpoint` at a
        // recording server: every call is Connect RPC carrying protobuf —
        // `content-type: application/proto`, `connect-protocol-version: 1`,
        // paths like `/aiserver.v1.ServerConfigService/GetServerConfig`. There
        // is no JSON message array for a wire walker to find and no OpenAI or
        // Anthropic shape to route, so the engine has nothing to read. Pointed
        // at the proxy for real, every path 404s and the agent stops with
        // `Error: [unimplemented]`.
        wiring: Wiring::Unsupported {
            why: "cursor-agent talks to Cursor's own API over Connect RPC with protobuf \
                  bodies, not the OpenAI or Anthropic JSON this proxy understands. There \
                  is nothing in those requests the engine can read, so wrapping it would \
                  route traffic through a proxy that cannot compress a byte of it — and \
                  in fact cannot answer it at all: the paths 404 and the agent stops with \
                  `[unimplemented]`. Cursor's *editor* can be pointed at this proxy when \
                  it is using your own API key: `kompressor wrap cursor`.",
        },
        note: None,
    },
    Agent {
        name: "cursor",
        // Never executed: `Wiring::Manual` short-circuits before any launch.
        // Named rather than left empty so `--list` and the error paths have
        // something truthful to print.
        binary: "cursor",
        about: "Cursor (GUI editor; base URL set by hand in Settings)",
        // Rendered into the instructions, not exported: Cursor is not a child
        // process of this command and never sees its environment.
        env: &[("OPENAI_BASE_URL", Endpoint::V1)],
        providers: &[Provider::OpenAi],
        fixed_env: &[],
        config: None,
        mcp: None,
        wiring: Wiring::Manual {
            where_: "Settings \u{2192} Models \u{2192} OpenAI API Key \u{2192} Override OpenAI Base URL",
        },
        note: Some(
            "REQUIRES your own OpenAI API key in Settings \u{2192} Models \u{2192} API Keys. \
             On a Cursor subscription every request goes to Cursor's own servers over \
             their protobuf API, which this proxy never sees and could not read if it \
             did — verified 2026-09-25 against a live editor: with the key field empty \
             and the override toggle off, no request reaches the proxy at all. Cursor \
             also cannot send custom headers, so a session is attributed to no project.",
        ),
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
        about: "OpenAI Codex CLI (OpenAI Responses), via its config file",
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
        about: "opencode (OpenAI Chat; only for its built-in `openai` provider)",
        env: &[("OPENAI_BASE_URL", Endpoint::V1)],
        providers: &[Provider::OpenAi],
        fixed_env: &[],
        config: None,
        mcp: None,
        wiring: Wiring::Launch,
        note: Some(
            "only for opencode's built-in `openai` provider: a provider declared in \
             opencode.json carries its own options.baseURL, and config beats environment.",
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
    for provider in [Provider::Anthropic, Provider::OpenAi] {
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
    fn a_gui_tool_is_wired_by_hand_and_never_launched() {
        let cursor = Agent::find("cursor").expect("cursor is in the catalogue");
        let Wiring::Manual { where_ } = cursor.wiring else {
            panic!("cursor cannot be launched: it is an editor, not a CLI");
        };
        assert!(
            where_.contains("Base URL"),
            "the instruction has to name the setting, got {where_:?}"
        );
        // The proxy must still be reachable for the endpoint the user pastes.
        assert_eq!(cursor.providers, &[Provider::OpenAi]);
        // …and the URL handed over is the one an OpenAI-style client wants:
        // Cursor appends /chat/completions to it.
        assert_eq!(cursor.env, &[("OPENAI_BASE_URL", Endpoint::V1)]);
        // Both facts, because either alone misleads: the key is what makes it
        // work, and the subscription is what most readers actually have.
        let note = cursor.note.expect("the precondition must be stated");
        assert!(
            note.contains("own OpenAI API key"),
            "the condition under which this works at all must be stated"
        );
        assert!(
            note.contains("subscription"),
            "and the case in which it cannot work, which is the common one"
        );
    }

    #[test]
    fn the_cursor_cli_is_listed_as_incompatible_rather_than_omitted() {
        // Measured against 2026.09.23-86fc751: Connect RPC, protobuf bodies,
        // `/aiserver.v1.*` paths. Leaving it out of the catalogue is worse than
        // saying so — `wrap` would answer "unknown agent" and recommend
        // `--env`, which points a tool at a proxy that 404s every request.
        let cli = Agent::find("cursor-agent").expect("the CLI is in the catalogue");
        let Wiring::Unsupported { why } = cli.wiring else {
            panic!("cursor-agent speaks protobuf; it cannot be wrapped");
        };
        assert!(why.contains("protobuf"), "the reason has to be the reason");
        assert!(
            why.contains("wrap cursor"),
            "and it has to point at the thing that does work"
        );
        assert!(cli.env.is_empty(), "no variable would help");
        assert!(cli.providers.is_empty(), "no upstream would help either");
    }

    #[test]
    fn the_editor_and_the_cli_are_different_entries() {
        // They are different products with different protocols, and conflating
        // them is how a user ends up pasting a URL that cannot work.
        let editor = Agent::find("cursor").unwrap();
        let cli = Agent::find("cursor-agent").unwrap();
        assert!(matches!(editor.wiring, Wiring::Manual { .. }));
        assert!(matches!(cli.wiring, Wiring::Unsupported { .. }));
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
    fn only_claude_carries_a_fixed_variable() {
        // `fixed_env` is for a variable a tool needs *because* it is proxied,
        // not a convenient place to set defaults. One entry needs one today.
        for agent in AGENTS.iter().filter(|a| a.name != "claude") {
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
        for agent in AGENTS
            .iter()
            .filter(|a| matches!(a.wiring, Wiring::Unsupported { .. } | Wiring::Manual { .. }))
        {
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
        for agent in AGENTS.iter().filter(|a| !a.name.starts_with("cursor")) {
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
        // help. It is refused before a proxy is started at all.
        for agent in AGENTS
            .iter()
            .filter(|a| !matches!(a.wiring, Wiring::Unsupported { .. }))
        {
            assert!(
                !agent.providers.is_empty(),
                "{} mounts no upstream",
                agent.name
            );
            // A config-file agent sets no variable by design, so the count can
            // only be checked where variables are the mechanism.
            if agent.config.is_none() {
                assert_eq!(
                    agent.env.len(),
                    agent.providers.len(),
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
                Wiring::Manual { .. } | Wiring::Unsupported { .. } => continue,
                Wiring::Launch => assert!(
                    !agent.env.is_empty() || agent.config.is_some(),
                    "{} is wired to the proxy by nothing at all",
                    agent.name
                ),
            }
        }
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
        const RENAMED: &[(&str, &str)] = &[];
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
        assert!(Agent::find("nope").is_none());
    }
}
