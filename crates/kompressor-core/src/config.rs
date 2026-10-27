//! Layered configuration.
//!
//! Precedence, lowest to highest: built-in defaults, a TOML file, the
//! environment, then whatever the caller sets programmatically (the CLI). The
//! defaults alone are a working single-tenant deployment, so a first run needs
//! no file at all.
//!
//! Environment names come in two shapes. The structured one is
//! `KOMPRESSOR__SECTION__KEY`; the flat one is the `KOMPRESSOR_<FIELD>` set the
//! service this replaces used, which is honoured so an existing deployment's
//! environment keeps working unchanged.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use kompressor_engine::EngineOptions;
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

fn default_listen() -> String {
    "127.0.0.1:8817".to_string()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub engine: EngineOptions,
    pub refs: RefsConfig,
    pub stats: StatsFileConfig,
    pub retrieve: RetrieveConfig,
    pub mcp: McpConfig,
    pub proxy: ProxyConfig,
    pub log: LogConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: String,
    /// Per-surface listeners; empty means "share `listen`".
    pub api_listen: String,
    pub mcp_listen: String,
    pub proxy_listen: String,
    pub max_body_bytes: usize,
    /// 0 means twice the detected parallelism.
    ///
    /// **It bounds the HTTP API surface and nothing else**, which is narrower
    /// than the name reads when `serve` is running the proxy and the MCP tool
    /// server in the same process. `ServerState` owns the only semaphore in the
    /// workspace; the proxy spawns its engine work directly
    /// (`handler.rs::compress_with_deadline`) and so does MCP, so a burst on
    /// either is bounded by tokio's blocking pool rather than by this number.
    ///
    /// Worth knowing because a compression timeout does not *cancel* the
    /// blocking task it gave up on: slow proxy requests can each start another,
    /// fall open after the deadline, and leave the work running while more
    /// arrives. Closing it means moving admission into shared state every
    /// surface acquires, and keeping the permit owned until the work actually
    /// finishes — not merely until the caller stops waiting.
    pub max_inflight_compressions: usize,
    #[serde(with = "humantime_serde")]
    pub queue_timeout: Duration,
    #[serde(with = "humantime_serde")]
    pub shutdown_grace: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            listen: default_listen(),
            api_listen: String::new(),
            mcp_listen: String::new(),
            proxy_listen: String::new(),
            max_body_bytes: 64 * 1024 * 1024,
            max_inflight_compressions: 0,
            queue_timeout: Duration::from_secs(2),
            shutdown_grace: Duration::from_secs(20),
        }
    }
}

impl ServerConfig {
    /// Effective parallelism limit for engine work.
    pub fn inflight_limit(&self) -> usize {
        if self.max_inflight_compressions > 0 {
            return self.max_inflight_compressions;
        }
        std::thread::available_parallelism()
            .map(|n| n.get() * 2)
            .unwrap_or(4)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// One anonymous tenant. The right default for a local process.
    #[default]
    None,
    ApiKeys,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    pub mode: AuthMode,
    pub keys: Vec<ApiKey>,
    /// A shared secret that lets a trusted internal caller name its own tenant.
    pub secret: String,
}

impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig {
            mode: AuthMode::None,
            keys: Vec::new(),
            secret: String::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApiKey {
    pub key: String,
    pub tenant: String,
    pub enabled: bool,
    /// Optional per-tenant override of the compression mode.
    pub mode: Option<kompressor_engine::Mode>,
}

impl Default for ApiKey {
    fn default() -> Self {
        ApiKey {
            key: String::new(),
            tenant: String::new(),
            enabled: true,
            mode: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RefsBackend {
    #[default]
    Sqlite,
    Memory,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RefsConfig {
    pub backend: RefsBackend,
    pub path: PathBuf,
    pub recent_cache_entries: usize,
    #[serde(with = "humantime_serde")]
    pub purge_interval: Duration,
    pub max_bytes: u64,
}

impl Default for RefsConfig {
    fn default() -> Self {
        RefsConfig {
            backend: RefsBackend::Sqlite,
            path: PathBuf::from("./data/refs.db"),
            recent_cache_entries: 4096,
            purge_interval: Duration::from_secs(60),
            max_bytes: 1024 * 1024 * 1024,
        }
    }
}

/// The CONNECT listener that lets a client which pins its own base URL be
/// compressed anyway.
///
/// A client that reads `ANTHROPIC_BASE_URL` needs none of this: point it at the
/// proxy and the bytes arrive. Claude Desktop sets that variable itself when it
/// spawns a session, overwriting both the inherited environment and
/// `settings.json`, so there is no value to set. What it does honour is a proxy
/// and a certificate authority, which is what this serves.
///
/// Off unless [`Self::listen`] names an address. A CONNECT proxy that appeared
/// because somebody upgraded would be an unpleasant surprise, and a listener
/// holding a signing key more so.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InterceptConfig {
    /// Where to accept CONNECT. `None`, the default, serves nothing.
    ///
    /// One field rather than a bool beside an address, which can contradict
    /// itself.
    pub listen: Option<String>,
    /// Hosts whose TLS this terminates. Everything else is tunnelled blind.
    ///
    /// Exact, lowercased hostnames; no globs and no patterns. mitmproxy takes a
    /// regex here and the anchors in `'^api\.anthropic\.com:443$'` are doing
    /// real work — a pattern that loses them intercepts hosts nobody meant it
    /// to, quietly, while still appearing to function. Equality cannot.
    pub intercept_hosts: Vec<String>,
    /// Directory holding `ca.pem` and `ca.key`, created on first use.
    pub ca_path: PathBuf,
    /// What to advertise in ALPN.
    ///
    /// Both, in preference order, because `http/1.1` alone does not work.
    /// Claude Desktop's client offers **only** `h2` for `api.anthropic.com`,
    /// and a server that cannot speak it answers `no_application_protocol`:
    /// the client hangs up mid-handshake and the listener logs a handshake
    /// EOF, which looks exactly like an untrusted certificate and is not one.
    /// Measured with `openssl s_client -alpn h2` against this listener —
    /// alert 120 with `Verify return code: 0 (ok)`, trust intact, protocol
    /// refused.
    pub alpn: Vec<String>,
    /// How many minted leaves to keep. Bounds memory, not disk; nothing about a
    /// leaf is written down.
    pub leaf_cache_entries: usize,
}

impl Default for InterceptConfig {
    fn default() -> Self {
        InterceptConfig {
            listen: None,
            intercept_hosts: Vec::new(),
            ca_path: PathBuf::from("./data/ca"),
            alpn: vec!["h2".to_string(), "http/1.1".to_string()],
            leaf_cache_entries: 64,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatsFileConfig {
    pub enabled: bool,
    pub path: PathBuf,
    pub queue_size: usize,
    #[serde(with = "humantime_serde")]
    pub flush_interval: Duration,
    pub retention_days: u32,
}

impl Default for StatsFileConfig {
    fn default() -> Self {
        StatsFileConfig {
            enabled: true,
            path: PathBuf::from("./data/stats.db"),
            queue_size: 16_384,
            flush_interval: Duration::from_millis(100),
            retention_days: 90,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetrieveConfig {
    /// Cap on what one retrieval may return.
    ///
    /// A retrieval re-injects its fragment into the next call, which the model
    /// pays for again. Without a cap a retrieval loop can cost more than the
    /// compression saved, so an oversized fragment is truncated and the response
    /// says so, pointing the caller at a range instead.
    pub max_retrieve_chars: usize,
}

impl Default for RetrieveConfig {
    fn default() -> Self {
        RetrieveConfig {
            max_retrieve_chars: 32_768,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    pub enabled: bool,
    pub path: String,
    /// Empty means any host is accepted, which is right for an internal service.
    pub allowed_hosts: Vec<String>,
    pub max_retrievals_per_minute: u32,
}

impl Default for McpConfig {
    fn default() -> Self {
        McpConfig {
            enabled: true,
            path: "/mcp".to_string(),
            allowed_hosts: Vec::new(),
            max_retrievals_per_minute: 120,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RetrieveTool {
    /// Offer the tool only when the outgoing request carries a handle.
    #[default]
    Auto,
    Always,
    Never,
}

/// Where the frozen prefix ends: what the client says, or what the provider
/// has actually cached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FrozenPrefix {
    /// The client's own `cache_control` marker decides, which is where the
    /// cache will be **after** this request. The shipped default, and the
    /// behaviour every measurement so far was taken under.
    #[default]
    Marker,
    /// Only as far as the provider's cache reached when this proxy first saw
    /// the conversation. Everything after that is not in any cache entry yet,
    /// so compressing it cannot invalidate one.
    ///
    /// **Off by default, and it changes what the provider is billed for.** On
    /// a 20-turn Harbor run the marker rule froze `frozen + opaque ==
    /// messages_total` on every single turn — the engine was handed nothing it
    /// could move — while 19,431 tokens of conversation tail were written to
    /// cache across those turns at a lifecycle cost of $0.1417 out of a $0.4202
    /// bill. That tail is what this unlocks.
    ///
    /// The risk it takes is the transition. A conversation the proxy joins
    /// mid-way keeps whatever floor the client had already established, so
    /// nothing already cached is rewritten; but the engine's output must be
    /// byte-stable turn over turn or every later turn misses the cache from the
    /// first compressed byte. That property is tested
    /// (`a_message_compresses_the_same_however_much_follows_it`) and handles
    /// are content-addressed, which is why this is offered at all — but it is
    /// the assumption to check first if a run with this on bills more, not
    /// less.
    FirstSeen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RetrieveExecution {
    /// Run the retrieve loop here, so recoverable mode works for any client.
    #[default]
    Server,
    /// Hand the tool call to the client, which resolves it itself.
    Client,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProxyConfig {
    /// Whether the proxy surface is served at all.
    ///
    /// False does not mean "forward unchanged" -- `serve` does not mount the
    /// proxy router, so every proxied path answers 404. Use [`compress`] for a
    /// proxy that runs and forwards without compressing.
    ///
    /// [`compress`]: ProxyConfig::compress
    pub enabled: bool,
    /// Whether a served proxy compresses, as opposed to forwarding unchanged.
    ///
    /// False is the uncompressed control arm: the router is mounted, bodies are
    /// parsed, usage and cache reads are recorded, and nothing is rewritten --
    /// so a baseline run differs from a compressed one in compression and in
    /// nothing else, over the same network path. Turning the proxy off instead
    /// removes the hop and the telemetry with it, and then the comparison
    /// measures several things at once.
    ///
    /// It resolves to `ProxyMode::Off`, so the response says so on
    /// `x-kompressor-mode`: a control arm indistinguishable from a compressed
    /// one in its own telemetry is not a control arm.
    pub compress: bool,
    /// Skip the whole compression path below this much compressible text.
    pub min_tokens: u32,
    #[serde(with = "humantime_serde")]
    pub compress_timeout: Duration,
    pub max_body_bytes: usize,
    #[serde(with = "humantime_serde")]
    pub upstream_connect_timeout: Duration,
    #[serde(with = "humantime_serde")]
    pub upstream_read_timeout: Duration,
    pub retrieve_tool: RetrieveTool,
    pub retrieve_execution: RetrieveExecution,
    /// Where the frozen prefix ends. See [`FrozenPrefix`].
    pub frozen_prefix: FrozenPrefix,
    /// How many retrieve rounds may be **answered** in one request.
    ///
    /// The metric counts one more than this. Reaching the cap does not end the
    /// exchange: the next call is answered with `retrieval budget exhausted` so
    /// the model learns it has run out rather than being cut off mid-turn, and
    /// that refusal is a round trip too, so `proxy_requests.retrieve_rounds`
    /// reads `retrieve_max_rounds + 1` on a request that spent the budget.
    /// Documented here because a metric that exceeds its own configured cap
    /// reads as a bug to everyone who meets it for the first time.
    pub retrieve_max_rounds: u32,
    pub retrieve_max_tokens_per_request: u32,
    /// Ceiling on everything kept inline for one request, as a percent of the
    /// tokens the engine looked at. 0 disables the ceiling.
    ///
    /// The per-message guard in the engine asks whether *this* message is worth
    /// keeping whole; this asks whether the set has grown too large. They catch
    /// different things, and the second exists because the first cannot see the
    /// difference. That guard prices a single turn and commits to every
    /// remaining one: measured across four runs of one terminal-bench task, a
    /// decision that looked like 2.4x forfeit-to-protected when it was taken
    /// cost 80.7x over the 34 turns it then applied to, and the guard still had
    /// roughly 13x headroom. A share of the context bounds the total however
    /// many individually-reasonable decisions produce it, and scales with the
    /// conversation rather than needing a number per deployment.
    ///
    /// **Calibrated, not derived**, unlike `KEEP_INLINE_MAX_FORFEIT`, which it
    /// backs up. That one is now the provider's measured cache discount and
    /// refuses any single decision that does not pay; this one is a blunt
    /// ceiling on the total, for the case where many individually-paying
    /// decisions still add up to a prompt that is mostly protected content.
    ///
    /// Observed peaks are 17.9%, 26.9% and 37.5% of context as one, two and
    /// three messages were kept, so 40 has about two points of headroom and
    /// will bind on the next promotion. That is tighter than intended -- the
    /// economics cross nearer 55% of context at the scale measured -- and it is
    /// left at 40 only because the per-message guard, now derived rather than
    /// guessed, is the bound that should be doing this work. Raise it if this
    /// one starts refusing decisions the other one passed.
    pub keep_inline_max_percent: u32,
    pub stream_buffer_bytes: usize,
    /// How long the streamed relay may hold frames back while it decides
    /// whether a turn is its own. **Zero — off — and it should stay off.**
    ///
    /// It was added because the keepalive is an SSE comment and a client may
    /// ignore comments when judging a stream idle; one agent gave up after five
    /// minutes having been sent thirty of them. Flushing early would let it see
    /// real frames instead.
    ///
    /// The first benchmark run with it enabled at 45s failed outright, and the
    /// reasoning that justified it was wrong. It looked like the same trade the
    /// byte bound makes. It is not: the byte bound fires only after the model
    /// has produced 256 KiB, by which point the turn is plainly not a bare tool
    /// call, while this fires on a timer with *nothing* buffered — precisely the
    /// window in which a reasoning model is about to emit one. It did:
    ///
    /// ```text
    /// error=unsupported call: optscale_kompressor_retrieve
    /// Incomplete response returned, reason: max_output_tokens
    /// Invalid 'input[14].arguments': string too long ... got 1552051
    /// ```
    ///
    /// The flush handed the retrieve call to a client that never declared the
    /// tool; the model then ran to its output cap building a 1.5 MB argument,
    /// and the next request was rejected. One run, 52 minutes, task failed.
    ///
    /// The mechanism is kept, and tested, because the problem it was aimed at is
    /// real — but nothing should enable it without a way to keep a retrieve call
    /// from being flushed with the rest.
    #[serde(with = "humantime_serde")]
    pub stream_buffer_timeout: Duration,
    /// How much the relay may hold once a turn is known to be a retrieve turn.
    ///
    /// Unlike [`Self::stream_buffer_timeout`] this one ships on, because the
    /// hazard that kept that one off does not apply: reaching this bound drops
    /// our own call rather than flushing it to a client that cannot answer it.
    /// The cost of reaching it is one lost interception — the model asks again
    /// next turn — against a five-minute hang and a full retry, which is what
    /// no bound bought on two requests of one measured run.
    ///
    /// `0` disables it.
    pub stream_intercept_bytes: usize,
    /// How long interception may run. `0` disables it.
    ///
    /// Has to sit below the client's own first-byte timeout to be worth
    /// anything, since the relay sends nothing the client counts as activity
    /// while it holds. Measured: legitimate retrieve rounds took ~14 s, and the
    /// client gave up at exactly 300 s.
    #[serde(with = "humantime_serde")]
    pub stream_intercept_timeout: Duration,
    #[serde(with = "humantime_serde")]
    pub stream_keepalive: Duration,
    /// How much model *content* the relay may hold while still waiting to see
    /// whether a retrieve call follows it.
    ///
    /// A reasoning model often says a sentence before it calls a tool — "I'll
    /// look at the omitted section" — and the relay used to read that sentence
    /// as proof the turn was not ours, hand the buffer over and relay the rest
    /// untouched. The `optscale_kompressor_retrieve` calls that followed then reached a
    /// client that never declared the tool, which answered `unsupported call`
    /// and lost the fragment. Measured against codex on `gpt-5.5`: 13 calls
    /// leaked in one session and nothing was fetched back, while the same
    /// commit on `gpt-6-sol` — which calls without narrating — retrieved 20.
    ///
    /// The cost is paid only where the benefit is possible: content is held
    /// solely on turns where the retrieve tool was actually injected, so a
    /// turn that could not retrieve streams as immediately as it ever did.
    /// Within such a turn, the first request's narration is discarded anyway
    /// when a retrieve round runs, so holding it costs nothing unless no call
    /// arrives — and then it costs this many bytes of delay, once.
    ///
    /// `0` restores the old behaviour: give up on the turn at the first byte
    /// of content.
    pub stream_content_grace_bytes: usize,
    /// Ask for usage on streaming completions so savings can be measured
    /// against what the provider actually billed.
    pub force_usage: bool,
    pub stale_read: bool,
    /// Relay a websocket upgrade to the upstream instead of letting it fall
    /// through. **On.**
    ///
    /// Codex opens `/v1/responses` over a websocket first. Off, the upgrade
    /// reaches the forward path as a plain GET, the upstream answers 405, and
    /// codex falls back to HTTPS after about eight seconds — where this proxy
    /// reads the body and compresses it. On, the upgrade is relayed and codex
    /// keeps the transport it asked for.
    ///
    /// # The table that used to justify shipping this off
    ///
    /// ```text
    ///            405s  fallbacks  relays  compression events  tokens saved
    ///   off        21          1       0                  23     1,768,634
    ///   on          0          0       1                   0             0
    /// ```
    ///
    /// Read that as history. Those zeroes are a property of the *first* relay,
    /// which only forwarded; this one compresses. `websocket::rewrite_frame`
    /// puts a `response.create` through the same engine the HTTP path uses —
    /// that frame shape carries a whole request in one JSON message — and
    /// relays everything else byte for byte. The measurement was taken before
    /// that existed and was never re-run after it landed, which is the one
    /// number missing from this comment.
    ///
    /// # What relaying still costs
    ///
    /// Two things, both structural rather than unfinished work.
    ///
    /// **No provider usage.** The relay never reads a `response.completed`, so
    /// `prompt_tokens`, `cache_read_tokens` and the rest are filed null rather
    /// than zero. Traffic on this transport is compressed and *unmeasured*:
    /// cost reports and the Harbor token table have nothing to say about it,
    /// which is why a relayed sweep is read on frame counts instead.
    ///
    /// **No retrieve loop.** The tool is deliberately not offered here — a
    /// capture of a live codex session says why, and DESIGN §12.5b carries it —
    /// so an elided fragment comes back through `piggyback`, on the *next*
    /// frame rather than inside the turn.
    ///
    /// # Why it is on
    ///
    /// Because the fallback was never ours. With this off, every token saved on
    /// the codex path depends on a client choosing to retry over HTTPS after
    /// its own transport fails; a build that drops that retry, or ships with it
    /// disabled, reaches the provider over a socket an off relay does not touch
    /// — nothing compressed, every health check green, no error anywhere.
    /// On, that case is compressed, and the cost is that its tokens are not
    /// counted.
    ///
    /// Turning it off is still a supported, one-variable decision for an
    /// operator who would rather have the numbers:
    /// `KOMPRESSOR__PROXY__WEBSOCKET_RELAY=false`.
    ///
    /// # It does not touch the HTTP path
    ///
    /// This gates exactly one branch, behind `websocket::is_upgrade`, which
    /// needs both `Connection: upgrade` and `Upgrade: websocket`. An ordinary
    /// request matches neither and is routed, compressed, measured and offered
    /// the retrieve loop exactly as before. The two transports run side by
    /// side; enabling one has never disabled the other.
    pub websocket_relay: bool,
    /// Age past which a provider-cached prefix is assumed lapsed. **Zero — off
    /// — and that is the shipped default.**
    ///
    /// The proxy already has a signal that is evidence rather than inference:
    /// the provider telling it, for this exact prefix, that it read nothing
    /// from cache. That one needs no configuration and is always on. This is
    /// the other half — assuming a prefix went stale because it has not been
    /// used for a while — and it is a guess about someone else's eviction
    /// policy.
    ///
    /// The two mistakes do not cost the same. Wrongly cold rewrites a live
    /// prefix and the provider re-reads everything after the first changed
    /// byte at full price; on the workloads measured here 90–100% of the
    /// billed prompt is cache reads, so that turns the cheapest part of a
    /// request into the most expensive. Wrongly warm leaves a saving
    /// unclaimed and breaks nothing. Hence off unless an operator knows their
    /// provider's TTL and wants it.
    ///
    /// Anthropic's default is five minutes and its extended one an hour; set
    /// this *below* whichever applies, never above.
    #[serde(with = "humantime_serde")]
    pub prefix_cache_ttl: Duration,
    pub upstreams: Vec<Upstream>,
    pub default_upstream: String,
    pub allow_insecure_upstream: bool,
    /// See [`InterceptConfig`]. Off unless its `listen` is set.
    pub intercept: InterceptConfig,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        ProxyConfig {
            enabled: true,
            compress: true,
            min_tokens: 512,
            compress_timeout: Duration::from_secs(2),
            max_body_bytes: 64 * 1024 * 1024,
            upstream_connect_timeout: Duration::from_secs(10),
            upstream_read_timeout: Duration::from_secs(600),
            retrieve_tool: RetrieveTool::Auto,
            frozen_prefix: FrozenPrefix::Marker,
            retrieve_execution: RetrieveExecution::Server,
            retrieve_max_rounds: 3,
            retrieve_max_tokens_per_request: 16_384,
            keep_inline_max_percent: 40,
            stream_buffer_bytes: 256 * 1024,
            stream_buffer_timeout: Duration::ZERO,
            stream_intercept_bytes: 1024 * 1024,
            stream_intercept_timeout: Duration::from_secs(90),
            stream_keepalive: Duration::from_secs(10),
            stream_content_grace_bytes: 8192,
            force_usage: true,
            stale_read: true,
            // Off: on this costs every token of compression on the codex path.
            websocket_relay: true,
            // Off: an age rule is a guess, and guessing cold is the expensive
            // mistake. The observed-zero-cache-read signal needs no config.
            prefix_cache_ttl: Duration::ZERO,
            upstreams: Vec::new(),
            default_upstream: String::new(),
            allow_insecure_upstream: false,
            intercept: InterceptConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Upstream {
    pub name: String,
    pub path_prefix: String,
    pub strip_prefix: bool,
    pub base_url: String,
    /// `auto` (default), `openai`, `anthropic`, or `passthrough`.
    ///
    /// Except for `passthrough`, this does not change routing. The wire family is derived
    /// from the path the upstream will see, by `kompressor_wire::detect_family`,
    /// which already resolves it through a mount prefix — `/openai/v1/messages`
    /// is recognised as Anthropic Messages whatever this field says. It is kept
    /// because a config file is also documentation, and a mount whose family is
    /// written down is easier to read than one where it must be inferred from a
    /// path. Making it an override is tracked in the README's feature list.
    pub api_family: String,
    /// Forward the client's own credentials rather than substituting ours.
    pub forward_auth: bool,
    pub headers: BTreeMap<String, String>,
    /// Where this mount's traffic goes when the caller pays by subscription.
    ///
    /// Routing is otherwise decided by path alone, which is the rule a client
    /// can reason about from the outside. Subscription traffic breaks that
    /// rule at the provider, not here: Codex sends the same Responses body to
    /// `api.openai.com/v1/responses` under an API key and to
    /// `chatgpt.com/backend-api/codex/responses` under a ChatGPT plan, and the
    /// only thing distinguishing them is the credential. A proxy that forwards
    /// by path alone therefore sends half its callers somewhere that 404s —
    /// and Codex reads a 404 on that surface as an invalid session and stops
    /// using the subscription at all.
    ///
    /// `None` keeps the path-only rule, so a mount that does not set this
    /// behaves exactly as it did before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription: Option<SubscriptionUpstream>,
}

/// The base URL and prefix an upstream uses for subscription callers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SubscriptionUpstream {
    /// The host subscription traffic belongs to, e.g. `https://chatgpt.com`.
    pub base_url: String,
    /// What the mount's `path_prefix` becomes upstream. `/v1/responses` under a
    /// mount prefixed `/v1` with `rewrite_prefix = "/backend-api/codex"`
    /// becomes `/backend-api/codex/responses`.
    ///
    /// Empty leaves the path alone, which is right for a provider that changes
    /// only the host.
    pub rewrite_prefix: String,
}

impl Default for Upstream {
    fn default() -> Self {
        Upstream {
            name: String::new(),
            path_prefix: String::new(),
            strip_prefix: false,
            base_url: String::new(),
            api_family: "auto".to_string(),
            forward_auth: true,
            headers: BTreeMap::new(),
            subscription: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    pub level: String,
    pub format: String,
    /// Log full payloads and retrieved fragments at debug level.
    ///
    /// Off by default because a fragment is tool output and can hold anything
    /// the command printed, including credentials.
    pub payloads: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig {
            level: "info".to_string(),
            format: "json".to_string(),
            payloads: false,
        }
    }
}

impl Config {
    /// Read a TOML file, then overlay the environment.
    /// The configuration file to read when none was named.
    ///
    /// Searched in the order a user would expect to be able to override:
    /// `$KOMPRESSOR_CONFIG` names one explicitly, a `kompressor.toml` in the
    /// working directory belongs to the project, and the XDG path is the
    /// per-user default. The first that exists wins; `$KOMPRESSOR_CONFIG` is
    /// returned even when it does not, so a typo in it is reported rather than
    /// silently falling through to a different file.
    pub fn discover() -> Option<std::path::PathBuf> {
        use std::path::PathBuf;
        if let Some(explicit) = std::env::var_os("KOMPRESSOR_CONFIG")
            && !explicit.is_empty()
        {
            return Some(PathBuf::from(explicit));
        }
        let local = PathBuf::from("kompressor.toml");
        if local.is_file() {
            return Some(local);
        }
        if let Some(user) = Self::user_config_in(|name| std::env::var_os(name))
            && user.is_file()
        {
            return Some(user);
        }
        None
    }

    /// The per-user configuration file for a given environment: under
    /// `$XDG_CONFIG_HOME`, else `~/.config`.
    ///
    /// `~` is `HOME` on Unix and the user profile on Windows. PowerShell and cmd
    /// do not set `HOME` at all, and reading only that meant the per-user file
    /// was never found there: every command silently ran on the defaults —
    /// relative `./data` stores in whatever directory it was started from —
    /// while the same command in Git Bash, which does set `HOME`, read the
    /// file. `~/.config` rather than `%APPDATA%` because it is the path every
    /// document here names, and the macOS install already chose it over the
    /// platform's own convention.
    ///
    /// An empty variable counts as unset, as the XDG specification says: an
    /// empty `XDG_CONFIG_HOME` used to resolve to a *relative* `kompressor/`.
    fn user_config_in(
        env: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> Option<std::path::PathBuf> {
        let set = |name: &str| {
            env(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        let home = || {
            if cfg!(windows) {
                set("USERPROFILE").or_else(|| set("HOME"))
            } else {
                set("HOME")
            }
        };
        set("XDG_CONFIG_HOME")
            .or_else(|| home().map(|home| home.join(".config")))
            .map(|base| base.join("kompressor").join("config.toml"))
    }

    /// Where a package installs the system configuration: `/etc/kompressor` on
    /// Linux, `%ProgramData%\kompressor` for the Windows service.
    ///
    /// Deliberately **not** part of [`Self::discover`]. Reading it there would
    /// make `kompressor compress` on a developer machine behave differently
    /// depending on whether a package happened to be installed — measured as
    /// six hermetic CLI tests changing their output the moment this file
    /// existed. Commands that are *about the running service* opt in instead;
    /// `kompressor stats` is the one that does.
    pub fn system_path() -> Option<std::path::PathBuf> {
        let system = Self::system_path_in(|name| std::env::var_os(name))?;
        system.is_file().then_some(system)
    }

    /// [`Self::system_path`] for a given environment, before the existence check.
    fn system_path_in(
        env: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> Option<std::path::PathBuf> {
        if cfg!(windows) {
            // Not `/etc/...`: without a drive letter that is `C:\etc`, which
            // any local user may create.
            env("ProgramData")
                .filter(|value| !value.is_empty())
                .map(|base| PathBuf::from(base).join("kompressor").join("config.toml"))
        } else {
            Some(PathBuf::from("/etc/kompressor/config.toml"))
        }
    }

    pub fn load(path: Option<&std::path::Path>) -> Result<Config, ConfigError> {
        let discovered;
        let path = match path {
            Some(path) => Some(path),
            None => {
                discovered = Config::discover();
                discovered.as_deref()
            }
        };
        let mut config = match path {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| ConfigError::Read(path.display().to_string(), e))?;
                toml::from_str(&text).map_err(|e| ConfigError::Parse(e.to_string()))?
            }
            None => Config::default(),
        };
        config.apply_env(&std::env::vars().collect::<Vec<_>>());
        config.validate()?;
        Ok(config)
    }

    /// Overlay environment variables. Split out from [`Config::load`] so it can
    /// be tested without touching the process environment.
    pub fn apply_env(&mut self, vars: &[(String, String)]) {
        for (key, value) in vars {
            if value.is_empty() {
                continue;
            }
            let Some(rest) = key.strip_prefix("KOMPRESSOR_") else {
                continue;
            };
            // the structured form wins where both spellings exist
            if let Some(rest) = rest.strip_prefix('_') {
                self.apply_structured(rest, value);
            } else {
                self.apply_flat(rest, value);
            }
        }
    }

    fn apply_structured(&mut self, key: &str, value: &str) {
        let lower = key.to_ascii_lowercase();
        let mut parts = lower.splitn(2, "__");
        let (Some(section), Some(field)) = (parts.next(), parts.next()) else {
            return;
        };
        match section {
            "server" => match field {
                "listen" => self.server.listen = value.to_string(),
                "api_listen" => self.server.api_listen = value.to_string(),
                "mcp_listen" => self.server.mcp_listen = value.to_string(),
                "proxy_listen" => self.server.proxy_listen = value.to_string(),
                _ => {}
            },
            "auth" => match field {
                "mode" => {
                    if value.eq_ignore_ascii_case("api_keys") {
                        self.auth.mode = AuthMode::ApiKeys;
                    }
                }
                "secret" => self.auth.secret = value.to_string(),
                _ => {}
            },
            "refs" => match field {
                "path" => self.refs.path = PathBuf::from(value),
                "backend" if value.eq_ignore_ascii_case("memory") => {
                    self.refs.backend = RefsBackend::Memory;
                }
                _ => {}
            },
            "stats" => match field {
                "path" => self.stats.path = PathBuf::from(value),
                "enabled" => {
                    self.stats.enabled =
                        parse_bool_setting("stats.enabled", value, self.stats.enabled)
                }
                _ => {}
            },
            "log" => match field {
                "level" => self.log.level = value.to_string(),
                "format" => self.log.format = value.to_string(),
                "payloads" => {
                    self.log.payloads = parse_bool_setting("log.payloads", value, self.log.payloads)
                }
                _ => {}
            },
            "engine" => self.apply_engine_field(field, value),
            "proxy" => self.apply_proxy_field(field, value),
            _ => {}
        }
    }

    /// The flat `KOMPRESSOR_<FIELD>` names, accepted alongside the sectioned
    /// `KOMPRESSOR__SECTION__KEY` ones.
    fn apply_flat(&mut self, key: &str, value: &str) {
        let field = key.to_ascii_lowercase();
        match field.as_str() {
            // these two belonged to the proxy plugin rather than the engine
            "min_tokens" => {
                if let Ok(v) = value.parse() {
                    self.proxy.min_tokens = v;
                }
            }
            "timeout" => {
                if let Some(d) = parse_duration(value) {
                    self.proxy.compress_timeout = d;
                }
            }
            "enabled" => {
                // the kill switch: off means "forward unchanged"
                if parse_bool(value) == Some(false) {
                    self.proxy.enabled = false;
                }
            }
            "redis_url" => {
                tracing::warn!(
                    "KOMPRESSOR_REDIS_URL is set but this build stores fragments in sqlite; \
                     set refs.path instead"
                );
            }
            "log_compressed" => {
                // deprecated, and superseded by log.payloads
                self.log.payloads = parse_bool_setting("log.payloads", value, self.log.payloads);
                tracing::warn!("KOMPRESSOR_LOG_COMPRESSED is deprecated; use log.payloads");
            }
            "mcp_port" => {
                if value.parse::<u16>().is_ok() {
                    self.server.mcp_listen = format!("0.0.0.0:{value}");
                }
            }
            other => self.apply_engine_field(other, value),
        }
    }

    fn apply_engine_field(&mut self, field: &str, value: &str) {
        let e = &mut self.engine;
        macro_rules! num {
            ($target:expr) => {
                if let Ok(v) = value.parse() {
                    $target = v;
                }
            };
        }
        macro_rules! flag {
            ($target:expr) => {{
                $target = parse_bool_setting(field, value, $target);
            }};
        }
        match field {
            "mode" => {
                if let Ok(v) = value.parse() {
                    e.mode = v;
                }
            }
            "min_compress_chars" => num!(e.min_compress_chars),
            "max_array_items" => num!(e.max_array_items),
            "max_field_chars" => num!(e.max_field_chars),
            "keep_tail_lines" => num!(e.keep_tail_lines),
            "ref_ttl_seconds" => num!(e.ref_ttl_seconds),
            "ref_max_lifetime_multiplier" => num!(e.ref_max_lifetime_multiplier),
            "max_ref_bytes" => num!(e.max_ref_bytes),
            "search_max_files" => num!(e.search_max_files),
            "diff_max_files" => num!(e.diff_max_files),
            "code_max_body_lines" => num!(e.code_max_body_lines),
            "min_nested_chars" => num!(e.min_nested_chars),
            "max_nested_depth" => num!(e.max_nested_depth),
            "enable_smart_crusher" => flag!(e.enable_smart_crusher),
            "enable_search_compressor" => flag!(e.enable_search_compressor),
            "enable_log_compressor" => flag!(e.enable_log_compressor),
            "enable_diff_compressor" => flag!(e.enable_diff_compressor),
            "enable_tabular_compressor" => flag!(e.enable_tabular_compressor),
            "enable_listing_compressor" => flag!(e.enable_listing_compressor),
            "enable_code_aware" => flag!(e.enable_code_aware),
            "enable_mixed_content" => flag!(e.enable_mixed_content),
            "enable_config_sections" => flag!(e.enable_config_sections),
            "enable_config_compressor" => flag!(e.enable_config_compressor),
            "enable_nested_routing" => flag!(e.enable_nested_routing),
            "enable_redraw_fold" => flag!(e.enable_redraw_fold),
            "enable_build_templates" => flag!(e.enable_build_templates),
            "enable_gutter_aware_detection" => flag!(e.enable_gutter_aware_detection),
            "enable_lossy_key_factoring" => flag!(e.enable_lossy_key_factoring),
            "enable_stale_read" => flag!(e.enable_stale_read),
            "enable_fold_fallback" => flag!(e.enable_fold_fallback),
            "enable_line_gutter" => flag!(e.enable_line_gutter),
            // Added late and forgotten here, which is the failure mode this
            // list has: an option that exists in `EngineOptions` and cannot be
            // set from the environment is one no benchmark arm can turn off.
            "enable_cross_turn_dedup" => flag!(e.enable_cross_turn_dedup),
            "enable_html_compressor" => flag!(e.enable_html_compressor),
            "enable_normalised_text_fold" => flag!(e.enable_normalised_text_fold),
            "enable_tabular_ratio" => flag!(e.enable_tabular_ratio),
            "enable_code_budget" => flag!(e.enable_code_budget),
            "max_line_chars" => num!(e.max_line_chars),
            "enable_preamble_aware_detection" => flag!(e.enable_preamble_aware_detection),
            "strict_lossless" => flag!(e.strict_lossless),
            "protected_roles" => {
                e.protected_roles = value
                    .split(',')
                    .map(|r| r.trim().to_ascii_lowercase())
                    .filter(|r| !r.is_empty())
                    .collect();
            }
            _ => {}
        }
    }

    fn apply_proxy_field(&mut self, field: &str, value: &str) {
        match field {
            "min_tokens" => {
                if let Ok(v) = value.parse() {
                    self.proxy.min_tokens = v;
                }
            }
            "retrieve_max_rounds" => {
                if let Ok(v) = value.parse() {
                    self.proxy.retrieve_max_rounds = v;
                }
            }
            "retrieve_execution" => {
                self.proxy.retrieve_execution = if value.eq_ignore_ascii_case("client") {
                    RetrieveExecution::Client
                } else {
                    RetrieveExecution::Server
                };
            }
            "retrieve_tool" => {
                self.proxy.retrieve_tool = match value.to_ascii_lowercase().as_str() {
                    "always" => RetrieveTool::Always,
                    "never" => RetrieveTool::Never,
                    "auto" => RetrieveTool::Auto,
                    _ => self.proxy.retrieve_tool,
                };
            }
            "frozen_prefix" => {
                // Unrecognised values leave the default standing rather than
                // guessing. This is the setting that decides what the provider
                // is billed for, so a typo must not quietly turn it on.
                self.proxy.frozen_prefix = if value.eq_ignore_ascii_case("first_seen") {
                    FrozenPrefix::FirstSeen
                } else if value.eq_ignore_ascii_case("marker") {
                    FrozenPrefix::Marker
                } else {
                    self.proxy.frozen_prefix
                };
            }
            "enabled" => {
                self.proxy.enabled = parse_bool_setting("proxy.enabled", value, self.proxy.enabled);
            }
            "compress" => {
                self.proxy.compress =
                    parse_bool_setting("proxy.compress", value, self.proxy.compress);
            }
            // Reachable from the environment on purpose: the situation it is
            // for — a client that stopped falling back to HTTPS — is one where
            // every wrapped session is silently uncompressed, and waiting for a
            // config-file edit to reach every deployment is the wrong shape of
            // fix for that.
            "websocket_relay" => {
                self.proxy.websocket_relay =
                    parse_bool_setting("proxy.websocket_relay", value, self.proxy.websocket_relay);
            }
            // A field on `ProxyConfig` is not reachable from `KOMPRESSOR__*`
            // until it is added above, and the arm that used to be here
            // swallowed the difference. Twice, measurably: a run meant to test
            // `frozen_prefix` came back as a control, and the command meant to
            // set `retrieve_tool` would have left it on `auto`. A setting that
            // is accepted and ignored is worse than one that is rejected.
            other => tracing::warn!(
                key = other,
                value,
                "KOMPRESSOR__PROXY__* names a setting this build does not read; \
                 it has been ignored"
            ),
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.refs.purge_interval.is_zero() {
            return Err(ConfigError::Invalid(
                "refs.purge_interval must be greater than zero".into(),
            ));
        }
        self.engine
            .validate()
            .map_err(|e| ConfigError::Invalid(format!("engine: {e}")))?;
        if self.auth.mode == AuthMode::ApiKeys && self.auth.keys.is_empty() {
            return Err(ConfigError::Invalid(
                "auth.mode is api_keys but no keys are configured, so nothing could authenticate"
                    .into(),
            ));
        }
        for key in &self.auth.keys {
            if key.key.is_empty() {
                return Err(ConfigError::Invalid(
                    "an auth key has an empty value".into(),
                ));
            }
        }
        for upstream in &self.proxy.upstreams {
            validate_upstream_url(
                &upstream.base_url,
                &upstream.name,
                self.proxy.allow_insecure_upstream,
            )?;
            if let Some(subscription) = &upstream.subscription {
                validate_upstream_url(
                    &subscription.base_url,
                    &format!("{} subscription", upstream.name),
                    self.proxy.allow_insecure_upstream,
                )?;
            }
            if !matches!(
                upstream.api_family.to_ascii_lowercase().as_str(),
                "auto" | "openai" | "anthropic" | "passthrough"
            ) {
                return Err(ConfigError::Invalid(format!(
                    "upstream {:?} has unknown api_family {:?}",
                    upstream.name, upstream.api_family
                )));
            }
        }
        if self.retrieve.max_retrieve_chars == 0 {
            return Err(ConfigError::Invalid(
                "retrieve.max_retrieve_chars must be positive".into(),
            ));
        }
        Ok(())
    }

    /// The config as TOML, with secrets replaced.
    pub fn redacted_toml(&self) -> String {
        let mut copy = self.clone();
        if !copy.auth.secret.is_empty() {
            copy.auth.secret = "<redacted>".to_string();
        }
        for key in &mut copy.auth.keys {
            key.key = "<redacted>".to_string();
        }
        for upstream in &mut copy.proxy.upstreams {
            for value in upstream.headers.values_mut() {
                *value = "<redacted>".to_string();
            }
        }
        toml::to_string_pretty(&copy).unwrap_or_else(|e| format!("# could not render config: {e}"))
    }
}

fn validate_upstream_url(value: &str, name: &str, allow_insecure: bool) -> Result<(), ConfigError> {
    let url = url::Url::parse(value).map_err(|error| {
        ConfigError::Invalid(format!(
            "upstream {name:?} has invalid base_url {value:?}: {error}"
        ))
    })?;
    let loopback = match url.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    match url.scheme() {
        "https" => Ok(()),
        "http" if allow_insecure || loopback => Ok(()),
        "http" => Err(ConfigError::Invalid(format!(
            "upstream {name:?} is plaintext ({value}); set proxy.allow_insecure_upstream if that is deliberate"
        ))),
        _ => Err(ConfigError::Invalid(format!(
            "upstream {name:?} must use http or https: {value}"
        ))),
    }
}

/// A boolean from the environment, saying so when it is not one.
///
/// The silent version of this is a trap this project has already paid for: a
/// setting accepted and quietly ignored cost four Harbor runs that were read as
/// tests of a feature which was never on. An unknown *key* has warned since
/// then; an unparseable *value* for a known key did not, so
/// `KOMPRESSOR__PROXY__WEBSOCKET_RELAY=enabled` left the relay off and said
/// nothing — and the run looked exactly like one where the relay was on and
/// simply never triggered.
///
/// Keeping the previous value is still the right behaviour. Refusing to start
/// over a malformed override would turn a typo into an outage, which is the
/// opposite trade from the one the proxy makes everywhere else. The fix is that
/// it is now audible.
fn parse_bool_setting(key: &str, value: &str, current: bool) -> bool {
    match parse_bool(value) {
        Some(parsed) => parsed,
        None => {
            tracing::warn!(
                key,
                value,
                keeping = current,
                "not a boolean; expected one of 1/true/yes/on or 0/false/no/off. \
                 The setting was ignored, not applied"
            );
            current
        }
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn parse_duration(value: &str) -> Option<Duration> {
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    humantime_serde::re::humantime::parse_duration(value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_purge_interval_is_rejected() {
        let mut config = Config::default();
        config.refs.purge_interval = Duration::ZERO;
        assert!(config.validate().is_err());
    }

    #[test]
    fn the_retrieve_tool_can_be_set_from_the_environment() {
        // Needed to test `retrieve_tool = "always"` against the cache break the
        // `auto` injection causes: the tool appears in `tools` the turn the
        // first handle is minted, and `tools` precedes every cached message.
        // The override was accepted and ignored before this arm existed.
        let mut config = Config::default();
        assert_eq!(config.proxy.retrieve_tool, RetrieveTool::Auto);
        config.apply_env(&[("KOMPRESSOR__PROXY__RETRIEVE_TOOL".into(), "always".into())]);
        assert_eq!(config.proxy.retrieve_tool, RetrieveTool::Always);
        config.apply_env(&[("KOMPRESSOR__PROXY__RETRIEVE_TOOL".into(), "never".into())]);
        assert_eq!(config.proxy.retrieve_tool, RetrieveTool::Never);
        config.apply_env(&[("KOMPRESSOR__PROXY__RETRIEVE_TOOL".into(), "nonsense".into())]);
        assert_eq!(
            config.proxy.retrieve_tool,
            RetrieveTool::Never,
            "an unreadable value leaves the setting standing"
        );
    }

    #[test]
    fn an_unknown_proxy_setting_changes_nothing() {
        // The trap, asserted. `_ => {}` swallowed two real mistakes in one
        // session; the arm now warns, and this pins the other half — a name
        // this build does not read must not quietly alter something adjacent.
        let mut config = Config::default();
        let before = (
            config.proxy.retrieve_tool,
            config.proxy.frozen_prefix,
            config.proxy.enabled,
            config.proxy.min_tokens,
        );
        config.apply_env(&[
            ("KOMPRESSOR__PROXY__RETRIEVE_TOOOL".into(), "always".into()),
            (
                "KOMPRESSOR__PROXY__FROZEN-PREFIX".into(),
                "first_seen".into(),
            ),
            ("KOMPRESSOR__PROXY__NOT_A_SETTING".into(), "1".into()),
        ]);
        assert_eq!(
            (
                config.proxy.retrieve_tool,
                config.proxy.frozen_prefix,
                config.proxy.enabled,
                config.proxy.min_tokens,
            ),
            before
        );
    }

    /// An environment holding exactly `vars`, so the lookup can be tested
    /// without `set_var`, which this crate's `forbid(unsafe_code)` rules out.
    fn env_of(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<std::ffi::OsString> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.into())
    }

    #[test]
    fn xdg_config_home_outranks_the_home_directory() {
        let env = env_of(&[
            ("XDG_CONFIG_HOME", "/xdg"),
            ("HOME", "/home/u"),
            ("USERPROFILE", "/profile"),
        ]);
        assert_eq!(
            Config::user_config_in(env),
            Some(PathBuf::from("/xdg/kompressor/config.toml"))
        );
    }

    /// An empty `XDG_CONFIG_HOME` is unset, not a relative `kompressor/`.
    #[test]
    fn an_empty_xdg_config_home_is_ignored() {
        let env = env_of(&[("XDG_CONFIG_HOME", ""), ("HOME", "/home/u")]);
        let found = Config::user_config_in(env).expect("a home to fall back on");
        assert!(found.is_absolute() || cfg!(windows), "{found:?}");
        assert!(found.ends_with(".config/kompressor/config.toml"));
    }

    /// PowerShell and cmd leave `HOME` unset. Reading only `HOME`, the
    /// per-user file was never found there.
    #[cfg(windows)]
    #[test]
    fn on_windows_the_user_config_is_under_the_profile() {
        let env = env_of(&[("USERPROFILE", r"C:\Users\u")]);
        assert_eq!(
            Config::user_config_in(env),
            Some(PathBuf::from(r"C:\Users\u\.config\kompressor\config.toml"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn on_unix_the_user_config_is_under_home() {
        let env = env_of(&[("HOME", "/home/u"), ("USERPROFILE", "/elsewhere")]);
        assert_eq!(
            Config::user_config_in(env),
            Some(PathBuf::from("/home/u/.config/kompressor/config.toml"))
        );
    }

    #[test]
    fn with_no_home_at_all_there_is_no_user_config() {
        assert_eq!(Config::user_config_in(env_of(&[])), None);
    }

    #[cfg(windows)]
    #[test]
    fn the_windows_system_config_is_under_program_data() {
        let env = env_of(&[("ProgramData", r"C:\ProgramData")]);
        assert_eq!(
            Config::system_path_in(env),
            Some(PathBuf::from(r"C:\ProgramData\kompressor\config.toml"))
        );
        assert_eq!(Config::system_path_in(env_of(&[])), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_unix_system_config_is_under_etc() {
        assert_eq!(
            Config::system_path_in(env_of(&[])),
            Some(PathBuf::from("/etc/kompressor/config.toml"))
        );
    }

    #[test]
    fn the_frozen_prefix_setting_can_be_set_from_the_environment() {
        // `apply_proxy_field` ignores keys it does not know, silently, so a
        // field added to the struct is not reachable from `KOMPRESSOR__*`
        // until it is added here too. This one was not: the override was
        // accepted and did nothing, and the run it was meant to configure came
        // back as a control that looked like a failed experiment.
        let mut config = Config::default();
        assert_eq!(config.proxy.frozen_prefix, FrozenPrefix::Marker);
        config.apply_env(&[(
            "KOMPRESSOR__PROXY__FROZEN_PREFIX".into(),
            "first_seen".into(),
        )]);
        assert_eq!(config.proxy.frozen_prefix, FrozenPrefix::FirstSeen);
        config.apply_env(&[("KOMPRESSOR__PROXY__FROZEN_PREFIX".into(), "marker".into())]);
        assert_eq!(config.proxy.frozen_prefix, FrozenPrefix::Marker);
    }

    #[test]
    fn an_unreadable_frozen_prefix_leaves_the_default_standing() {
        // This setting changes what the provider is billed for. A typo must
        // not turn it on, and must not turn it off either.
        let mut config = Config::default();
        config.apply_env(&[(
            "KOMPRESSOR__PROXY__FROZEN_PREFIX".into(),
            "first-seen".into(),
        )]);
        assert_eq!(config.proxy.frozen_prefix, FrozenPrefix::Marker);
        config.proxy.frozen_prefix = FrozenPrefix::FirstSeen;
        config.apply_env(&[("KOMPRESSOR__PROXY__FROZEN_PREFIX".into(), "nonsense".into())]);
        assert_eq!(config.proxy.frozen_prefix, FrozenPrefix::FirstSeen);
    }

    #[test]
    fn the_defaults_are_a_working_single_tenant_deployment() {
        let config = Config::default();
        assert!(config.validate().is_ok());
        assert_eq!(config.auth.mode, AuthMode::None);
        assert_eq!(config.engine.mode, kompressor_engine::Mode::Recoverable);
        assert!(!config.log.payloads, "fragments are not logged by default");
        assert_eq!(Upstream::default().api_family, "auto");
    }

    #[test]
    fn upstream_url_validation_checks_the_actual_host_and_subscription_target() {
        let mut config = Config::default();
        config.proxy.upstreams.push(Upstream {
            name: "provider".into(),
            base_url: "http://localhost.example.com".into(),
            ..Default::default()
        });
        assert!(config.validate().is_err());
        config.proxy.upstreams[0].base_url = "http://[::1]:9000".into();
        assert!(config.validate().is_ok());
        config.proxy.upstreams[0].subscription = Some(SubscriptionUpstream {
            base_url: "http://remote.example.com".into(),
            ..Default::default()
        });
        assert!(config.validate().is_err());
        config.proxy.upstreams[0]
            .subscription
            .as_mut()
            .unwrap()
            .base_url = "https://remote.example.com".into();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn the_structured_environment_form_overlays_a_file() {
        let mut config = Config::default();
        config.apply_env(&[
            ("KOMPRESSOR__ENGINE__MODE".into(), "aggressive".into()),
            ("KOMPRESSOR__SERVER__LISTEN".into(), "0.0.0.0:9000".into()),
            ("KOMPRESSOR__REFS__BACKEND".into(), "memory".into()),
        ]);
        assert_eq!(config.engine.mode, kompressor_engine::Mode::Aggressive);
        assert_eq!(config.server.listen, "0.0.0.0:9000");
        assert_eq!(config.refs.backend, RefsBackend::Memory);
    }

    /// One test rather than four, because `discover` reads process-global
    /// environment and a working directory: split across parallel tests they
    /// would race each other.

    #[test]
    fn the_flat_environment_names_still_work() {
        // an existing deployment's environment has to keep meaning what it meant
        let mut config = Config::default();
        config.apply_env(&[
            ("KOMPRESSOR_MODE".into(), "lossless".into()),
            ("KOMPRESSOR_MAX_ARRAY_ITEMS".into(), "25".into()),
            ("KOMPRESSOR_PROTECTED_ROLES".into(), "assistant,tool".into()),
            ("KOMPRESSOR_MIN_TOKENS".into(), "900".into()),
            ("KOMPRESSOR_TIMEOUT".into(), "5s".into()),
        ]);
        assert_eq!(config.engine.mode, kompressor_engine::Mode::Lossless);
        assert_eq!(config.engine.max_array_items, 25);
        assert_eq!(config.engine.protected_roles, vec!["assistant", "tool"]);
        assert_eq!(
            config.proxy.min_tokens, 900,
            "a plugin knob, not an engine one"
        );
        assert_eq!(config.proxy.compress_timeout, Duration::from_secs(5));
    }

    #[test]
    fn an_empty_environment_value_does_not_override() {
        let mut config = Config::default();
        let before = config.engine.max_array_items;
        config.apply_env(&[("KOMPRESSOR_MAX_ARRAY_ITEMS".into(), String::new())]);
        assert_eq!(config.engine.max_array_items, before);
    }

    #[test]
    fn api_key_auth_with_no_keys_is_refused() {
        let config = Config {
            auth: AuthConfig {
                mode: AuthMode::ApiKeys,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn a_plaintext_upstream_is_refused_unless_it_is_deliberate() {
        let mut config = Config::default();
        config.proxy.upstreams.push(Upstream {
            name: "remote".into(),
            base_url: "http://api.example.com".into(),
            ..Default::default()
        });
        assert!(config.validate().is_err(), "credentials would go in clear");

        config.proxy.allow_insecure_upstream = true;
        assert!(config.validate().is_ok());

        // a local model server is the case this exception exists for
        config.proxy.allow_insecure_upstream = false;
        config.proxy.upstreams[0].base_url = "http://127.0.0.1:11434".into();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn rendering_the_config_never_reveals_a_credential() {
        let mut config = Config::default();
        config.auth.secret = "cluster-secret".into();
        config.auth.mode = AuthMode::ApiKeys;
        config.auth.keys.push(ApiKey {
            key: "sk-real-key".into(),
            tenant: "team-a".into(),
            ..Default::default()
        });
        config.proxy.upstreams.push(Upstream {
            name: "openai".into(),
            base_url: "https://api.openai.com".into(),
            headers: [(
                "Authorization".to_string(),
                "Bearer sk-upstream".to_string(),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        });
        let rendered = config.redacted_toml();
        assert!(!rendered.contains("sk-real-key"));
        assert!(!rendered.contains("cluster-secret"));
        assert!(!rendered.contains("sk-upstream"));
        assert!(rendered.contains("team-a"), "non-secret fields survive");
    }

    #[test]
    fn a_toml_file_round_trips() {
        let config = Config::default();
        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.engine.mode, config.engine.mode);
        assert_eq!(
            parsed.proxy.retrieve_max_rounds,
            config.proxy.retrieve_max_rounds
        );
    }

    #[test]
    fn an_unknown_key_in_a_config_file_is_an_error_not_a_silent_typo() {
        let text = "[engine]\nmax_array_itemz = 5\n";
        assert!(toml::from_str::<Config>(text).is_err());
    }
}
