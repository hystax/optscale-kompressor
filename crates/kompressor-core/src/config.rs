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

use kompressor_engine::{EngineOptions, EngineOptionsPatch};
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
    pub maintenance: MaintenanceConfig,
    /// Hook mode: agents that call this binary from their own hook points
    /// instead of sending traffic through the proxy. Cursor is the one.
    pub hooks: HooksConfig,
    /// Environment overrides that named a value this build does not know, for
    /// the settings where carrying on regardless is not safe. Reported by
    /// [`Config::validate`]; never read from or written to a file.
    #[serde(skip)]
    pub refused_env: Vec<String>,
    /// Every `KOMPRESSOR_*` variable [`Config::apply_env`] was given, and
    /// whether it changed anything. Names only: a value can be a secret.
    /// Never read from or written to a file.
    #[serde(skip)]
    pub env: Vec<EnvOverride>,
    /// The file this configuration was read from, made absolute; `None` for
    /// the built-in defaults. So the Maintenance page can name it in the
    /// commands it shows. Never read from or written to a file.
    #[serde(skip)]
    pub source: Option<PathBuf>,
}

/// One `KOMPRESSOR_*` variable the process was started with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EnvOverride {
    /// The variable's name, as it was spelt.
    pub name: String,
    pub effect: EnvEffect,
}

/// What a `KOMPRESSOR_*` variable did to the configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvEffect {
    /// It changed a setting from what the file and the defaults gave.
    Applied,
    /// It changed nothing: the name is not a setting, the value could not be
    /// read, or the value is the one already in force. The three cannot be
    /// told apart here, and the first two are the ones worth a look.
    NoEffect,
    /// Not a setting: read somewhere else, for something else.
    Elsewhere,
}

/// `KOMPRESSOR_*` variables that are not settings and are read where they are
/// used: the file to load, `wrap`'s home, the capture files, the build's
/// commit, and the credentials `mcp serve` takes its tenant from.
const ENV_READ_ELSEWHERE: [&str; 8] = [
    "CONFIG",
    "HOME",
    "BODY_CAPTURE",
    "WS_CAPTURE",
    "GIT_COMMIT",
    "API_KEY",
    "SECRET",
    "TENANT",
];

/// Backups on a schedule, and what the Maintenance page may do over HTTP.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MaintenanceConfig {
    /// Serve `POST /v1/maintenance/backup`: a gzipped tar of every database
    /// this process keeps, whole.
    ///
    /// Off by default, for the reason `refs.browse` is and more: the archive
    /// holds every tenant's statistics and every stored fragment — tool
    /// output, which can be a `.env` read. Even when on, it is served only to
    /// a caller that may act for every tenant: anyone under
    /// `auth.mode = "none"`, where there is one tenant, and otherwise only a
    /// caller presenting `auth.secret`.
    ///
    /// `kompressor backup` on the host needs none of this: it reads the files
    /// directly, as whoever runs it.
    pub backup: bool,
    /// Serve the housekeeping actions: `POST
    /// /v1/maintenance/databases/{name}/check` (a full integrity check) and
    /// `…/checkpoint` (fold the write-ahead log into the database).
    ///
    /// Apart from `backup` because the grant is a different one: these read
    /// nothing out, but they are work on every tenant's database, so they are
    /// off by default and limited to the same callers. What the page *shows*
    /// about each database needs neither setting.
    pub actions: bool,
    /// Serve `POST /v1/maintenance/statistics/erase`: delete every statistic
    /// this deployment has recorded, for every tenant, rollups included. The
    /// Maintenance page's danger zone.
    ///
    /// Apart from `actions` because nothing those do loses anything, and this
    /// loses all of it: off by default, limited to the same callers, and the
    /// request must spell out what it is asking for. There is no undoing it
    /// short of `kompressor restore`.
    pub erase: bool,
    /// Where `serve` writes a backup on a schedule. Empty, the default, takes
    /// none. The directory is created if it is missing, readable by its
    /// owner alone.
    ///
    /// Under the shipped systemd unit only the state directory is writable:
    /// use a directory inside it (`/var/lib/kompressor/backups`), or add the
    /// path to the unit's `ReadWritePaths`. A backup on the disk it backs up
    /// survives a bad migration, not a dead disk — copy it off the host.
    pub backup_dir: PathBuf,
    /// How often. A backup is taken when the newest archive in `backup_dir`
    /// is older than this, so the schedule holds across restarts and a
    /// service that was down takes one when it comes back. Zero takes none.
    #[serde(with = "humantime_serde")]
    pub backup_every: Duration,
    /// How many archives to keep in `backup_dir`; older ones are removed
    /// after each scheduled backup. Zero keeps them all. Only files this
    /// names itself (`kompressor-backup-<UTC>.tar.gz`) are ever removed.
    pub backup_keep: u32,
    /// How old the last backup may be before the Health page warns
    /// (`last_backup`), and three times that before it is `bad`. Zero, the
    /// default, follows the schedule — twice `backup_every`, so one missed
    /// backup is a warning — and with no schedule says nothing: a deployment
    /// that has not asked for backups is not nagged about them. Set it to be
    /// held to an age whoever takes them.
    #[serde(with = "humantime_serde")]
    pub backup_max_age: Duration,
    /// How often `serve` gathers the query planner's statistics on each
    /// database (`ANALYZE`, sampled: quick on a large file) — what the page's
    /// Optimize does by hand. Due when the last optimize on record is older
    /// than this, so it holds across restarts. Zero takes none.
    #[serde(with = "humantime_serde")]
    pub optimize_every: Duration,
    /// An off-site copy of every archive, on S3-compatible object storage.
    pub s3: S3Config,
}

/// Where backup archives are copied to, off this host: any S3-compatible
/// object store (AWS S3, MinIO, Cloudflare R2, Backblaze B2, Wasabi, Ceph…).
///
/// Off by default. When on, `serve` uploads each scheduled archive (with its
/// `.sha256`) after writing it, and the Maintenance page can upload one on
/// demand. Nothing is ever downloaded or restored from here by the service:
/// fetch an archive with any S3 client and give it to `kompressor restore`.
///
/// The archive holds every tenant's statistics and fragments, so the bucket
/// should be private, and the credentials should be able to write (and, with
/// `keep`, delete) under `prefix` and nothing else.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct S3Config {
    /// Copy archives to the bucket below.
    pub enabled: bool,
    /// The service's URL, `https://minio.internal:9000`. Empty is AWS S3 in
    /// `region`.
    pub endpoint: String,
    /// The region requests are signed for. Most S3-compatible stores accept
    /// any value; some (R2) want `auto`.
    pub region: String,
    pub bucket: String,
    /// A key prefix ("folder") the archives are kept under.
    pub prefix: String,
    pub access_key_id: String,
    /// Replaced by `<redacted>` wherever the configuration is shown.
    pub secret_access_key: String,
    /// Address the bucket as `endpoint/bucket/key` rather than
    /// `bucket.endpoint/key`. What a store at your own address needs; AWS
    /// takes either.
    pub path_style: bool,
    /// How many archives to keep in the bucket; older ones (and their
    /// `.sha256`) are removed after each upload. Zero keeps them all, and is
    /// the default: a bucket's own lifecycle rules are the safer place to
    /// expire objects. Only names this writes itself are ever removed.
    pub keep: u32,
}

impl Default for S3Config {
    fn default() -> Self {
        S3Config {
            enabled: false,
            endpoint: String::new(),
            region: "us-east-1".to_string(),
            bucket: String::new(),
            prefix: "kompressor/".to_string(),
            access_key_id: String::new(),
            secret_access_key: String::new(),
            path_style: true,
            keep: 0,
        }
    }
}

impl std::fmt::Debug for S3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Config")
            .field("enabled", &self.enabled)
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field("path_style", &self.path_style)
            .field("keep", &self.keep)
            .finish()
    }
}

impl S3Config {
    /// Where the archives go, for people: `s3://bucket/prefix/`.
    pub fn location(&self) -> String {
        let prefix = self.prefix.trim().trim_matches('/');
        if prefix.is_empty() {
            format!("s3://{}/", self.bucket.trim())
        } else {
            format!("s3://{}/{prefix}/", self.bucket.trim())
        }
    }

    /// What is wrong with the settings, when something is. Only checked when
    /// `enabled`.
    pub fn problem(&self) -> Option<String> {
        if self.bucket.trim().is_empty() {
            return Some("maintenance.s3.bucket is empty".into());
        }
        if self.access_key_id.is_empty() || self.secret_access_key.is_empty() {
            return Some(
                "maintenance.s3.access_key_id and secret_access_key must both be set".into(),
            );
        }
        if self.region.trim().is_empty() {
            return Some("maintenance.s3.region is empty".into());
        }
        if !self.endpoint.is_empty() {
            match url::Url::parse(&self.endpoint) {
                Ok(url) if matches!(url.scheme(), "http" | "https") && url.host().is_some() => {}
                _ => {
                    return Some(format!(
                        "maintenance.s3.endpoint {:?} is not an http(s) URL",
                        self.endpoint
                    ));
                }
            }
        }
        None
    }
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        MaintenanceConfig {
            backup: false,
            actions: false,
            erase: false,
            backup_dir: PathBuf::new(),
            backup_every: Duration::from_secs(24 * 3_600),
            backup_keep: 7,
            backup_max_age: Duration::ZERO,
            optimize_every: Duration::from_secs(24 * 3_600),
            s3: S3Config::default(),
        }
    }
}

impl MaintenanceConfig {
    /// How old the last backup may be before it is a warning, when the
    /// deployment expects backups at all: `backup_max_age`, or failing that
    /// twice the scheduled interval.
    pub fn max_age(&self) -> Option<Duration> {
        if !self.backup_max_age.is_zero() {
            return Some(self.backup_max_age);
        }
        self.schedule().map(|(_, every)| every.saturating_mul(2))
    }

    /// The schedule, when there is one: a directory and an interval.
    pub fn schedule(&self) -> Option<(&std::path::Path, Duration)> {
        (!self.backup_dir.as_os_str().is_empty() && !self.backup_every.is_zero())
            .then_some((self.backup_dir.as_path(), self.backup_every))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: String,
    /// Per-surface listeners; empty means "share `listen`".
    pub api_listen: String,
    pub mcp_listen: String,
    pub proxy_listen: String,
    /// A listener of its own for the live statistics and cache analysis pages
    /// and their JSON, e.g.
    /// `127.0.0.1:8818`. Empty serves them only on the API listener, at
    /// `/v1/stats/live`; set, they are served here too, whatever surfaces the
    /// process runs, so a proxy-only deployment still has a dashboard.
    pub stats_listen: String,
    pub max_body_bytes: usize,
    /// 0 means twice the detected parallelism.
    ///
    /// Bounds engine work across **every surface in the process** — the HTTP
    /// API, the proxy and the MCP tool server share one semaphore on
    /// `AppCore::engine_slots`. Until the 2026-11-11 review it bounded the API
    /// alone: the proxy and MCP spawned blocking work directly, and a proxy
    /// timeout does not cancel the task it gave up on, so a burst of large
    /// histories could queue and run without limit after every caller had
    /// fallen open.
    ///
    /// Each surface keeps the permit inside its blocking task, so it stays
    /// taken until the work actually ends. A full engine is a fail-open on the
    /// proxy (counted as `kompressor_proxy_failopen_total{stage="admission"}`;
    /// the wait counts against `proxy.compress_timeout`), a 503 on the API and
    /// a tool error on MCP, the last two after `queue_timeout`.
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
            stats_listen: String::new(),
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
    /// A Redis server shared by several proxies; see [`RefsConfig::redis_url`].
    Redis,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RefsConfig {
    pub backend: RefsBackend,
    pub path: PathBuf,
    pub recent_cache_entries: usize,
    /// Fragment bytes the recent-write cache may hold, beside its entry cap.
    ///
    /// The entry cap alone bounds nothing in bytes: 4,096 entries at the
    /// default 1 MiB `max_ref_bytes` is 4 GiB. Whichever bound binds first
    /// evicts; a fragment larger than this is written through uncached.
    pub recent_cache_bytes: usize,
    #[serde(with = "humantime_serde")]
    pub purge_interval: Duration,
    pub max_bytes: u64,
    /// Serve the ref browser: `/v1/refs`, which lists, shows and deletes
    /// stored fragments for the caller's tenant.
    ///
    /// Off by default. Fragments are tool output — a `.env` read, a cloud
    /// API response — and until this is on, reading one back needs its
    /// unguessable handle. With it on, any credential for a tenant can list
    /// that tenant's fragments, and with `auth.mode = "none"` anyone who can
    /// reach the listener can.
    pub browse: bool,
    /// `redis://[user[:password]@]host[:port][/db]`, read when `backend =
    /// "redis"`. `rediss://` is refused. The password is redacted by
    /// `check-config`.
    pub redis_url: String,
    /// Prepended to every key, so deployments can share a server.
    pub redis_prefix: String,
    /// Pool acquisition, connect, read and write timeout for each Redis call.
    #[serde(with = "humantime_serde")]
    pub redis_timeout: Duration,
    /// Maximum Redis connections, including idle and connecting ones.
    /// Zero is treated as one.
    pub redis_pool: usize,
}

impl Default for RefsConfig {
    fn default() -> Self {
        RefsConfig {
            backend: RefsBackend::Sqlite,
            path: PathBuf::from("./data/refs.db"),
            recent_cache_entries: 4096,
            recent_cache_bytes: 64 * 1024 * 1024,
            purge_interval: Duration::from_secs(60),
            max_bytes: 1024 * 1024 * 1024,
            browse: false,
            redis_url: "redis://127.0.0.1:6379/0".to_string(),
            redis_prefix: "kompressor".to_string(),
            redis_timeout: Duration::from_secs(2),
            redis_pool: 8,
        }
    }
}

/// An MCP server this one fronts.
///
/// Proxy mode exists for clients that cannot be pointed at the HTTP proxy at
/// all — Claude Desktop's Chat tab among them. Their inference requests carry
/// no bulk worth eliding, because the conversation lives on the server; the
/// bulk they carry is **tool results**, which are machine-facing in exactly the
/// way tool output in a coding agent is. Fronting the servers that produce
/// those results compresses them without touching anything a person reads.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpServerConfig {
    /// Prefixes this server's tool names, so two servers exporting `read_file`
    /// do not shadow each other.
    pub name: String,
    /// Spawned, speaking JSON-RPC on its pipe. Exclusive with [`Self::url`].
    pub command: Option<String>,
    pub args: Vec<String>,
    /// Extra environment for a spawned server. The parent's is inherited,
    /// except `KOMPRESSOR_API_KEY`, `KOMPRESSOR_SECRET` and
    /// `KOMPRESSOR_TENANT`: this process's own credential reaches a fronted
    /// server only when named here.
    pub env: std::collections::BTreeMap<String, String>,
    /// Connected to over streamable HTTP. Exclusive with [`Self::command`].
    pub url: Option<String>,
    /// Whether results from this server are compressed at all.
    ///
    /// False forwards them untouched, which is right for a server whose results
    /// are already small or structurally fragile: excluding one is better than
    /// tuning the engine around it.
    pub compress: bool,
    /// How long the server has to answer `initialize` before it is killed and
    /// skipped.
    ///
    /// Servers are connected before the client is served, so one that starts
    /// and never answers would otherwise hold the whole session, our own tools
    /// included. Generous by default because `npx -y` downloads the package on
    /// its first run.
    #[serde(with = "humantime_serde")]
    pub startup_timeout: Duration,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        McpServerConfig {
            name: String::new(),
            command: None,
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            url: None,
            compress: true,
            startup_timeout: Duration::from_secs(60),
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
    /// Optional experiment and run labels for historical analysis.
    pub experiment_id: Option<String>,
    pub run_id: Option<String>,
    pub enabled: bool,
    pub path: PathBuf,
    pub queue_size: usize,
    #[serde(with = "humantime_serde")]
    pub flush_interval: Duration,
    pub retention_days: u32,
    /// Days a request keeps its copy of the effective settings
    /// (`request_context.context`, most of a busy `stats.db`) before it is
    /// emptied, the request kept. The analysis history is archived first, so
    /// no breakdown loses a request. Zero, the default, keeps it as long as
    /// the request; a value no shorter than `retention_days` changes nothing.
    pub context_retention_days: u32,
}

impl Default for StatsFileConfig {
    fn default() -> Self {
        StatsFileConfig {
            experiment_id: None,
            run_id: None,
            enabled: true,
            path: PathBuf::from("./data/stats.db"),
            queue_size: 16_384,
            flush_interval: Duration::from_millis(100),
            retention_days: 90,
            context_retention_days: 0,
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
    /// Servers this one fronts. Empty, the default, serves only our own tools.
    pub servers: Vec<McpServerConfig>,
    /// Empty means any host is accepted, which is right for an internal service.
    pub allowed_hosts: Vec<String>,
    pub max_retrievals_per_minute: u32,
}

impl Default for McpConfig {
    fn default() -> Self {
        McpConfig {
            enabled: true,
            path: "/mcp".to_string(),
            servers: Vec::new(),
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

/// How long Anthropic should keep the cache entries a request writes.
///
/// The client's `cache_control` markers ask for the provider's default, five
/// minutes. An agent that waits on a long build or test run outlives it, and
/// the next request writes the whole conversation again: on aset-check04 that
/// was 698,697 tokens over five turns, 63% of every cache write in the run.
/// A one-hour entry costs 2x the input price to write instead of 1.25x, so it
/// pays only where gaps actually exceed five minutes. Replayed over eight
/// Anthropic runs, upgrading every marker cost 2.3% more overall (up to 27%
/// on runs with no long gap); upgrading a conversation only after it first
/// idled past five minutes cost 7.7% less, and never more than 1.6% more.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CacheTtl {
    /// The client's markers go out as written. The shipped default.
    #[default]
    Default,
    /// Every marker asks for one hour.
    #[serde(rename = "1h")]
    OneHour,
    /// One hour, from the request that finds a conversation idle longer than
    /// the five-minute default. That request has lost its prefix anyway, so it
    /// writes it once at the one-hour price. Switching a request later costs
    /// the prefix twice: a request asking for an hour does not read the
    /// five-minute entry its predecessor wrote.
    Adaptive,
}

/// How long OpenAI should keep the prompt-cache entries a request writes.
///
/// OpenAI caches every prompt of 1,024 tokens or more without being asked and
/// bills nothing to write it. How long an entry lives is the request's
/// `prompt_cache_retention`: `in_memory`, typically five to ten minutes idle,
/// or `24h`, around 30 minutes and up to a day. Which one a request gets when
/// it names neither depends on the organization (zero-data-retention
/// organizations get `in_memory`), and Codex names neither.
///
/// Measured over 12,114 archived Codex requests on gpt-5.5 (2026-09-18 to
/// 10-31), turns that followed more than five minutes idle failed to read back
/// 46–76% of their cached prefix: the in-memory lifetime, not a day. Those
/// losses came to about 0.35% of the Codex bill, because Codex seldom idles,
/// so this is a small, free saving rather than the large one keep-warm is on
/// Anthropic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheRetention {
    /// The client's request goes out as written. The shipped default.
    #[default]
    Default,
    /// Ask for `24h` retention where the client named none, on the models
    /// OpenAI documents it for. Free to write on those models.
    #[serde(rename = "24h")]
    Extended,
}

/// What to do on the turn that finds a conversation's provider cache gone.
///
/// A request that arrives after the cache has lapsed is written to the cache
/// again at the write price whatever it contains, so it is the one turn on
/// which rewriting the whole conversation costs nothing: the frozen prefix can
/// be compressed, and a file read a later edit invalidated can be replaced.
/// What that turn decides is recorded per conversation and replayed on every
/// later turn, so the bytes it wrote are the bytes sent from then on.
///
/// Acts only under `frozen_prefix = "first_seen"`, which is what lets a
/// conversation's freeze be lowered for good; under `marker` it does nothing.
/// With it on, `first_seen` no longer withholds the stale-read replacement:
/// that is decided on these turns and on no other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ColdRecompact {
    /// A lapse is handled as it always was. The shipped default.
    #[default]
    Off,
    /// Recompact when the provider's last answer showed the cache gone: it
    /// read nothing and wrote nothing.
    Observed,
    /// Also when this request arrives longer after the conversation's last one
    /// than the cache lifetime that one asked for, plus
    /// `cold_recompact_margin`. Anthropic Messages only, where the lifetime is
    /// in the request. Three wrong predictions in an hour turn it off for the
    /// life of the process.
    Predicted,
}

/// The order tool definitions go upstream in.
///
/// A provider renders the tool list ahead of the conversation, so a client
/// that sends the same tools in another order on a later turn loses the cache
/// from the first tool that moved. `pinned` keeps each conversation's tools in
/// the order its first request sent them; a tool that appears later is added
/// after those. Nothing is sorted, so the first request of every conversation
/// goes out exactly as written, and a client that never reorders is never
/// touched. Built on the evidence of the `/prefix` dashboard's `tool_order`
/// row, and off until that row says it is worth having.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ToolsOrder {
    /// The client's order, as written. The shipped default.
    #[default]
    Client,
    /// The order the conversation's first request used.
    Pinned,
}

/// Whether OpenAI requests that name no `prompt_cache_key` are given one.
///
/// OpenAI routes a request to a cache by a hash of its opening, and combines
/// that with `prompt_cache_key` when the request carries one. A client that
/// sends none leaves two conversations with the same instructions and tools
/// competing for one route. `conversation` names the conversation by its
/// opening messages, so every turn of it is routed alike. Codex sets its own
/// key and is never overridden.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheKey {
    /// The client's request goes out as written. The shipped default.
    #[default]
    Client,
    /// Add a key derived from the conversation's opening where the client
    /// named none.
    Conversation,
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

/// What decides whether a compression that leaves a handle is worth the fetch
/// it invites. See `proxy.elision_gate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ElisionGate {
    /// `engine.min_elision_saving_chars`, the same bar for every payload. The
    /// shipped behaviour.
    #[default]
    Fixed,
    /// `proxy.net_cost_gate`: the saving priced against what a retrieve round
    /// costs on this conversation, decided once per message (§12.6).
    NetCost,
}

/// What a stability guard does when the body going upstream fails its check.
///
/// Each guard compares the rewritten body with the one the client sent, after
/// every rewrite and before it is sent, and trips only on a fault the rewrite
/// introduced: a body the client already sent broken is the client's, and
/// forwarding it as it came would not mend it. A trip is counted and logged in
/// every mode but `off`. See `kompressor_proxy::guard`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GuardMode {
    /// The check is not run.
    Off,
    /// A trip is counted and logged and the rewritten body still goes out.
    /// The shipped default, so a guard that is wrong cannot turn a request
    /// the provider would have accepted into a fail-open.
    #[default]
    Observe,
    /// A trip sends the client's original body instead, as any other
    /// fail-open does.
    Enforce,
}

impl GuardMode {
    /// Whether the check runs at all.
    pub fn checks(self) -> bool {
        self != GuardMode::Off
    }

    /// Parse an environment value; anything else is `None`.
    pub fn parse(value: &str) -> Option<GuardMode> {
        match value.to_ascii_lowercase().as_str() {
            "off" => Some(GuardMode::Off),
            "observe" => Some(GuardMode::Observe),
            "enforce" => Some(GuardMode::Enforce),
            _ => None,
        }
    }
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
    /// Check that every Anthropic `thinking` and `redacted_thinking` block,
    /// and every Responses `reasoning` item carrying `encrypted_content`, goes
    /// upstream byte-identical and in order to what the client sent. See
    /// [`GuardMode`]; **observe** by default.
    ///
    /// The provider verifies a thinking block's `signature` against its bytes
    /// and refuses the whole request with a 400 when they differ. The walker
    /// never offers those blocks to the engine, so no rewrite is meant to
    /// touch them; this is the assertion that none did, re-serialisation
    /// included. Headroom forwards a mutated body only behind the same
    /// fingerprint.
    pub guard_thinking: GuardMode,
    /// Check the Anthropic Messages body going upstream for three faults the
    /// provider answers with a 400: more than four `cache_control`
    /// breakpoints across tools, system and messages; a one-hour breakpoint
    /// after a five-minute one in that order; a `tool_result` whose
    /// `tool_use_id` names no `tool_use` in the assistant message before it.
    /// See [`GuardMode`]; **observe** by default.
    ///
    /// Only a fault the rewrite introduced trips it. Headroom hit the
    /// five-breakpoint 400 twice from its own rewrites.
    pub guard_upstream_body: GuardMode,
    #[serde(with = "humantime_serde")]
    pub upstream_connect_timeout: Duration,
    #[serde(with = "humantime_serde")]
    pub upstream_read_timeout: Duration,
    pub retrieve_tool: RetrieveTool,
    pub retrieve_execution: RetrieveExecution,
    /// Where the frozen prefix ends. See [`FrozenPrefix`].
    pub frozen_prefix: FrozenPrefix,
    /// The cache TTL Anthropic requests ask for. See [`CacheTtl`].
    pub cache_ttl: CacheTtl,
    /// The prompt-cache retention OpenAI requests ask for. See
    /// [`PromptCacheRetention`].
    ///
    /// Applied only to Chat Completions and Responses requests through the
    /// `openai` upstream (or any upstream on `api.openai.com`), never on its
    /// ChatGPT-subscription route, never to a request that names a retention
    /// of its own, and only for the models OpenAI documents `24h` for. A 400
    /// that names the field is answered by sending the request again without
    /// it, and the field is not added again for that upstream and model.
    pub prompt_cache_retention: PromptCacheRetention,
    /// Whether an OpenAI request that names no `prompt_cache_key` is given
    /// one. See [`PromptCacheKey`]. Applied where
    /// [`Self::prompt_cache_retention`] is: Chat Completions and Responses
    /// through the `openai` upstream or one on `api.openai.com`, never on the
    /// ChatGPT-subscription route, and a 400 naming the field is answered by
    /// sending again without it.
    pub prompt_cache_key: PromptCacheKey,
    /// The order tool definitions go upstream in. See [`ToolsOrder`].
    pub tools_order: ToolsOrder,
    /// The models whose earlier-turn reasoning is left out of the requests
    /// that re-send it. **Empty by default**, and nothing is elided for a model
    /// that is not named.
    ///
    /// Some providers return reasoning as plain text (`reasoning_content`, or
    /// a `<think>` span) and bill it as input when the client sends it back.
    /// For a model named here, an OpenAI Chat Completions request has the
    /// reasoning of every assistant message *before its last user message*
    /// replaced by a marker, with a handle in `recoverable` mode. A tool round
    /// in progress keeps all of its reasoning, which is when a provider needs
    /// it back.
    ///
    /// The step from one user turn to the next rewrites the turn just
    /// finished, and so costs its cached prefix once per user turn. Name a
    /// model only where its provider documents that earlier reasoning need not
    /// be sent, and where conversations run long enough to repay that. An
    /// entry ending in `*` matches by prefix.
    pub reasoning_elide_models: Vec<String>,
    /// Take out of each tool's schema what says nothing to a model: the
    /// dialect, comments, an unreferenced identifier, a `title` that only
    /// restates a property's name. **Off by default.**
    ///
    /// Nothing a model might act on is touched — descriptions, examples,
    /// defaults, enums and constraints stay as written. Decided per
    /// conversation when it is first seen, and never for one that was already
    /// under way: the tool block sits at the front of the cached prefix and
    /// must not change part-way. On Claude Code's own tools it removes about
    /// 2% of the tool block; it exists for clients whose tools come from a
    /// schema generator.
    pub tools_compact: bool,
    /// Mark the tools a conversation does not use every turn `defer_loading`
    /// and add Anthropic's tool-search tool, so their schemas are not
    /// rendered into the prompt until the model searches for one. **Off by
    /// default.**
    ///
    /// Anthropic Messages to the first-party API only (gateways, Bedrock and
    /// Vertex refuse or gate the shape). Decided on a conversation's first
    /// request and held for its life, like [`Self::tools_compact`]; never
    /// applied when the client already defers, or to fewer than
    /// [`Self::tools_defer_min_tools`] tools. The tools named in
    /// [`Self::tools_defer_keep`] stay in the prompt. It changes what the
    /// model sees: a deferred tool takes one search before its first use.
    pub tools_defer: bool,
    /// The fewest tools a request must carry for `tools_defer` to act.
    pub tools_defer_min_tools: usize,
    /// Tools `tools_defer` never defers, compared lower-cased with leading
    /// underscores dropped: the ones an agent calls every turn, which would
    /// otherwise cost a search round each.
    pub tools_defer_keep: Vec<String>,
    /// Append [`Self::output_shaping_text`] to the instructions ahead of the
    /// conversation, as an experiment. **Off by default.** Anthropic Messages:
    /// the last `system` block. OpenAI Responses: the tail of `instructions`.
    /// OpenAI Chat: the leading `system`/`developer` message, or a `system`
    /// message put in front when there is none. Not applied on the websocket
    /// relay.
    ///
    /// The only setting that changes what the model is asked rather than how
    /// the request is carried. It goes after every `system` block, with no
    /// breakpoint of its own, so no cached byte before it moves; decided on a
    /// conversation's first request and held for its life (a conversation
    /// first seen part-way through is never treated), and
    /// [`Self::output_shaping_holdout_pct`] of conversations are held out so
    /// the statistics can compare the two arms. `max_tokens`, thinking and
    /// effort are left alone.
    pub output_shaping: bool,
    /// The share of conversations, in percent, that `output_shaping` holds
    /// out as the control arm. 30 to 100; 100 runs the experiment with nobody
    /// treated.
    pub output_shaping_holdout_pct: u8,
    /// The instruction `output_shaping` appends. Fixed per conversation from
    /// its first request, so changing it affects only conversations that
    /// start afterwards.
    pub output_shaping_text: String,
    /// Keep sending the `anthropic-beta` tokens a conversation has already
    /// sent. **Off by default.**
    ///
    /// A beta token can change how the provider renders the prompt, so a
    /// client that sends one on some turns of a conversation and not on others
    /// can lose its cache each time the set changes. With this on, a request
    /// carrying fewer tokens than the conversation has sent before goes out
    /// with all of them, in the order they were first seen. A token the client
    /// never sent in this conversation is never added. Anthropic Messages
    /// only.
    pub sticky_beta: bool,
    /// What to do on the turn that finds the provider's cache gone. See
    /// [`ColdRecompact`].
    pub cold_recompact: ColdRecompact,
    /// How far past the cache lifetime a request must arrive before
    /// `cold_recompact = "predicted"` calls the cache gone. A wrong call
    /// rewrites a live prefix, so the margin is what keeps a slow clock or a
    /// late response from being read as a lapse.
    #[serde(with = "humantime_serde")]
    pub cold_recompact_margin: Duration,
    /// On a recompacted turn, also replace a file read that a later read of
    /// the same file contains. **Off by default**, and does nothing unless
    /// [`Self::cold_recompact`] is on.
    ///
    /// An agent that reads lines 40 to 60 of a file and later reads the whole
    /// of it carries the twenty lines twice for the rest of the session. The
    /// earlier read is replaced by a marker that says the file was read again
    /// and keeps a handle to what it held. Decided only where the walker can
    /// tell which lines each read asked for — Anthropic Messages, for a read
    /// tool taking a path, an `offset` and a `limit`.
    pub superseded_reads: bool,
    /// On a recompacted turn, also compress as source a file read nothing has
    /// touched for [`Self::matured_quiesce_turns`] assistant turns. **Off by
    /// default**, and does nothing unless [`Self::cold_recompact`] is on.
    ///
    /// Function bodies go behind handles and signatures stay, whatever
    /// `enable_code_aware` says for the provider: a profile turns that off
    /// because a body elided from a file in use is fetched straight back, and
    /// a file the conversation has finished with is where it is not.
    pub matured_reads: bool,
    /// Assistant turns a file must go unread and unwritten before a read of it
    /// is matured. Five is where Headroom's audit of its own traffic put it
    /// (the next touch of a file came a median of four turns later); it has
    /// not been measured here.
    pub matured_quiesce_turns: u32,
    /// Assistant turns after which a read is matured however recently its file
    /// was touched again.
    pub matured_max_hold_turns: u32,
    /// The smallest read worth maturing, in bytes.
    pub matured_min_bytes: usize,
    /// Keep an Anthropic conversation's cached prefix alive while the client
    /// is busy running a tool. **On by default**; only Anthropic Messages
    /// traffic is ever pinged, so this is in effect Anthropic's default, and
    /// `false` here or under `[proxy.providers.anthropic]` turns it off.
    ///
    /// An agent that waits on a long build or test run outlives the provider's
    /// five-minute cache entry, and its next request writes the whole
    /// conversation again at 25x the price of reading it. A read refreshes the
    /// entry, so after a response that stopped on `tool_use` (or on `end_turn`
    /// with a background task running, under
    /// [`Self::cache_keep_warm_background_pings`]) the proxy re-sends
    /// that request every [`Self::cache_keep_warm_interval`] with `max_tokens`
    /// of 1 until the client's next request arrives, at most
    /// [`Self::cache_keep_warm_max_pings`] times. Each ping is billed as cache
    /// reads plus one output token, and is filed as its own `proxy_requests`
    /// row with `keep_warm = 1` so the cost is in the bill.
    ///
    /// Replayed over the request logs of seven 89-task Harbor runs on Claude
    /// Opus 5.5, it would have saved $1.84–$3.00 per run (5–9%) against what
    /// expiry and one-hour writes actually cost, net of $0.34–$0.58 of pings.
    /// Measured live on one such run: cache-write spend per agent step −20% and
    /// cache-loss turns 13 → 2, for 25 pings costing $0.15.
    ///
    /// Anthropic only. OpenAI does not bill cache writes on the models Codex
    /// runs, and Codex seldom idles past five minutes: on archived Codex runs
    /// the expiry a ping could prevent came to about 0.35% of the bill, less
    /// than the pings' output tokens would cost. There the lever is
    /// [`Self::prompt_cache_retention`]. Not armed for a
    /// request that asked for one-hour entries, which do not need it, nor for
    /// one whose `thinking` has a token budget, since `max_tokens` cannot go
    /// below the budget without changing the cache key.
    pub cache_keep_warm: bool,
    /// How long after the cached request started the first keep-warm ping goes
    /// out, and after each ping the next. Below the provider's five-minute TTL
    /// with room for a slow ping, which is why the shipped 270s is not 299s;
    /// zero and anything from five minutes up are rejected.
    #[serde(with = "humantime_serde")]
    pub cache_keep_warm_interval: Duration,
    /// The most pings one wait may send: 11 at 270s is about 50 minutes, which
    /// covers every pause in those seven runs bar one (4,924 s, of 192).
    pub cache_keep_warm_max_pings: u32,
    /// The most pings for a wait on a background task, and `0` to not keep
    /// those warm at all.
    ///
    /// Claude Code can run a command in the background and end its turn on
    /// `end_turn`; the model is woken by a notification when the job is done.
    /// Those waits are kept warm too, while the conversation has a background
    /// task it has started and not yet been told has finished. In the second
    /// keep-warm run they were 13 of 30 pauses past five minutes, every one a
    /// miss.
    ///
    /// This wait can be a dead end: an agent that gives its final answer with
    /// a job still running never sends another request, and 0–4 conversations
    /// a run did. That was the reason for a lower cap, four pings (about 20
    /// minutes), and it priced the wrong way round. A ping costs 0.1x the
    /// prefix; the wait it fails to bridge is re-written at 2x, because
    /// `cache_ttl = "adaptive"` moves the conversation to one-hour entries on
    /// that very request — so one bridged wait pays for about 19 pings, and a
    /// dead end at 11 costs at most seven more than one at 4. Measured on the
    /// 89-task run of 2026-10-30 (`14-37-11`): three background waits of 26,
    /// 43 and 60 minutes outran four pings and re-wrote 97,535 tokens at the
    /// one-hour price, about $0.76, where the pings to bridge them would have
    /// cost about $0.12; trailing pings after a conversation's last request
    /// came to $0.03–$0.11 a run across the three final runs. It ships at 4
    /// again (about 20 minutes) with keep-warm on by default, so a deployment
    /// that never chose the feature does not leave conversations that have
    /// ended pinging for 50 minutes; [`Self::cache_keep_warm_max_pings`] also
    /// caps it.
    pub cache_keep_warm_background_pings: u32,
    /// Stop pinging a background wait in a conversation whose earlier
    /// background waits did not come back. Off by default.
    ///
    /// Each background wait the proxy arms (or would have armed, had this
    /// held it back) is scored at the conversation's next request: it came
    /// back if that request arrived before its pings ran out or within one
    /// interval of the last, and did not otherwise. A later background wait
    /// gets no pings when at least two have been scored and the
    /// Laplace-smoothed share that came back, `(back + 1) / (scored + 2)`, is
    /// under a third;
    /// the first background wait of a conversation is always pinged as
    /// before. The rule and its constants are in `keepwarm::Prior`.
    ///
    /// Replayed over 20 days of interactive Claude Code traffic
    /// (`scripts/harbor/keepwarm-replay.py`, 2026-11-05 to 11-24) it changed
    /// nothing: no conversation had a second background wait after one that
    /// did not come back, and 1,254 of the 1,274 trailing pings the current
    /// rule sends follow a `tool_use`, which this does not touch. It is here
    /// for agent runs, where a dead-end background wait is the common case.
    pub cache_keep_warm_background_prior: bool,
    /// Send one ping [`Self::cache_keep_warm_one_hour_delay`] after a request
    /// whose every cache marker asks for an hour and that stopped on
    /// `tool_use`. Off by default.
    ///
    /// Such a request is otherwise not pinged at all: its entry outlives any
    /// pause eleven pings would cover. One read just before the hour is out
    /// keeps it for another, for the price of one read, against a rewrite at
    /// 2x if the client comes back after the hour. Replayed over the same 20
    /// days it would have sent two pings and bridged nothing: no `tool_use`
    /// wait on one-hour entries lasted past 55 minutes and came back.
    pub cache_keep_warm_one_hour: bool,
    /// When the [`Self::cache_keep_warm_one_hour`] ping goes out, counted from
    /// the start of the request it repeats. Must be above zero and below the
    /// hour, or the entry has lapsed before the ping reads it.
    #[serde(with = "humantime_serde")]
    pub cache_keep_warm_one_hour_delay: Duration,
    /// The most conversations kept warm at one moment, and `0` for no limit.
    ///
    /// Each one holds the request it re-sends — the whole prompt as this proxy
    /// sent it — until its wait ends or its pings run out, and nothing else
    /// bounded how many did: the per-wait caps above limit how long a timer
    /// lives, not how many live together. Past the limit a new wait is not
    /// armed and its cache is left to lapse; a conversation already being
    /// kept warm is unaffected. Deployment-wide, like the interval and the
    /// caps: it is one map for every provider.
    ///
    /// 1,024 is not a measurement: no deployment has been seen near it. It is
    /// where a number has to be — at a megabyte a prompt, about a gigabyte
    /// held — and it was set by review (2026-11-22), not after an incident.
    pub cache_keep_warm_max_conversations: usize,
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
    /// Answer a retrieve call with the other handles of the same tool output
    /// too, within `retrieve_max_tokens_per_request`. **Off by default.**
    ///
    /// A model that has just asked for one handle has said what it is working
    /// on, and the other elisions of the same output are the obvious next
    /// asks. On the 89-task run of 2026-10-12, 71 of 181 retrieve rounds were
    /// second-or-later, and a multi-round request billed 102,011 prompt tokens
    /// against 56,577 for a single-round one (DESIGN §20 item 13). Attaching
    /// only ever extends a round the proxy builds itself, after the model's
    /// call, so no byte the provider has cached moves. Whether the rule would
    /// have attached what the next round asked for is what
    /// `scripts/bench/sibling-rounds.py` measures on a capture.
    pub retrieve_attach_siblings: bool,
    /// Which gate decides whether a handle-bearing compression ships:
    /// `fixed` (`engine.min_elision_saving_chars`, the default) or `net_cost`
    /// ([`Self::net_cost_gate`]). Only `recoverable` mode mints handles, so
    /// only it is gated.
    ///
    /// The fixed 4,000 characters was fitted at $0.03 a retrieve round, and a
    /// round re-reads the whole prompt: at 300K tokens it costs about $0.06
    /// before output. The net-cost gate loosens on a short conversation and
    /// tightens on a long one. It decides each message once, against the
    /// prompt size the provider last billed for the conversation, and replays
    /// that decision on every later turn so the message's bytes never move.
    /// The pins live in memory: a restart re-decides what it forgot.
    pub elision_gate: ElisionGate,
    /// The net-cost gate's configuration; see `kompressor_engine::gate`.
    /// Cache read and write prices come from the provider's price list for
    /// the request's model, not from here.
    pub net_cost_gate: kompressor_engine::gate::NetCostModel,
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
    /// Where the proxy saves what it remembers about each conversation, so a
    /// restart does not forget it. **Empty — off — and that is the shipped
    /// default.**
    ///
    /// The prefix tracker's freeze watermark, cache evidence, learned
    /// conversation identities and fetch history, the cold-turn read ledger,
    /// the pinned tool order and beta tokens, and the tool-compaction
    /// decision: every per-conversation decision that changes the bytes sent
    /// upstream. Without them, every conversation open across a restart is
    /// new to the next process, which freezes it to the client's marker and
    /// sends bytes the provider holds in compressed form: one lost prefix per
    /// open conversation. Hashes, counts, flags and fragment handles only —
    /// never message text (DESIGN §12.10).
    ///
    /// Written atomically on graceful shutdown and every
    /// [`Self::state_save_interval`], read once at start. A file that is
    /// missing, unreadable, or written by another format version is ignored
    /// with a log line and the proxy starts with nothing remembered. One
    /// path per process: two proxies writing one file overwrite each other.
    pub state_path: String,
    /// How often the state file is rewritten while serving, so a crash loses
    /// at most this much. `0s` writes only on graceful shutdown.
    #[serde(with = "humantime_serde")]
    pub state_save_interval: Duration,
    /// Whether the state file is read at start. On by default; it does
    /// nothing unless [`Self::state_path`] is set. The rollback: `false`
    /// starts every process with nothing remembered, as before the file
    /// existed, and ignores what the file holds. The file is still written.
    pub state_load: bool,
    pub upstreams: Vec<Upstream>,
    pub default_upstream: String,
    pub allow_insecure_upstream: bool,
    /// See [`InterceptConfig`]. Off unless its `listen` is set.
    pub intercept: InterceptConfig,
    /// Settings that apply to one provider's traffic only, keyed `anthropic`
    /// (Messages) or `openai` (Chat Completions and Responses). See
    /// [`ProviderOverrides`].
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ProviderOverrides>,
}

/// The providers [`ProxyConfig::providers`] may name: the API family a request
/// arrives as, not the upstream it is sent to, so a mount renamed or split in
/// two still gets its provider's settings.
pub const PROVIDERS: &[&str] = &["anthropic", "openai", "gemini", "bedrock"];

/// What one provider's traffic does differently from the deployment default.
///
/// Every field is optional and an absent one inherits. It exists because the
/// settings that pay are not the same on every provider: on Claude Code against
/// Claude Opus 5.5, eliding function bodies cost more in retrieval rounds than
/// it saved and the one-hour cache TTL is Anthropic's field alone, while the
/// Codex traffic the defaults were measured on has neither problem. One
/// deployment serving both had to pick one set for both.
///
/// ```toml
/// [proxy.providers.anthropic]
/// cache_ttl = "adaptive"
/// frozen_prefix = "first_seen"
///
/// [proxy.providers.anthropic.engine]
/// enable_code_aware = false
/// ```
///
/// From the environment, `KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__CACHE_TTL`
/// and `KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__ENGINE__ENABLE_CODE_AWARE`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderOverrides {
    /// `false` forwards this provider's traffic uncompressed, measured as the
    /// control arm is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compress: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retrieve_tool: Option<RetrieveTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frozen_prefix: Option<FrozenPrefix>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_ttl: Option<CacheTtl>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_retention: Option<PromptCacheRetention>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<PromptCacheKey>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_order: Option<ToolsOrder>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sticky_beta: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_compact: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_defer: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cold_recompact: Option<ColdRecompact>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_reads: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matured_reads: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_keep_warm: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_read: Option<bool>,
    /// Prices differ by provider (OpenAI charges no cache write), so whether
    /// the net-cost gate pays may too.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elision_gate: Option<ElisionGate>,
    /// Engine options, any of `[engine]`'s that a request may override.
    #[serde(skip_serializing_if = "patch_is_empty")]
    pub engine: EngineOptionsPatch,
}

fn patch_is_empty(patch: &EngineOptionsPatch) -> bool {
    toml::Value::try_from(patch)
        .ok()
        .and_then(|value| value.as_table().map(toml::map::Map::is_empty))
        .unwrap_or(true)
}

impl ProviderOverrides {
    /// `base` with these overrides applied.
    fn applied_to(&self, base: &Config) -> Config {
        let mut out = base.clone();
        let proxy = &mut out.proxy;
        macro_rules! set {
            ($($field:ident),* $(,)?) => { $(if let Some(v) = self.$field { proxy.$field = v; })* };
        }
        set!(
            compress,
            min_tokens,
            retrieve_tool,
            frozen_prefix,
            cache_ttl,
            prompt_cache_retention,
            prompt_cache_key,
            tools_order,
            sticky_beta,
            tools_compact,
            tools_defer,
            cold_recompact,
            superseded_reads,
            matured_reads,
            cache_keep_warm,
            stale_read,
            elision_gate
        );
        out.engine = out.engine.merged(&self.engine);
        out
    }

    /// Set one field from its environment spelling, `cache_ttl` or
    /// `engine__enable_code_aware`, through the same serde names the file uses.
    ///
    /// A value is tried as an integer, then a boolean, then a string, and the
    /// first one the field accepts is kept, so `KOMPRESSOR__…__MIN_TOKENS=0`
    /// and `…__ENGINE__ENABLE_CODE_AWARE=false` both mean what they say.
    fn set_from_env(&mut self, field: &str, value: &str) -> Result<(), String> {
        let path: Vec<&str> = field.split("__").collect();
        let lowered = value.to_ascii_lowercase();
        let mut candidates = Vec::new();
        if let Ok(n) = value.parse::<i64>() {
            candidates.push(toml::Value::Integer(n));
        }
        if let Some(b) = parse_bool(value) {
            candidates.push(toml::Value::Boolean(b));
        }
        candidates.push(toml::Value::String(lowered));
        candidates.push(toml::Value::String(value.to_string()));
        let mut last = String::new();
        for candidate in candidates {
            let mut table = toml::Value::try_from(&*self).map_err(|e| e.to_string())?;
            let mut node = &mut table;
            for key in &path[..path.len() - 1] {
                node = node
                    .as_table_mut()
                    .ok_or("not a table")?
                    .entry(key.to_string())
                    .or_insert_with(|| toml::Value::Table(Default::default()));
            }
            node.as_table_mut()
                .ok_or("not a table")?
                .insert(path[path.len() - 1].to_string(), candidate);
            match table.try_into::<ProviderOverrides>() {
                Ok(parsed) => {
                    *self = parsed;
                    return Ok(());
                }
                Err(error) => last = error.to_string(),
            }
        }
        Err(last)
    }
}

/// The tools `proxy.tools_defer` keeps in the prompt unless configured
/// otherwise: the file, shell and planning tools coding agents call every
/// turn, as Claude Code, opencode and Codex spell them, and Claude Code's own
/// `ToolSearch`, which loads tools nothing else can reach.
pub const DEFAULT_TOOLS_DEFER_KEEP: [&str; 16] = [
    "bash",
    "read",
    "write",
    "edit",
    "multiedit",
    "apply_patch",
    "glob",
    "grep",
    "ls",
    "task",
    "todowrite",
    "todoread",
    "webfetch",
    "notebookedit",
    "skill",
    "toolsearch",
];

/// What `proxy.output_shaping` asks for unless configured otherwise: shorter
/// prose, and nothing taken from the work itself. It names what to leave out
/// rather than a length, so code, commands and tool calls are not squeezed.
pub const DEFAULT_OUTPUT_SHAPING_TEXT: &str = "Keep answers concise. Do not restate the \
     question, repeat what a tool returned, or summarise work already shown; \
     state the result and anything the user must act on. Code, commands and \
     tool calls are as long as they need to be.";

impl Default for ProxyConfig {
    fn default() -> Self {
        ProxyConfig {
            enabled: true,
            compress: true,
            min_tokens: 512,
            compress_timeout: Duration::from_secs(2),
            max_body_bytes: 64 * 1024 * 1024,
            guard_thinking: GuardMode::Observe,
            guard_upstream_body: GuardMode::Observe,
            upstream_connect_timeout: Duration::from_secs(10),
            upstream_read_timeout: Duration::from_secs(600),
            retrieve_tool: RetrieveTool::Auto,
            frozen_prefix: FrozenPrefix::Marker,
            cache_ttl: CacheTtl::Default,
            prompt_cache_retention: PromptCacheRetention::Default,
            prompt_cache_key: PromptCacheKey::Client,
            tools_order: ToolsOrder::Client,
            sticky_beta: false,
            reasoning_elide_models: Vec::new(),
            tools_compact: false,
            tools_defer: false,
            tools_defer_min_tools: 12,
            tools_defer_keep: DEFAULT_TOOLS_DEFER_KEEP
                .iter()
                .map(|name| name.to_string())
                .collect(),
            output_shaping: false,
            output_shaping_holdout_pct: 50,
            output_shaping_text: DEFAULT_OUTPUT_SHAPING_TEXT.to_string(),
            cold_recompact: ColdRecompact::Off,
            cold_recompact_margin: Duration::from_secs(60),
            superseded_reads: false,
            matured_reads: false,
            matured_quiesce_turns: 5,
            matured_max_hold_turns: 25,
            matured_min_bytes: 2_048,
            cache_keep_warm: true,
            cache_keep_warm_interval: Duration::from_secs(270),
            cache_keep_warm_max_pings: 11,
            cache_keep_warm_background_pings: 4,
            cache_keep_warm_background_prior: false,
            cache_keep_warm_one_hour: false,
            cache_keep_warm_one_hour_delay: Duration::from_secs(55 * 60),
            cache_keep_warm_max_conversations: 1_024,
            retrieve_execution: RetrieveExecution::Server,
            retrieve_max_rounds: 3,
            retrieve_max_tokens_per_request: 16_384,
            retrieve_attach_siblings: false,
            elision_gate: ElisionGate::Fixed,
            net_cost_gate: kompressor_engine::gate::NetCostModel::default(),
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
            // Off: a file on disk is a deployment's decision to make.
            state_path: String::new(),
            state_save_interval: Duration::from_secs(60),
            state_load: true,
            upstreams: Vec::new(),
            default_upstream: String::new(),
            allow_insecure_upstream: false,
            intercept: InterceptConfig::default(),
            providers: BTreeMap::new(),
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
    /// Sign this mount's requests with AWS Signature Version 4, over the body
    /// that is sent. For a Bedrock runtime upstream: a client's own signature
    /// covers its uncompressed body and the proxy's host, and cannot survive
    /// either. Credentials are `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`
    /// and `AWS_SESSION_TOKEN` from the proxy's environment; a client sending
    /// a Bedrock API key (`Authorization: Bearer`) is forwarded unsigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_sigv4: Option<AwsSigV4>,
}

/// AWS signing for an upstream: see [`Upstream::aws_sigv4`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AwsSigV4 {
    /// `us-east-1`, as in `bedrock-runtime.us-east-1.amazonaws.com`.
    pub region: String,
    /// The signing name: `bedrock` for the Bedrock runtime.
    pub service: String,
}

impl Default for AwsSigV4 {
    fn default() -> Self {
        AwsSigV4 {
            region: String::new(),
            service: "bedrock".to_string(),
        }
    }
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
            aws_sigv4: None,
        }
    }
}

/// Hook mode: `kompressor hook <agent>`, called by the agent itself with one
/// JSON event per tool call. See docs/design/CURSOR.md.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HooksConfig {
    pub cursor: CursorHooksConfig,
}

/// `[hooks.cursor]`: what `kompressor hook cursor` does with Cursor's events.
///
/// Read by every hook process, so a change here takes effect at the next tool
/// call without touching a file Cursor owns.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CursorHooksConfig {
    /// Master switch. Installed hooks answer `{}` when this is false.
    pub enabled: bool,
    /// Give up and answer `{}` after this long. Must stay below the `timeout`
    /// the installer writes into `hooks.json` (5 s), so the handler stops
    /// cleanly before Cursor kills it.
    #[serde(with = "humantime_serde")]
    pub deadline: Duration,
    /// Record what every tool returned and what the engine would have
    /// removed, without changing anything the model sees.
    pub shadow: bool,
    pub mcp: CursorMcpHooksConfig,
    pub shell: CursorShellHooksConfig,
}

impl Default for CursorHooksConfig {
    fn default() -> Self {
        CursorHooksConfig {
            enabled: true,
            deadline: Duration::from_secs(2),
            shadow: false,
            mcp: CursorMcpHooksConfig::default(),
            shell: CursorShellHooksConfig::default(),
        }
    }
}

/// `[hooks.cursor.mcp]`: MCP tool results, replaced through
/// `updated_mcp_tool_output`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CursorMcpHooksConfig {
    pub enabled: bool,
    /// Tools whose results are forwarded untouched. Matched against the name
    /// after `MCP:`; the hook is not told which server a tool belongs to.
    pub skip_tools: Vec<String>,
}

impl Default for CursorMcpHooksConfig {
    fn default() -> Self {
        CursorMcpHooksConfig {
            enabled: true,
            skip_tools: Vec::new(),
        }
    }
}

/// `[hooks.cursor.shell]`: shell output, compressed by rewriting the command
/// before it runs. Off by default until a measurement says otherwise.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CursorShellHooksConfig {
    pub enabled: bool,
    /// Rewrite only when the Cursor CLI would run the rewritten command:
    /// its `approvalMode` is `unrestricted`, or its allow-list
    /// (`~/.cursor/cli-config.json`, a workspace's `.cursor/cli.json`)
    /// approves this binary as the rewrite names it — `Shell(kompressor)`, or
    /// `Shell(<absolute path>)` when `PATH` does not find it. Without that,
    /// every rewritten command would be refused, including the ones the user
    /// approved. The desktop's approval settings cannot be read, so in the
    /// editor this has to be switched off for the rewrite to run.
    pub only_when_approved: bool,
    /// Commands never rewritten, matched on the first word (two for `git`).
    pub skip: Vec<String>,
    /// Captured output above this is printed uncompressed rather than read
    /// into memory. The engine is not streaming.
    pub max_capture_bytes: u64,
}

impl Default for CursorShellHooksConfig {
    fn default() -> Self {
        CursorShellHooksConfig {
            enabled: false,
            only_when_approved: true,
            skip: [
                "cd",
                "pwd",
                "echo",
                "export",
                "mkdir",
                "touch",
                "rm",
                "mv",
                "cp",
                "chmod",
                "which",
                "git add",
                "git commit",
                "git checkout",
                "git switch",
                "git push",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            max_capture_bytes: 64 * 1024 * 1024,
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
        config.source = path.map(|path| std::path::absolute(path).unwrap_or(path.to_path_buf()));
        config.apply_env(&std::env::vars().collect::<Vec<_>>());
        config.validate()?;
        Ok(config)
    }

    /// Read exactly one file: no discovery and no `KOMPRESSOR_*` overlay.
    ///
    /// For a measurement that has to mean the same on every machine — `bench
    /// replay --config`, whose `--save` writes the baseline `--compare` checks.
    /// [`Config::load`] overlays the process environment whatever path it is
    /// given, so an ambient `KOMPRESSOR_MIN_COMPRESS_CHARS=1000000` turned a
    /// replay that removed 78.91% of a captured payload into one that found
    /// nothing eligible, and still exited 0.
    pub fn load_file(path: &std::path::Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::Read(path.display().to_string(), e))?;
        let config: Config =
            toml::from_str(&text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// Overlay environment variables. Split out from [`Config::load`] so it can
    /// be tested without touching the process environment.
    ///
    /// The structured form wins where both spellings name one setting, and it
    /// wins whatever order the variables arrive in. That takes two passes:
    /// every flat `KOMPRESSOR_<FIELD>` first, then every structured
    /// `KOMPRESSOR__SECTION__KEY`. One pass in arrival order let whichever came
    /// *later* in the process environment win — `KOMPRESSOR__ENGINE__MAX_ARRAY_ITEMS=7`
    /// then `KOMPRESSOR_MAX_ARRAY_ITEMS=25` resolved to 25, the reverse to 7,
    /// and `check-config` accepted both without a word.
    ///
    /// Within one spelling the later variable wins, as it always did. A
    /// structured value the setting cannot parse is ignored where its parser
    /// ignores it, which leaves the flat value (or the file's) in force rather
    /// than the default.
    pub fn apply_env(&mut self, vars: &[(String, String)]) {
        let named = || {
            vars.iter()
                .filter_map(|(key, value)| Some((key, key.strip_prefix("KOMPRESSOR_")?, value)))
        };
        // What each variable did is read off the configuration itself, before
        // and after: the parsers below ignore what they cannot read in a dozen
        // different ways, and this sees all of them.
        let mut before = self.settings();
        let mut apply = |config: &mut Config, name: &String, apply: &dyn Fn(&mut Config)| {
            let refused = config.refused_env.len();
            apply(config);
            let after = config.settings();
            let changed = after != before || config.refused_env.len() != refused;
            before = after;
            config.env.push(EnvOverride {
                name: name.clone(),
                effect: if changed {
                    EnvEffect::Applied
                } else {
                    EnvEffect::NoEffect
                },
            });
        };
        for (name, rest, value) in named().filter(|(_, rest, _)| !rest.starts_with('_')) {
            if ENV_READ_ELSEWHERE.contains(&rest) {
                self.env.push(EnvOverride {
                    name: name.clone(),
                    effect: EnvEffect::Elsewhere,
                });
            } else if value.is_empty() {
                apply(self, name, &|_| {});
            } else {
                apply(self, name, &|config| config.apply_flat(rest, value));
            }
        }
        for (name, rest, value) in named() {
            if let Some(rest) = rest.strip_prefix('_') {
                if value.is_empty() {
                    apply(self, name, &|_| {});
                } else {
                    apply(self, name, &|config| config.apply_structured(rest, value));
                }
            }
        }
    }

    /// `KOMPRESSOR__HOOKS__CURSOR__*`.
    fn apply_hooks_field(&mut self, field: &str, value: &str) {
        let Some(field) = field.strip_prefix("cursor__") else {
            return;
        };
        let cursor = &mut self.hooks.cursor;
        let list = |value: &str| -> Vec<String> {
            value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(String::from)
                .collect()
        };
        match field {
            "enabled" => {
                cursor.enabled = parse_bool_setting("hooks.cursor.enabled", value, cursor.enabled)
            }
            "shadow" => {
                cursor.shadow = parse_bool_setting("hooks.cursor.shadow", value, cursor.shadow)
            }
            "deadline" => {
                if let Some(d) = parse_duration(value) {
                    cursor.deadline = d;
                }
            }
            "mcp__enabled" => {
                cursor.mcp.enabled =
                    parse_bool_setting("hooks.cursor.mcp.enabled", value, cursor.mcp.enabled)
            }
            "mcp__skip_tools" => cursor.mcp.skip_tools = list(value),
            "shell__enabled" => {
                cursor.shell.enabled =
                    parse_bool_setting("hooks.cursor.shell.enabled", value, cursor.shell.enabled)
            }
            "shell__only_when_approved" => {
                cursor.shell.only_when_approved = parse_bool_setting(
                    "hooks.cursor.shell.only_when_approved",
                    value,
                    cursor.shell.only_when_approved,
                )
            }
            "shell__skip" => cursor.shell.skip = list(value),
            "shell__max_capture_bytes" => {
                if let Ok(bytes) = value.parse() {
                    cursor.shell.max_capture_bytes = bytes;
                }
            }
            _ => {}
        }
    }

    /// Every setting as one value, for telling whether something changed any.
    fn settings(&self) -> Option<String> {
        serde_json::to_string(self).ok()
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
                "stats_listen" => self.server.stats_listen = value.to_string(),
                // The engine permit every surface shares, proxy included, so
                // it is worth setting per deployment without a file.
                "max_inflight_compressions" => {
                    if let Ok(v) = value.parse() {
                        self.server.max_inflight_compressions = v;
                    }
                }
                "queue_timeout" => {
                    if let Some(d) = parse_duration(value) {
                        self.server.queue_timeout = d;
                    }
                }
                _ => {}
            },
            "auth" => match field {
                // Not "leave the setting standing", which is the rule for the
                // tuning knobs. The setting left standing here is usually
                // `none`: an operator who wrote `api-keys` or `apikeys` got a
                // server that started, validated, and served every caller as
                // the anonymous tenant. An unreadable auth mode stops the
                // server instead (see `validate`).
                "mode" => match value.to_ascii_lowercase().replace('-', "_").as_str() {
                    "api_keys" | "apikeys" => self.auth.mode = AuthMode::ApiKeys,
                    "none" => self.auth.mode = AuthMode::None,
                    _ => self.refused_env.push(format!(
                        "auth.mode cannot be {value:?}: expected none or api_keys"
                    )),
                },
                "secret" => self.auth.secret = value.to_string(),
                _ => {}
            },
            "maintenance" => match field {
                "backup" => {
                    self.maintenance.backup =
                        parse_bool_setting("maintenance.backup", value, self.maintenance.backup)
                }
                "actions" => {
                    self.maintenance.actions =
                        parse_bool_setting("maintenance.actions", value, self.maintenance.actions)
                }
                "erase" => {
                    self.maintenance.erase =
                        parse_bool_setting("maintenance.erase", value, self.maintenance.erase)
                }
                "backup_dir" => self.maintenance.backup_dir = PathBuf::from(value),
                "backup_every" => {
                    if let Some(every) = parse_duration(value) {
                        self.maintenance.backup_every = every;
                    }
                }
                "backup_keep" => {
                    if let Ok(keep) = value.parse() {
                        self.maintenance.backup_keep = keep;
                    }
                }
                "backup_max_age" => {
                    if let Some(age) = parse_duration(value) {
                        self.maintenance.backup_max_age = age;
                    }
                }
                "optimize_every" => {
                    if let Some(every) = parse_duration(value) {
                        self.maintenance.optimize_every = every;
                    }
                }
                "s3__enabled" => {
                    self.maintenance.s3.enabled = parse_bool_setting(
                        "maintenance.s3.enabled",
                        value,
                        self.maintenance.s3.enabled,
                    )
                }
                "s3__path_style" => {
                    self.maintenance.s3.path_style = parse_bool_setting(
                        "maintenance.s3.path_style",
                        value,
                        self.maintenance.s3.path_style,
                    )
                }
                "s3__keep" => {
                    if let Ok(keep) = value.parse() {
                        self.maintenance.s3.keep = keep;
                    }
                }
                "s3__endpoint" => self.maintenance.s3.endpoint = value.to_string(),
                "s3__region" => self.maintenance.s3.region = value.to_string(),
                "s3__bucket" => self.maintenance.s3.bucket = value.to_string(),
                "s3__prefix" => self.maintenance.s3.prefix = value.to_string(),
                "s3__access_key_id" => self.maintenance.s3.access_key_id = value.to_string(),
                "s3__secret_access_key" => {
                    self.maintenance.s3.secret_access_key = value.to_string()
                }
                _ => {}
            },
            "refs" => match field {
                "path" => self.refs.path = PathBuf::from(value),
                // Refused when unreadable, for the same reason as `auth.mode`:
                // the backend left standing is SQLite, so asking for an
                // in-memory store with a misspelt name wrote every fragment
                // to disk.
                "backend" => match value.to_ascii_lowercase().as_str() {
                    "memory" => self.refs.backend = RefsBackend::Memory,
                    "sqlite" => self.refs.backend = RefsBackend::Sqlite,
                    "redis" => self.refs.backend = RefsBackend::Redis,
                    _ => self.refused_env.push(format!(
                        "refs.backend cannot be {value:?}: expected sqlite, memory or redis"
                    )),
                },
                "redis_url" => self.refs.redis_url = value.to_string(),
                "redis_prefix" => self.refs.redis_prefix = value.to_string(),
                "redis_timeout" => match parse_duration(value) {
                    Some(timeout) => self.refs.redis_timeout = timeout,
                    None => self.refused_env.push(format!(
                        "refs.redis_timeout cannot be {value:?}: not a duration"
                    )),
                },
                "redis_pool" => {
                    if let Ok(pool) = value.parse() {
                        self.refs.redis_pool = pool;
                    }
                }
                "browse" => {
                    self.refs.browse = parse_bool_setting("refs.browse", value, self.refs.browse)
                }
                "recent_cache_bytes" => {
                    if let Ok(bytes) = value.parse() {
                        self.refs.recent_cache_bytes = bytes;
                    }
                }
                _ => {}
            },
            "stats" => match field {
                "path" => self.stats.path = PathBuf::from(value),
                "retention_days" => {
                    if let Ok(days) = value.parse() {
                        self.stats.retention_days = days;
                    }
                }
                "context_retention_days" => {
                    if let Ok(days) = value.parse() {
                        self.stats.context_retention_days = days;
                    }
                }
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
            "hooks" => self.apply_hooks_field(field, value),
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
            "max_compress_bytes" => num!(e.max_compress_bytes),
            "min_elision_saving_chars" => num!(e.min_elision_saving_chars),
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
            "enable_runtime_frame_collapse" => flag!(e.enable_runtime_frame_collapse),
            "keep_test_totals" => flag!(e.keep_test_totals),
            "enable_json_csv" => flag!(e.enable_json_csv),
            "protect_tool_error_chars" => num!(e.protect_tool_error_chars),
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
            // A typo leaves the guard as it was rather than turning it off.
            "guard_thinking" => {
                self.proxy.guard_thinking =
                    GuardMode::parse(value).unwrap_or(self.proxy.guard_thinking);
            }
            "guard_upstream_body" => {
                self.proxy.guard_upstream_body =
                    GuardMode::parse(value).unwrap_or(self.proxy.guard_upstream_body);
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
            "cache_ttl" => {
                // Unrecognised values leave the setting alone, for the reason
                // `frozen_prefix` does: it changes what the provider bills.
                self.proxy.cache_ttl = match value.to_ascii_lowercase().as_str() {
                    "default" => CacheTtl::Default,
                    "1h" => CacheTtl::OneHour,
                    "adaptive" => CacheTtl::Adaptive,
                    _ => self.proxy.cache_ttl,
                };
            }
            "prompt_cache_retention" => {
                // A typo leaves the setting alone, as `cache_ttl` does.
                self.proxy.prompt_cache_retention = match value.to_ascii_lowercase().as_str() {
                    "default" => PromptCacheRetention::Default,
                    "24h" => PromptCacheRetention::Extended,
                    _ => self.proxy.prompt_cache_retention,
                };
            }
            "prompt_cache_key" => {
                self.proxy.prompt_cache_key = match value.to_ascii_lowercase().as_str() {
                    "client" => PromptCacheKey::Client,
                    "conversation" => PromptCacheKey::Conversation,
                    _ => self.proxy.prompt_cache_key,
                };
            }
            "tools_order" => {
                self.proxy.tools_order = match value.to_ascii_lowercase().as_str() {
                    "client" => ToolsOrder::Client,
                    "pinned" => ToolsOrder::Pinned,
                    _ => self.proxy.tools_order,
                };
            }
            "cold_recompact" => {
                // A typo leaves the setting alone: it changes what is rewritten.
                self.proxy.cold_recompact = match value.to_ascii_lowercase().as_str() {
                    "off" => ColdRecompact::Off,
                    "observed" => ColdRecompact::Observed,
                    "predicted" => ColdRecompact::Predicted,
                    _ => self.proxy.cold_recompact,
                };
            }
            "cold_recompact_margin" => match parse_duration(value) {
                Some(margin) => self.proxy.cold_recompact_margin = margin,
                None => tracing::warn!(
                    value,
                    "KOMPRESSOR__PROXY__COLD_RECOMPACT_MARGIN is not a duration; ignored"
                ),
            },
            "superseded_reads" => {
                self.proxy.superseded_reads = parse_bool_setting(
                    "proxy.superseded_reads",
                    value,
                    self.proxy.superseded_reads,
                );
            }
            "matured_reads" => {
                self.proxy.matured_reads =
                    parse_bool_setting("proxy.matured_reads", value, self.proxy.matured_reads);
            }
            "matured_quiesce_turns" => {
                if let Ok(turns) = value.parse() {
                    self.proxy.matured_quiesce_turns = turns;
                }
            }
            "matured_max_hold_turns" => {
                if let Ok(turns) = value.parse() {
                    self.proxy.matured_max_hold_turns = turns;
                }
            }
            "matured_min_bytes" => {
                if let Ok(bytes) = value.parse() {
                    self.proxy.matured_min_bytes = bytes;
                }
            }
            "reasoning_elide_models" => {
                self.proxy.reasoning_elide_models = value
                    .split(',')
                    .map(str::trim)
                    .filter(|model| !model.is_empty())
                    .map(str::to_string)
                    .collect();
            }
            "tools_compact" => {
                self.proxy.tools_compact =
                    parse_bool_setting("proxy.tools_compact", value, self.proxy.tools_compact);
            }
            "tools_defer" => {
                self.proxy.tools_defer =
                    parse_bool_setting("proxy.tools_defer", value, self.proxy.tools_defer);
            }
            "tools_defer_min_tools" => {
                if let Ok(v) = value.parse() {
                    self.proxy.tools_defer_min_tools = v;
                }
            }
            "tools_defer_keep" => {
                self.proxy.tools_defer_keep = value
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string)
                    .collect();
            }
            "output_shaping" => {
                self.proxy.output_shaping =
                    parse_bool_setting("proxy.output_shaping", value, self.proxy.output_shaping);
            }
            "output_shaping_holdout_pct" => {
                if let Ok(v) = value.parse() {
                    self.proxy.output_shaping_holdout_pct = v;
                }
            }
            "output_shaping_text" => self.proxy.output_shaping_text = value.to_string(),
            "sticky_beta" => {
                self.proxy.sticky_beta =
                    parse_bool_setting("proxy.sticky_beta", value, self.proxy.sticky_beta);
            }
            "cache_keep_warm" => {
                self.proxy.cache_keep_warm =
                    parse_bool_setting("proxy.cache_keep_warm", value, self.proxy.cache_keep_warm);
            }
            "cache_keep_warm_interval" => match parse_duration(value) {
                Some(interval) => self.proxy.cache_keep_warm_interval = interval,
                None => tracing::warn!(
                    value,
                    "KOMPRESSOR__PROXY__CACHE_KEEP_WARM_INTERVAL is not a duration; ignored"
                ),
            },
            "cache_keep_warm_max_pings" => {
                if let Ok(v) = value.parse() {
                    self.proxy.cache_keep_warm_max_pings = v;
                }
            }
            "cache_keep_warm_background_pings" => {
                if let Ok(v) = value.parse() {
                    self.proxy.cache_keep_warm_background_pings = v;
                }
            }
            "cache_keep_warm_background_prior" => {
                self.proxy.cache_keep_warm_background_prior = parse_bool_setting(
                    "proxy.cache_keep_warm_background_prior",
                    value,
                    self.proxy.cache_keep_warm_background_prior,
                );
            }
            "cache_keep_warm_one_hour" => {
                self.proxy.cache_keep_warm_one_hour = parse_bool_setting(
                    "proxy.cache_keep_warm_one_hour",
                    value,
                    self.proxy.cache_keep_warm_one_hour,
                );
            }
            "cache_keep_warm_one_hour_delay" => match parse_duration(value) {
                Some(delay) => self.proxy.cache_keep_warm_one_hour_delay = delay,
                None => tracing::warn!(
                    value,
                    "KOMPRESSOR__PROXY__CACHE_KEEP_WARM_ONE_HOUR_DELAY is not a duration; ignored"
                ),
            },
            "cache_keep_warm_max_conversations" => {
                if let Ok(v) = value.parse() {
                    self.proxy.cache_keep_warm_max_conversations = v;
                }
            }
            "retrieve_attach_siblings" => {
                self.proxy.retrieve_attach_siblings = parse_bool_setting(
                    "proxy.retrieve_attach_siblings",
                    value,
                    self.proxy.retrieve_attach_siblings,
                );
            }
            // Unrecognised values leave the gate standing: it decides what is
            // elided, so a typo must not quietly change it.
            "elision_gate" => match value.to_ascii_lowercase().replace('-', "_").as_str() {
                "fixed" => self.proxy.elision_gate = ElisionGate::Fixed,
                "net_cost" => self.proxy.elision_gate = ElisionGate::NetCost,
                _ => tracing::warn!(
                    value,
                    "KOMPRESSOR__PROXY__ELISION_GATE is not fixed or net_cost; ignored"
                ),
            },
            "net_cost_gate__expected_reads" => {
                if let Ok(v) = value.parse() {
                    self.proxy.net_cost_gate.expected_reads = v;
                }
            }
            "net_cost_gate__output_cost_tokens" => {
                if let Ok(v) = value.parse() {
                    self.proxy.net_cost_gate.output_cost_tokens = v;
                }
            }
            "net_cost_gate__fetch_probability" => {
                if let Ok(v) = value.parse() {
                    self.proxy.net_cost_gate.fetch_probability = v;
                }
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
            "state_path" => self.proxy.state_path = value.to_string(),
            "state_save_interval" => match parse_duration(value) {
                Some(interval) => self.proxy.state_save_interval = interval,
                None => tracing::warn!(
                    value,
                    "proxy.state_save_interval is not a duration; keeping the configured value"
                ),
            },
            // Reachable from the environment because it is the rollback.
            "state_load" => {
                self.proxy.state_load =
                    parse_bool_setting("proxy.state_load", value, self.proxy.state_load);
            }
            provider if provider.starts_with("providers__") => {
                let rest = &provider["providers__".len()..];
                let Some((name, setting)) = rest.split_once("__") else {
                    tracing::warn!(
                        key = provider,
                        "KOMPRESSOR__PROXY__PROVIDERS__ needs a provider and a setting"
                    );
                    return;
                };
                if !PROVIDERS.contains(&name) {
                    tracing::warn!(
                        provider = name,
                        known = ?PROVIDERS,
                        "KOMPRESSOR__PROXY__PROVIDERS__* names a provider this build does not know; ignored"
                    );
                    return;
                }
                let overrides = self.proxy.providers.entry(name.to_string()).or_default();
                if let Err(error) = overrides.set_from_env(setting, value) {
                    tracing::warn!(
                        provider = name,
                        setting,
                        value,
                        %error,
                        "KOMPRESSOR__PROXY__PROVIDERS__* could not be applied; it has been ignored"
                    );
                }
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
        if let Some(refused) = self.refused_env.first() {
            return Err(ConfigError::Invalid(format!(
                "environment override refused: {refused}"
            )));
        }
        // Zero would mature a read on the turn after it was made, which is
        // the case the setting exists to wait out.
        if self.proxy.matured_quiesce_turns == 0 || self.proxy.matured_max_hold_turns == 0 {
            return Err(ConfigError::Invalid(
                "proxy.matured_quiesce_turns and proxy.matured_max_hold_turns must be at least 1"
                    .into(),
            ));
        }
        if self.hooks.cursor.deadline.is_zero()
            || self.hooks.cursor.deadline >= Duration::from_secs(5)
        {
            return Err(ConfigError::Invalid(
                "hooks.cursor.deadline must be greater than zero and below the 5 s timeout \
                 `hook install cursor` writes into hooks.json"
                    .into(),
            ));
        }
        if self.refs.purge_interval.is_zero() {
            return Err(ConfigError::Invalid(
                "refs.purge_interval must be greater than zero".into(),
            ));
        }
        if let Some((_, every)) = self.maintenance.schedule()
            && every < Duration::from_secs(60)
        {
            return Err(ConfigError::Invalid(
                "maintenance.backup_every must be at least one minute (or zero for none)".into(),
            ));
        }
        if self.maintenance.s3.enabled
            && let Some(problem) = self.maintenance.s3.problem()
        {
            return Err(ConfigError::Invalid(problem));
        }
        if let Some(server) = self
            .mcp
            .servers
            .iter()
            .find(|s| s.startup_timeout.is_zero())
        {
            return Err(ConfigError::Invalid(format!(
                "mcp.servers {:?}: startup_timeout must be greater than zero",
                server.name
            )));
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
            if let Some(sigv4) = &upstream.aws_sigv4
                && (sigv4.region.trim().is_empty() || sigv4.service.trim().is_empty())
            {
                return Err(ConfigError::Invalid(format!(
                    "upstream {:?}: aws_sigv4 needs a region and a service",
                    upstream.name
                )));
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
        if self.refs.backend == RefsBackend::Redis {
            kompressor_store::redis::parse_url(&self.refs.redis_url)
                .map_err(|e| ConfigError::Invalid(format!("refs.redis_url: {e}")))?;
            if self.refs.redis_prefix.is_empty() || self.refs.redis_timeout.is_zero() {
                return Err(ConfigError::Invalid(
                    "refs.redis_prefix must not be empty and refs.redis_timeout must be above zero"
                        .into(),
                ));
            }
        }
        // A deferral needs at least one tool left in the prompt beside the
        // search tool, and one deferred: below two there is nothing to choose.
        if self.proxy.tools_defer_min_tools < 2 {
            return Err(ConfigError::Invalid(
                "proxy.tools_defer_min_tools must be at least 2".into(),
            ));
        }
        // Checked whether or not the feature is on, so turning it on for one
        // provider later cannot meet an interval nobody looked at.
        if self.proxy.cache_keep_warm_interval.is_zero()
            || self.proxy.cache_keep_warm_interval >= Duration::from_secs(300)
        {
            return Err(ConfigError::Invalid(
                "proxy.cache_keep_warm_interval must be above zero and below the provider's \
                 five-minute cache TTL, or the entry lapses between pings"
                    .into(),
            ));
        }
        if self.proxy.cache_keep_warm_one_hour_delay.is_zero()
            || self.proxy.cache_keep_warm_one_hour_delay >= Duration::from_secs(3_600)
        {
            return Err(ConfigError::Invalid(
                "proxy.cache_keep_warm_one_hour_delay must be above zero and below the \
                 one-hour cache TTL, or the entry has lapsed before the ping reads it"
                    .into(),
            ));
        }

        self.proxy
            .net_cost_gate
            .validate()
            .map_err(|e| ConfigError::Invalid(format!("proxy.net_cost_gate: {e}")))?;
        // Checked whether or not the experiment is on, for the reason the
        // keep-warm interval is: turning it on later must not meet a value
        // nobody looked at.
        if !(30..=100).contains(&self.proxy.output_shaping_holdout_pct) {
            return Err(ConfigError::Invalid(
                "proxy.output_shaping_holdout_pct must be between 30 and 100: an \
                 experiment that holds out fewer conversations cannot measure itself"
                    .into(),
            ));
        }
        if self.proxy.output_shaping_text.trim().is_empty()
            || self.proxy.output_shaping_text.chars().count() > 2_000
        {
            return Err(ConfigError::Invalid(
                "proxy.output_shaping_text must be non-empty and at most 2,000 characters".into(),
            ));
        }
        if self.retrieve.max_retrieve_chars == 0 {
            return Err(ConfigError::Invalid(
                "retrieve.max_retrieve_chars must be positive".into(),
            ));
        }
        for (name, overrides) in &self.proxy.providers {
            if !PROVIDERS.contains(&name.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "proxy.providers.{name} names no provider this build knows; use one of {PROVIDERS:?}"
                )));
            }
            overrides
                .applied_to(self)
                .engine
                .validate()
                .map_err(|e| ConfigError::Invalid(format!("proxy.providers.{name}.engine: {e}")))?;
        }
        Ok(())
    }

    /// This configuration as one provider's traffic sees it: `self` with
    /// `proxy.providers.<provider>` applied, or `None` when it sets nothing.
    pub fn for_provider(&self, provider: &str) -> Option<Config> {
        self.proxy
            .providers
            .get(provider)
            .map(|overrides| overrides.applied_to(self))
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
        if !copy.maintenance.s3.secret_access_key.is_empty() {
            copy.maintenance.s3.secret_access_key = "<redacted>".to_string();
        }
        // `redis://user:password@host`: the password goes, the rest stays so
        // the address can still be read.
        if let Some((scheme, rest)) = copy.refs.redis_url.split_once("://")
            && let Some((auth, host)) = rest.rsplit_once('@')
        {
            let user = auth.split_once(':').map_or("", |(user, _)| user);
            copy.refs.redis_url = format!("{scheme}://{user}:<redacted>@{host}");
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
    fn mcp_servers_are_none_by_default() {
        assert!(Config::default().mcp.servers.is_empty());
    }

    #[test]
    fn an_mcp_server_entry_takes_defaults_for_what_it_does_not_name() {
        let config: Config = toml::from_str(
            "[[mcp.servers]]\nname = \"fs\"\ncommand = \"npx\"\n\
             args = [\"-y\", \"@modelcontextprotocol/server-filesystem\"]\n\
             [mcp.servers.env]\nFS_ROOT = \"/srv\"\n\
             [[mcp.servers]]\nname = \"wiki\"\ncommand = \"wiki-mcp\"\ncompress = false\n",
        )
        .expect("parses");
        config.validate().expect("valid");
        let [fs, wiki] = config.mcp.servers.as_slice() else {
            panic!("two servers: {:?}", config.mcp.servers);
        };
        assert_eq!(fs.name, "fs");
        assert_eq!(fs.command.as_deref(), Some("npx"));
        assert_eq!(fs.args, ["-y", "@modelcontextprotocol/server-filesystem"]);
        assert_eq!(fs.env.get("FS_ROOT").map(String::as_str), Some("/srv"));
        assert_eq!(fs.url, None);
        assert!(fs.compress, "compressing is the default");
        assert!(!wiki.compress);
        assert!(wiki.args.is_empty() && wiki.env.is_empty());
        assert_eq!(fs.startup_timeout, Duration::from_secs(60));
        // The rest of `[mcp]` is untouched by naming servers.
        assert_eq!(config.mcp.path, "/mcp");
    }

    #[test]
    fn an_mcp_server_startup_timeout_is_a_duration_above_zero() {
        let config: Config = toml::from_str(
            "[[mcp.servers]]\nname = \"fs\"\ncommand = \"npx\"\nstartup_timeout = \"5s\"\n",
        )
        .expect("parses");
        assert_eq!(
            config.mcp.servers[0].startup_timeout,
            Duration::from_secs(5)
        );
        config.validate().expect("valid");

        let zero: Config = toml::from_str(
            "[[mcp.servers]]\nname = \"fs\"\ncommand = \"npx\"\nstartup_timeout = \"0s\"\n",
        )
        .expect("parses");
        let error = zero
            .validate()
            .expect_err("a zero timeout fails every server");
        assert!(error.to_string().contains("startup_timeout"), "{error}");
    }

    #[test]
    fn a_misspelt_mcp_server_key_is_refused() {
        assert!(
            toml::from_str::<Config>("[[mcp.servers]]\nname = \"fs\"\ncomand = \"npx\"\n").is_err()
        );
    }

    #[test]
    fn a_zero_purge_interval_is_rejected() {
        let mut config = Config::default();
        config.refs.purge_interval = Duration::ZERO;
        assert!(config.validate().is_err());
    }

    #[test]
    fn provider_overrides_read_from_a_file_apply_to_that_provider_only() {
        let config: Config = toml::from_str(
            "[proxy]\ncache_ttl = \"default\"\n\
             [proxy.providers.anthropic]\ncache_ttl = \"adaptive\"\nfrozen_prefix = \"first_seen\"\n\
             [proxy.providers.anthropic.engine]\nenable_code_aware = false\n",
        )
        .expect("parses");
        config.validate().expect("valid");
        let anthropic = config.for_provider("anthropic").expect("named");
        assert_eq!(anthropic.proxy.cache_ttl, CacheTtl::Adaptive);
        assert_eq!(anthropic.proxy.frozen_prefix, FrozenPrefix::FirstSeen);
        assert!(!anthropic.engine.enable_code_aware);
        assert_eq!(
            anthropic.proxy.retrieve_tool, config.proxy.retrieve_tool,
            "what it does not name it inherits"
        );
        assert!(
            config.for_provider("openai").is_none(),
            "openai keeps the defaults"
        );
        assert!(config.engine.enable_code_aware);
        assert_eq!(config.proxy.cache_ttl, CacheTtl::Default);
    }

    #[test]
    fn a_provider_this_build_does_not_know_is_rejected() {
        let config: Config =
            toml::from_str("[proxy.providers.antrhopic]\ncache_ttl = \"1h\"\n").expect("parses");
        assert!(
            config.validate().is_err(),
            "a typo must not silently do nothing"
        );
        assert!(
            toml::from_str::<Config>("[proxy.providers.anthropic]\ncache_tll = \"1h\"\n").is_err(),
            "nor a misspelt setting"
        );
    }

    #[test]
    fn provider_overrides_can_be_set_from_the_environment() {
        let mut config = Config::default();
        config.apply_env(&[
            (
                "KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__CACHE_TTL".into(),
                "adaptive".into(),
            ),
            (
                "KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__FROZEN_PREFIX".into(),
                "first_seen".into(),
            ),
            (
                "KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__RETRIEVE_TOOL".into(),
                "Always".into(),
            ),
            (
                "KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__MIN_TOKENS".into(),
                "1000".into(),
            ),
            (
                "KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__ENGINE__ENABLE_CODE_AWARE".into(),
                "false".into(),
            ),
            (
                "KOMPRESSOR__PROXY__PROVIDERS__OPENAI__COMPRESS".into(),
                "false".into(),
            ),
        ]);
        config.validate().expect("valid");
        let anthropic = config.for_provider("anthropic").unwrap();
        assert_eq!(anthropic.proxy.cache_ttl, CacheTtl::Adaptive);
        assert_eq!(anthropic.proxy.frozen_prefix, FrozenPrefix::FirstSeen);
        assert_eq!(anthropic.proxy.retrieve_tool, RetrieveTool::Always);
        assert_eq!(anthropic.proxy.min_tokens, 1000);
        assert!(!anthropic.engine.enable_code_aware);
        assert!(anthropic.proxy.compress);
        let openai = config.for_provider("openai").unwrap();
        assert!(!openai.proxy.compress);
        assert!(openai.engine.enable_code_aware);
        assert_eq!(openai.proxy.cache_ttl, CacheTtl::Default);
    }

    #[test]
    fn an_unreadable_provider_override_changes_nothing() {
        let mut config = Config::default();
        config.apply_env(&[
            (
                "KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__CACHE_TTL".into(),
                "60m".into(),
            ),
            (
                "KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__NOT_A_SETTING".into(),
                "1".into(),
            ),
            (
                "KOMPRESSOR__PROXY__PROVIDERS__MISTRAL__CACHE_TTL".into(),
                "1h".into(),
            ),
        ]);
        assert!(!config.proxy.providers.contains_key("mistral"));
        let anthropic = config
            .for_provider("anthropic")
            .unwrap_or_else(|| config.clone());
        assert_eq!(anthropic.proxy.cache_ttl, CacheTtl::Default);
        config.validate().expect("still valid");
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
    fn the_stability_guards_observe_by_default_and_are_set_from_the_environment() {
        let mut config = Config::default();
        assert_eq!(config.proxy.guard_thinking, GuardMode::Observe);
        assert_eq!(config.proxy.guard_upstream_body, GuardMode::Observe);
        assert_eq!(config.engine.max_compress_bytes, 0, "no payload bound");
        config.apply_env(&[
            ("KOMPRESSOR__PROXY__GUARD_THINKING".into(), "enforce".into()),
            (
                "KOMPRESSOR__PROXY__GUARD_UPSTREAM_BODY".into(),
                "OFF".into(),
            ),
            (
                "KOMPRESSOR__ENGINE__MAX_COMPRESS_BYTES".into(),
                "1048576".into(),
            ),
        ]);
        assert_eq!(config.proxy.guard_thinking, GuardMode::Enforce);
        assert_eq!(config.proxy.guard_upstream_body, GuardMode::Off);
        assert_eq!(config.engine.max_compress_bytes, 1_048_576);
        // A typo leaves the guard as it was.
        config.apply_env(&[("KOMPRESSOR__PROXY__GUARD_THINKING".into(), "strict".into())]);
        assert_eq!(config.proxy.guard_thinking, GuardMode::Enforce);

        let parsed: Config = toml::from_str(
            "[proxy]\nguard_thinking = \"enforce\"\nguard_upstream_body = \"off\"\n\
             [engine]\nmax_compress_bytes = 2048\n",
        )
        .unwrap();
        assert_eq!(parsed.proxy.guard_thinking, GuardMode::Enforce);
        assert_eq!(parsed.proxy.guard_upstream_body, GuardMode::Off);
        assert_eq!(parsed.engine.max_compress_bytes, 2048);
        assert!(toml::from_str::<Config>("[proxy]\nguard_thinking = \"strict\"\n").is_err());
    }

    #[test]
    fn the_prefix_stabilisers_are_off_by_default_and_set_per_provider_or_from_the_environment() {
        let mut config = Config::default();
        assert_eq!(config.proxy.prompt_cache_key, PromptCacheKey::Client);
        assert_eq!(config.proxy.tools_order, ToolsOrder::Client);
        assert!(!config.proxy.sticky_beta);
        assert!(config.proxy.reasoning_elide_models.is_empty());
        config.apply_env(&[(
            "KOMPRESSOR__PROXY__REASONING_ELIDE_MODELS".into(),
            "deepseek-reasoner, kimi-k2*,".into(),
        )]);
        assert_eq!(
            config.proxy.reasoning_elide_models,
            ["deepseek-reasoner", "kimi-k2*"]
        );
        let listed: Config =
            toml::from_str("[proxy]\nreasoning_elide_models = [\"glm-4.6\"]\n").unwrap();
        assert_eq!(listed.proxy.reasoning_elide_models, ["glm-4.6"]);
        assert!(!config.proxy.tools_defer);
        assert_eq!(config.proxy.tools_defer_min_tools, 12);
        config.apply_env(&[
            ("KOMPRESSOR__PROXY__TOOLS_DEFER".into(), "true".into()),
            (
                "KOMPRESSOR__PROXY__TOOLS_DEFER_MIN_TOOLS".into(),
                "20".into(),
            ),
            (
                "KOMPRESSOR__PROXY__TOOLS_DEFER_KEEP".into(),
                "Bash, Read".into(),
            ),
        ]);
        assert!(config.proxy.tools_defer);
        assert_eq!(config.proxy.tools_defer_min_tools, 20);
        assert_eq!(config.proxy.tools_defer_keep, ["Bash", "Read"]);
        let deferring: Config =
            toml::from_str("[proxy.providers.anthropic]\ntools_defer = true\n").unwrap();
        assert!(!deferring.proxy.tools_defer);
        assert!(
            deferring
                .for_provider("anthropic")
                .unwrap()
                .proxy
                .tools_defer
        );
        assert!(!config.proxy.tools_compact);
        config.apply_env(&[("KOMPRESSOR__PROXY__TOOLS_COMPACT".into(), "true".into())]);
        assert!(config.proxy.tools_compact);
        let per_provider: Config =
            toml::from_str("[proxy.providers.openai]\ntools_compact = true\n").unwrap();
        assert!(!per_provider.proxy.tools_compact);
        assert!(
            per_provider
                .for_provider("openai")
                .expect("openai has overrides")
                .proxy
                .tools_compact
        );

        config.apply_env(&[
            (
                "KOMPRESSOR__PROXY__PROMPT_CACHE_KEY".into(),
                "conversation".into(),
            ),
            ("KOMPRESSOR__PROXY__TOOLS_ORDER".into(), "pinned".into()),
            ("KOMPRESSOR__PROXY__STICKY_BETA".into(), "true".into()),
        ]);
        assert_eq!(config.proxy.prompt_cache_key, PromptCacheKey::Conversation);
        assert_eq!(config.proxy.tools_order, ToolsOrder::Pinned);
        assert!(config.proxy.sticky_beta);
        // A typo leaves the setting alone.
        config.apply_env(&[("KOMPRESSOR__PROXY__TOOLS_ORDER".into(), "sorted".into())]);
        assert_eq!(config.proxy.tools_order, ToolsOrder::Pinned);

        let parsed: Config = toml::from_str(
            "[proxy.providers.anthropic]\nsticky_beta = true\ntools_order = \"pinned\"\n\
             [proxy.providers.openai]\nprompt_cache_key = \"conversation\"\n",
        )
        .unwrap();
        assert!(
            !parsed.proxy.sticky_beta,
            "the deployment default is untouched"
        );
        let anthropic = parsed
            .for_provider("anthropic")
            .expect("anthropic has overrides");
        assert!(anthropic.proxy.sticky_beta);
        assert_eq!(anthropic.proxy.tools_order, ToolsOrder::Pinned);
        assert_eq!(anthropic.proxy.prompt_cache_key, PromptCacheKey::Client);
        let openai = parsed.for_provider("openai").expect("openai has overrides");
        assert_eq!(openai.proxy.prompt_cache_key, PromptCacheKey::Conversation);
        assert!(!openai.proxy.sticky_beta);

        assert!(toml::from_str::<Config>("[proxy]\ntools_order = \"sorted\"\n").is_err());
    }

    #[test]
    fn the_read_decisions_are_off_by_default_and_set_per_provider_or_from_the_environment() {
        let mut config = Config::default();
        assert!(!config.proxy.superseded_reads && !config.proxy.matured_reads);
        assert_eq!(
            (
                config.proxy.matured_quiesce_turns,
                config.proxy.matured_max_hold_turns,
                config.proxy.matured_min_bytes
            ),
            (5, 25, 2_048)
        );
        config.apply_env(&[
            ("KOMPRESSOR__PROXY__SUPERSEDED_READS".into(), "true".into()),
            ("KOMPRESSOR__PROXY__MATURED_READS".into(), "true".into()),
            (
                "KOMPRESSOR__PROXY__MATURED_QUIESCE_TURNS".into(),
                "8".into(),
            ),
            (
                "KOMPRESSOR__PROXY__MATURED_MAX_HOLD_TURNS".into(),
                "40".into(),
            ),
            ("KOMPRESSOR__PROXY__MATURED_MIN_BYTES".into(), "4096".into()),
        ]);
        assert!(config.proxy.superseded_reads && config.proxy.matured_reads);
        assert_eq!(
            (
                config.proxy.matured_quiesce_turns,
                config.proxy.matured_max_hold_turns,
                config.proxy.matured_min_bytes
            ),
            (8, 40, 4_096)
        );
        // Not a number: left alone.
        config.apply_env(&[(
            "KOMPRESSOR__PROXY__MATURED_QUIESCE_TURNS".into(),
            "soon".into(),
        )]);
        assert_eq!(config.proxy.matured_quiesce_turns, 8);
        assert!(config.validate().is_ok());
        // Zero turns is no wait at all, and is refused.
        config.proxy.matured_quiesce_turns = 0;
        assert!(config.validate().is_err());

        let parsed: Config = toml::from_str(
            "[proxy.providers.anthropic]\nsuperseded_reads = true\nmatured_reads = true\n",
        )
        .unwrap();
        assert!(!parsed.proxy.matured_reads);
        let anthropic = parsed
            .for_provider("anthropic")
            .expect("anthropic has overrides");
        assert!(anthropic.proxy.superseded_reads && anthropic.proxy.matured_reads);
    }

    #[test]
    fn cold_recompaction_is_off_by_default_and_set_per_provider_or_from_the_environment() {
        let mut config = Config::default();
        assert_eq!(config.proxy.cold_recompact, ColdRecompact::Off);
        assert_eq!(config.proxy.cold_recompact_margin, Duration::from_secs(60));

        config.apply_env(&[
            (
                "KOMPRESSOR__PROXY__COLD_RECOMPACT".into(),
                "predicted".into(),
            ),
            (
                "KOMPRESSOR__PROXY__COLD_RECOMPACT_MARGIN".into(),
                "2m".into(),
            ),
        ]);
        assert_eq!(config.proxy.cold_recompact, ColdRecompact::Predicted);
        assert_eq!(config.proxy.cold_recompact_margin, Duration::from_secs(120));
        // A typo leaves the setting alone.
        config.apply_env(&[("KOMPRESSOR__PROXY__COLD_RECOMPACT".into(), "always".into())]);
        assert_eq!(config.proxy.cold_recompact, ColdRecompact::Predicted);

        let parsed: Config =
            toml::from_str("[proxy.providers.anthropic]\ncold_recompact = \"observed\"\n").unwrap();
        assert_eq!(parsed.proxy.cold_recompact, ColdRecompact::Off);
        let anthropic = parsed
            .for_provider("anthropic")
            .expect("anthropic has overrides");
        assert_eq!(anthropic.proxy.cold_recompact, ColdRecompact::Observed);
        assert!(toml::from_str::<Config>("[proxy]\ncold_recompact = \"always\"\n").is_err());
    }

    #[test]
    fn prompt_cache_retention_is_off_by_default_and_set_per_provider_or_from_the_environment() {
        let mut config = Config::default();
        assert_eq!(
            config.proxy.prompt_cache_retention,
            PromptCacheRetention::Default
        );
        config.apply_env(&[(
            "KOMPRESSOR__PROXY__PROMPT_CACHE_RETENTION".into(),
            "24h".into(),
        )]);
        assert_eq!(
            config.proxy.prompt_cache_retention,
            PromptCacheRetention::Extended
        );
        // A typo changes nothing: it is what the provider is asked to store.
        config.apply_env(&[(
            "KOMPRESSOR__PROXY__PROMPT_CACHE_RETENTION".into(),
            "1d".into(),
        )]);
        assert_eq!(
            config.proxy.prompt_cache_retention,
            PromptCacheRetention::Extended
        );

        let parsed: Config =
            toml::from_str("[proxy.providers.openai]\nprompt_cache_retention = \"24h\"\n")
                .expect("parses");
        assert_eq!(
            parsed.proxy.prompt_cache_retention,
            PromptCacheRetention::Default
        );
        let openai = parsed.for_provider("openai").expect("openai has overrides");
        assert_eq!(
            openai.proxy.prompt_cache_retention,
            PromptCacheRetention::Extended
        );
        assert!(parsed.for_provider("anthropic").is_none());
        assert!(toml::from_str::<Config>("[proxy]\nprompt_cache_retention = \"12h\"\n").is_err());
    }

    #[test]
    fn the_cache_ttl_setting_can_be_set_from_the_environment_and_a_typo_changes_nothing() {
        let mut config = Config::default();
        assert_eq!(config.proxy.cache_ttl, CacheTtl::Default);
        config.apply_env(&[("KOMPRESSOR__PROXY__CACHE_TTL".into(), "adaptive".into())]);
        assert_eq!(config.proxy.cache_ttl, CacheTtl::Adaptive);
        config.apply_env(&[("KOMPRESSOR__PROXY__CACHE_TTL".into(), "1h".into())]);
        assert_eq!(config.proxy.cache_ttl, CacheTtl::OneHour);
        // It changes what the provider bills, so an unknown value is ignored
        // rather than read as any of the three.
        config.apply_env(&[("KOMPRESSOR__PROXY__CACHE_TTL".into(), "60m".into())]);
        assert_eq!(config.proxy.cache_ttl, CacheTtl::OneHour);
        let parsed: Config = toml::from_str("[proxy]\ncache_ttl = \"1h\"\n").expect("parses");
        assert_eq!(parsed.proxy.cache_ttl, CacheTtl::OneHour);
    }

    #[test]
    fn output_shaping_is_off_by_default_and_its_holdout_cannot_fall_below_thirty() {
        let mut config = Config::default();
        assert!(!config.proxy.output_shaping);
        assert_eq!(config.proxy.output_shaping_holdout_pct, 50);
        assert_eq!(
            config.proxy.output_shaping_text,
            DEFAULT_OUTPUT_SHAPING_TEXT
        );
        config.validate().expect("the defaults are valid");
        config.apply_env(&[
            ("KOMPRESSOR__PROXY__OUTPUT_SHAPING".into(), "true".into()),
            (
                "KOMPRESSOR__PROXY__OUTPUT_SHAPING_HOLDOUT_PCT".into(),
                "35".into(),
            ),
            (
                "KOMPRESSOR__PROXY__OUTPUT_SHAPING_TEXT".into(),
                "Be brief.".into(),
            ),
        ]);
        assert!(config.proxy.output_shaping);
        assert_eq!(config.proxy.output_shaping_holdout_pct, 35);
        assert_eq!(config.proxy.output_shaping_text, "Be brief.");
        config.validate().expect("35% is enough");
        for pct in [0u8, 10, 29, 101, 255] {
            config.proxy.output_shaping_holdout_pct = pct;
            assert!(config.validate().is_err(), "{pct}");
        }
        config.proxy.output_shaping_holdout_pct = 30;
        config.validate().expect("30% is the floor");
        config.proxy.output_shaping_text = "  ".into();
        assert!(config.validate().is_err(), "an empty instruction");
        let parsed: Config = toml::from_str(
            "[proxy]\noutput_shaping = true\noutput_shaping_holdout_pct = 40\n\
             output_shaping_text = \"Short answers.\"\n",
        )
        .expect("parses");
        assert!(parsed.proxy.output_shaping);
        assert_eq!(parsed.proxy.output_shaping_holdout_pct, 40);
        assert_eq!(parsed.proxy.output_shaping_text, "Short answers.");
    }

    #[test]
    fn engine_borrows_from_headroom_are_reachable_from_the_environment() {
        let mut config = Config::default();
        assert!(config.engine.enable_runtime_frame_collapse);
        assert!(config.engine.keep_test_totals);
        assert!(!config.engine.enable_json_csv);
        assert_eq!(config.engine.protect_tool_error_chars, 2_000);
        config.apply_env(&[
            (
                "KOMPRESSOR__ENGINE__ENABLE_RUNTIME_FRAME_COLLAPSE".into(),
                "false".into(),
            ),
            (
                "KOMPRESSOR__ENGINE__KEEP_TEST_TOTALS".into(),
                "false".into(),
            ),
            ("KOMPRESSOR__ENGINE__ENABLE_JSON_CSV".into(), "true".into()),
            (
                "KOMPRESSOR__ENGINE__PROTECT_TOOL_ERROR_CHARS".into(),
                "0".into(),
            ),
        ]);
        assert!(!config.engine.enable_runtime_frame_collapse);
        assert!(!config.engine.keep_test_totals);
        assert!(config.engine.enable_json_csv);
        assert_eq!(config.engine.protect_tool_error_chars, 0);
    }

    #[test]
    fn keep_warm_is_on_by_default_and_reachable_from_the_environment() {
        let mut config = Config::default();
        assert!(config.proxy.cache_keep_warm);
        assert_eq!(
            config.proxy.cache_keep_warm_interval,
            Duration::from_secs(270)
        );
        assert_eq!(config.proxy.cache_keep_warm_max_pings, 11);
        assert_eq!(config.proxy.cache_keep_warm_background_pings, 4);
        config.apply_env(&[("KOMPRESSOR__PROXY__CACHE_KEEP_WARM".into(), "false".into())]);
        assert!(!config.proxy.cache_keep_warm);
        config.apply_env(&[
            ("KOMPRESSOR__PROXY__CACHE_KEEP_WARM".into(), "true".into()),
            (
                "KOMPRESSOR__PROXY__CACHE_KEEP_WARM_INTERVAL".into(),
                "4m".into(),
            ),
            (
                "KOMPRESSOR__PROXY__CACHE_KEEP_WARM_MAX_PINGS".into(),
                "5".into(),
            ),
        ]);
        assert!(config.proxy.cache_keep_warm);
        assert_eq!(
            config.proxy.cache_keep_warm_interval,
            Duration::from_secs(240)
        );
        assert_eq!(config.proxy.cache_keep_warm_max_pings, 5);
        assert_eq!(config.proxy.cache_keep_warm_background_pings, 4);
        config.apply_env(&[(
            "KOMPRESSOR__PROXY__CACHE_KEEP_WARM_BACKGROUND_PINGS".into(),
            "0".into(),
        )]);
        assert_eq!(config.proxy.cache_keep_warm_background_pings, 0);
        assert_eq!(config.proxy.cache_keep_warm_max_conversations, 1_024);
        config.apply_env(&[(
            "KOMPRESSOR__PROXY__CACHE_KEEP_WARM_MAX_CONVERSATIONS".into(),
            "2".into(),
        )]);
        assert_eq!(config.proxy.cache_keep_warm_max_conversations, 2);
        config.apply_env(&[(
            "KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__CACHE_KEEP_WARM".into(),
            "false".into(),
        )]);
        let anthropic = config.for_provider("anthropic").expect("scoped");
        assert!(!anthropic.proxy.cache_keep_warm, "the provider table wins");
        assert!(config.proxy.cache_keep_warm, "and changes nothing else");
    }

    #[test]
    fn the_retrieval_economics_settings_default_off_and_are_reachable() {
        let mut config = Config::default();
        assert!(!config.proxy.retrieve_attach_siblings);
        assert_eq!(config.proxy.elision_gate, ElisionGate::Fixed);
        assert_eq!(config.proxy.net_cost_gate.fetch_probability, 0.30);
        config.apply_env(&[
            (
                "KOMPRESSOR__PROXY__RETRIEVE_ATTACH_SIBLINGS".into(),
                "true".into(),
            ),
            ("KOMPRESSOR__PROXY__ELISION_GATE".into(), "net_cost".into()),
            (
                "KOMPRESSOR__PROXY__NET_COST_GATE__EXPECTED_READS".into(),
                "25".into(),
            ),
            (
                "KOMPRESSOR__PROXY__NET_COST_GATE__FETCH_PROBABILITY".into(),
                "0.2".into(),
            ),
        ]);
        assert!(config.proxy.retrieve_attach_siblings);
        assert_eq!(config.proxy.elision_gate, ElisionGate::NetCost);
        assert_eq!(config.proxy.net_cost_gate.expected_reads, 25.0);
        assert_eq!(config.proxy.net_cost_gate.fetch_probability, 0.2);
        // A typo leaves the gate as it was.
        config.apply_env(&[("KOMPRESSOR__PROXY__ELISION_GATE".into(), "netcost".into())]);
        assert_eq!(config.proxy.elision_gate, ElisionGate::NetCost);

        let parsed: Config = toml::from_str(
            "[proxy]\nelision_gate = \"net_cost\"\n\
             [proxy.net_cost_gate]\nexpected_reads = 4\n\
             [[proxy.net_cost_gate.fetch_table]]\nkind = \"log\"\nbelow_saved_chars = 4000\np = 0.3\n\
             [proxy.providers.openai]\nelision_gate = \"fixed\"\n",
        )
        .expect("parses");
        assert_eq!(parsed.proxy.elision_gate, ElisionGate::NetCost);
        assert_eq!(parsed.proxy.net_cost_gate.expected_reads, 4.0);
        assert_eq!(parsed.proxy.net_cost_gate.fetch_table[0].kind, "log");
        let openai = parsed.for_provider("openai").expect("scoped");
        assert_eq!(openai.proxy.elision_gate, ElisionGate::Fixed);
        assert!(parsed.validate().is_ok());

        let mut broken = parsed.clone();
        broken.proxy.net_cost_gate.fetch_probability = 2.0;
        assert!(
            broken.validate().is_err(),
            "a probability above one is refused"
        );
    }

    #[test]
    fn a_keep_warm_interval_the_cache_would_not_survive_is_rejected() {
        for interval in [Duration::ZERO, Duration::from_secs(300)] {
            let mut config = Config::default();
            config.proxy.cache_keep_warm_interval = interval;
            assert!(config.validate().is_err(), "{interval:?}");
        }
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn the_background_prior_and_the_hour_ping_are_off_and_reachable_from_the_environment() {
        let mut config = Config::default();
        assert!(!config.proxy.cache_keep_warm_background_prior);
        assert!(!config.proxy.cache_keep_warm_one_hour);
        assert_eq!(
            config.proxy.cache_keep_warm_one_hour_delay,
            Duration::from_secs(3_300)
        );
        config.apply_env(&[
            (
                "KOMPRESSOR__PROXY__CACHE_KEEP_WARM_BACKGROUND_PRIOR".into(),
                "true".into(),
            ),
            (
                "KOMPRESSOR__PROXY__CACHE_KEEP_WARM_ONE_HOUR".into(),
                "true".into(),
            ),
            (
                "KOMPRESSOR__PROXY__CACHE_KEEP_WARM_ONE_HOUR_DELAY".into(),
                "50m".into(),
            ),
        ]);
        assert!(config.proxy.cache_keep_warm_background_prior);
        assert!(config.proxy.cache_keep_warm_one_hour);
        assert_eq!(
            config.proxy.cache_keep_warm_one_hour_delay,
            Duration::from_secs(3_000)
        );
        config.apply_env(&[(
            "KOMPRESSOR__PROXY__CACHE_KEEP_WARM_ONE_HOUR_DELAY".into(),
            "soon".into(),
        )]);
        assert_eq!(
            config.proxy.cache_keep_warm_one_hour_delay,
            Duration::from_secs(3_000),
            "an unreadable delay leaves the last one standing"
        );
        let parsed: Config = toml::from_str(
            "[proxy]\ncache_keep_warm_one_hour = true\ncache_keep_warm_one_hour_delay = \"58m\"\n",
        )
        .expect("parses");
        assert!(parsed.proxy.cache_keep_warm_one_hour);
        assert_eq!(
            parsed.proxy.cache_keep_warm_one_hour_delay,
            Duration::from_secs(58 * 60)
        );
    }

    #[test]
    fn a_one_hour_ping_delay_the_entry_would_not_survive_is_rejected() {
        for delay in [Duration::ZERO, Duration::from_secs(3_600)] {
            let mut config = Config::default();
            config.proxy.cache_keep_warm_one_hour_delay = delay;
            assert!(config.validate().is_err(), "{delay:?}");
        }
        let mut config = Config::default();
        config.proxy.cache_keep_warm_one_hour_delay = Duration::from_secs(3_599);
        assert!(config.validate().is_ok());
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

    /// Reproduced in the 2026-11-11 review through `check-config`: the flat
    /// spelling set *after* the structured one won. Both orders are checked,
    /// because one favourable ordering is all a single-pass overlay needs to
    /// pass.
    #[test]
    fn the_structured_spelling_wins_in_either_order() {
        let structured = (
            "KOMPRESSOR__ENGINE__MAX_ARRAY_ITEMS".to_string(),
            "7".to_string(),
        );
        let flat = ("KOMPRESSOR_MAX_ARRAY_ITEMS".to_string(), "25".to_string());
        for vars in [
            vec![structured.clone(), flat.clone()],
            vec![flat.clone(), structured.clone()],
        ] {
            let mut config = Config::default();
            config.apply_env(&vars);
            assert_eq!(config.engine.max_array_items, 7, "order {vars:?}");
        }

        // The same holds for a setting that lives outside the engine section.
        let structured = (
            "KOMPRESSOR__PROXY__MIN_TOKENS".to_string(),
            "300".to_string(),
        );
        let flat = ("KOMPRESSOR_MIN_TOKENS".to_string(), "900".to_string());
        for vars in [
            vec![structured.clone(), flat.clone()],
            vec![flat.clone(), structured.clone()],
        ] {
            let mut config = Config::default();
            config.apply_env(&vars);
            assert_eq!(config.proxy.min_tokens, 300, "order {vars:?}");
        }
    }

    /// The shared engine permit is set per deployment as often as any listen
    /// address, now that it bounds the proxy too.
    #[test]
    fn the_engine_permit_is_reachable_from_the_environment() {
        let mut config = Config::default();
        config.apply_env(&[
            (
                "KOMPRESSOR__SERVER__MAX_INFLIGHT_COMPRESSIONS".into(),
                "1".into(),
            ),
            ("KOMPRESSOR__SERVER__QUEUE_TIMEOUT".into(), "250ms".into()),
        ]);
        assert_eq!(config.server.max_inflight_compressions, 1);
        assert_eq!(config.server.inflight_limit(), 1);
        assert_eq!(config.server.queue_timeout, Duration::from_millis(250));
        config.apply_env(&[(
            "KOMPRESSOR__SERVER__MAX_INFLIGHT_COMPRESSIONS".into(),
            "lots".into(),
        )]);
        assert_eq!(
            config.server.max_inflight_compressions, 1,
            "unreadable: ignored"
        );
    }

    /// A structured value its setting cannot read leaves the flat one in
    /// force, rather than reverting the setting to its default.
    #[test]
    fn an_unreadable_structured_value_leaves_the_flat_one_in_force() {
        let mut config = Config::default();
        config.apply_env(&[
            ("KOMPRESSOR__ENGINE__MAX_ARRAY_ITEMS".into(), "many".into()),
            ("KOMPRESSOR_MAX_ARRAY_ITEMS".into(), "25".into()),
        ]);
        assert_eq!(config.engine.max_array_items, 25);
    }

    #[test]
    fn an_empty_environment_value_does_not_override() {
        let mut config = Config::default();
        let before = config.engine.max_array_items;
        config.apply_env(&[("KOMPRESSOR_MAX_ARRAY_ITEMS".into(), String::new())]);
        assert_eq!(config.engine.max_array_items, before);
    }

    /// A mistyped value used to be ignored without a trace; the record says
    /// which variables changed a setting and which did not, by name alone.
    #[test]
    fn what_each_environment_variable_did_is_on_record() {
        let mut config = Config::default();
        config.apply_env(&[
            ("PATH".into(), "/usr/bin".into()),
            (
                "KOMPRESSOR__PROXY__COLD_RECOMPACT".into(),
                "predicted".into(),
            ),
            ("KOMPRESSOR__PROXY__CACHE_KEEP_WARM".into(), "treu".into()),
            ("KOMPRESSOR__PROXY__RETRIEVE_TOOOL".into(), "always".into()),
            ("KOMPRESSOR__ENGINE__MAX_ARRAY_ITEMS".into(), String::new()),
            ("KOMPRESSOR_MAX_ARRAY_ITEMS".into(), "7".into()),
            (
                "KOMPRESSOR_CONFIG".into(),
                "/etc/kompressor/config.toml".into(),
            ),
            ("KOMPRESSOR__AUTH__MODE".into(), "apikeys?".into()),
        ]);
        let effect = |name: &str| {
            let found: Vec<EnvEffect> = config
                .env
                .iter()
                .filter(|one| one.name == name)
                .map(|one| one.effect)
                .collect();
            assert_eq!(found.len(), 1, "{name}: {:?}", config.env);
            found[0]
        };
        assert_eq!(config.env.len(), 7, "only this service's variables");
        assert_eq!(
            effect("KOMPRESSOR__PROXY__COLD_RECOMPACT"),
            EnvEffect::Applied
        );
        assert_eq!(effect("KOMPRESSOR_MAX_ARRAY_ITEMS"), EnvEffect::Applied);
        assert_eq!(
            effect("KOMPRESSOR__PROXY__CACHE_KEEP_WARM"),
            EnvEffect::NoEffect
        );
        assert_eq!(
            effect("KOMPRESSOR__PROXY__RETRIEVE_TOOOL"),
            EnvEffect::NoEffect
        );
        assert_eq!(
            effect("KOMPRESSOR__ENGINE__MAX_ARRAY_ITEMS"),
            EnvEffect::NoEffect
        );
        assert_eq!(effect("KOMPRESSOR_CONFIG"), EnvEffect::Elsewhere);
        // Refused, which stops the server: it did something.
        assert_eq!(effect("KOMPRESSOR__AUTH__MODE"), EnvEffect::Applied);
        let text = toml::to_string(&config).expect("toml");
        assert!(!text.contains("KOMPRESSOR"), "the record is not a setting");
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

    /// `KOMPRESSOR__AUTH__MODE=api-keys` used to be ignored without a word,
    /// leaving `none` in force: the server came up serving everyone as the
    /// anonymous tenant. An auth mode that cannot be read is now an error at
    /// validation, and the common spellings are understood.
    #[test]
    fn an_unreadable_auth_mode_in_the_environment_stops_the_server() {
        let env = |value: &str| {
            let mut config = Config::default();
            config.apply_env(&[("KOMPRESSOR__AUTH__MODE".into(), value.into())]);
            config
        };
        for spelling in ["api_keys", "API_KEYS", "api-keys", "ApiKeys"] {
            assert_eq!(env(spelling).auth.mode, AuthMode::ApiKeys, "{spelling}");
        }
        assert_eq!(env("none").auth.mode, AuthMode::None);
        assert!(env("none").validate().is_ok());

        for typo in ["api_key", "keys", "on", "true", "nonee"] {
            let config = env(typo);
            let error = config.validate().expect_err(typo).to_string();
            assert!(
                error.contains("auth.mode") && error.contains(typo),
                "{error}"
            );
        }

        // The environment can also turn a file's `api_keys` off, explicitly.
        let mut config = Config::default();
        config.auth.mode = AuthMode::ApiKeys;
        config.apply_env(&[("KOMPRESSOR__AUTH__MODE".into(), "none".into())]);
        assert_eq!(config.auth.mode, AuthMode::None);
    }

    #[test]
    fn password_only_redis_credentials_are_redacted() {
        let mut config = Config::default();
        config.refs.redis_url = "redis://secret-password@cache/0".into();
        assert!(!config.redacted_toml().contains("secret-password"));
    }

    #[test]
    fn a_redis_store_is_validated_at_load_and_its_password_redacted() {
        let ok: Config = toml::from_str(
            "[refs]\nbackend = \"redis\"\nredis_url = \"redis://app:s3cret@cache:6380/2\"\n",
        )
        .unwrap();
        ok.validate().expect("a redis:// url is accepted");
        let rendered = ok.redacted_toml();
        assert!(!rendered.contains("s3cret"), "{rendered}");
        assert!(rendered.contains("redis://app:<redacted>@cache:6380/2"));

        for url in ["rediss://cache:6380", "http://cache", "redis://cache/x"] {
            let mut bad = ok.clone();
            bad.refs.redis_url = url.into();
            let error = bad.validate().unwrap_err().to_string();
            assert!(error.contains("refs.redis_url"), "{url}: {error}");
        }
        // Not read unless the backend is redis.
        let mut sqlite = ok.clone();
        sqlite.refs.backend = RefsBackend::Sqlite;
        sqlite.refs.redis_url = "nonsense".into();
        sqlite.validate().expect("ignored for sqlite");
    }

    /// The same for the fragment store: a misspelt `memory` kept SQLite, and
    /// fragments the operator wanted held in memory were written to disk.
    #[test]
    fn an_unreadable_refs_backend_in_the_environment_stops_the_server() {
        let env = |value: &str| {
            let mut config = Config::default();
            config.apply_env(&[("KOMPRESSOR__REFS__BACKEND".into(), value.into())]);
            config
        };
        assert_eq!(env("memory").refs.backend, RefsBackend::Memory);
        assert_eq!(env("Memory").refs.backend, RefsBackend::Memory);
        assert_eq!(env("sqlite").refs.backend, RefsBackend::Sqlite);
        assert_eq!(env("redis").refs.backend, RefsBackend::Redis);
        assert!(env("sqlite").validate().is_ok());
        let error = env("ephemeral").validate().unwrap_err().to_string();
        assert!(
            error.contains("refs.backend") && error.contains("ephemeral"),
            "{error}"
        );
    }

    /// The refusals never reach a file or the fingerprint of a configuration.
    #[test]
    fn refused_overrides_are_not_part_of_the_serialised_configuration() {
        let mut config = Config::default();
        config.apply_env(&[("KOMPRESSOR__AUTH__MODE".into(), "bogus".into())]);
        let text = toml::to_string(&config).unwrap();
        assert!(
            !text.contains("refused_env") && !text.contains("bogus"),
            "{text}"
        );
    }

    #[test]
    fn an_unknown_key_in_a_config_file_is_an_error_not_a_silent_typo() {
        let text = "[engine]\nmax_array_itemz = 5\n";
        assert!(toml::from_str::<Config>(text).is_err());
    }
    #[test]
    fn the_statistics_listener_is_off_until_a_file_or_the_environment_names_it() {
        assert!(Config::default().server.stats_listen.is_empty());
        let from_file: Config =
            toml::from_str("[server]\nstats_listen = \"127.0.0.1:8818\"\n").expect("parses");
        assert_eq!(from_file.server.stats_listen, "127.0.0.1:8818");
        let mut config = Config::default();
        config.apply_env(&[(
            "KOMPRESSOR__SERVER__STATS_LISTEN".into(),
            "0.0.0.0:9000".into(),
        )]);
        assert_eq!(config.server.stats_listen, "0.0.0.0:9000");
    }

    #[test]
    fn backup_over_http_is_off_until_a_file_or_the_environment_turns_it_on() {
        assert!(!Config::default().maintenance.backup);
        let from_file: Config = toml::from_str("[maintenance]\nbackup = true\n").expect("parses");
        assert!(from_file.maintenance.backup);
        let mut config = Config::default();
        config.apply_env(&[("KOMPRESSOR__MAINTENANCE__BACKUP".into(), "true".into())]);
        assert!(config.maintenance.backup);
        assert!(toml::from_str::<Config>("[maintenance]\nbackups = true\n").is_err());
    }

    #[test]
    fn scheduled_backups_need_a_directory_and_are_off_without_one() {
        use std::path::Path;
        let config = Config::default();
        assert_eq!(
            config.maintenance.schedule(),
            None,
            "no directory, no schedule"
        );
        assert!(!config.maintenance.actions);
        assert!(!config.maintenance.erase, "nothing is erased by default");
        assert_eq!(
            config.maintenance.optimize_every,
            Duration::from_secs(24 * 3_600),
            "the planner's figures are refreshed daily"
        );
        assert_eq!(
            config.stats.context_retention_days, 0,
            "kept with the request"
        );
        assert_eq!(config.maintenance.backup_keep, 7);

        let config: Config = toml::from_str(
            "[maintenance]\nbackup_dir = \"/var/lib/kompressor/backups\"\nbackup_every = \"6h\"\nbackup_keep = 3\nactions = true\n",
        )
        .expect("parses");
        config.validate().expect("valid");
        assert_eq!(
            config.maintenance.schedule(),
            Some((
                Path::new("/var/lib/kompressor/backups"),
                Duration::from_secs(6 * 3_600)
            ))
        );
        assert_eq!(config.maintenance.backup_keep, 3);
        assert!(config.maintenance.actions);

        // A directory with the interval set to zero is a place for manual
        // backups, not a schedule.
        let mut manual = config.clone();
        manual.maintenance.backup_every = Duration::ZERO;
        assert_eq!(manual.maintenance.schedule(), None);
        manual.validate().expect("valid");
        // Faster than a backup can be taken is a mistake, not a schedule.
        let mut hasty = config.clone();
        hasty.maintenance.backup_every = Duration::from_secs(5);
        let error = hasty.validate().expect_err("refused").to_string();
        assert!(error.contains("maintenance.backup_every"), "{error}");

        let mut env = Config::default();
        env.apply_env(&[
            (
                "KOMPRESSOR__MAINTENANCE__BACKUP_DIR".into(),
                "/srv/b".into(),
            ),
            ("KOMPRESSOR__MAINTENANCE__BACKUP_EVERY".into(), "12h".into()),
            ("KOMPRESSOR__MAINTENANCE__BACKUP_KEEP".into(), "0".into()),
            ("KOMPRESSOR__MAINTENANCE__ACTIONS".into(), "true".into()),
            ("KOMPRESSOR__MAINTENANCE__ERASE".into(), "true".into()),
            (
                "KOMPRESSOR__MAINTENANCE__OPTIMIZE_EVERY".into(),
                "6h".into(),
            ),
            ("KOMPRESSOR__STATS__RETENTION_DAYS".into(), "30".into()),
            (
                "KOMPRESSOR__STATS__CONTEXT_RETENTION_DAYS".into(),
                "7".into(),
            ),
        ]);
        assert_eq!(
            env.maintenance.optimize_every,
            Duration::from_secs(6 * 3_600)
        );
        assert_eq!(env.stats.retention_days, 30);
        assert_eq!(env.stats.context_retention_days, 7);
        assert_eq!(
            env.maintenance.schedule(),
            Some((Path::new("/srv/b"), Duration::from_secs(12 * 3_600)))
        );
        assert_eq!(env.maintenance.backup_keep, 0);
        assert!(env.maintenance.actions);
        assert!(env.maintenance.erase);
    }

    #[test]
    fn the_age_a_backup_may_reach_follows_the_schedule_unless_it_is_set() {
        let mut config = Config::default();
        assert_eq!(config.maintenance.max_age(), None, "no backups asked for");
        config.maintenance.backup_dir = PathBuf::from("/srv/b");
        assert_eq!(
            config.maintenance.max_age(),
            Some(Duration::from_secs(2 * 24 * 3_600)),
            "one missed daily backup"
        );
        config.maintenance.backup_max_age = Duration::from_secs(3_600);
        assert_eq!(
            config.maintenance.max_age(),
            Some(Duration::from_secs(3_600))
        );
        // Held to an age with no schedule: someone else takes them.
        let alone: Config = toml::from_str("[maintenance]\nbackup_max_age = \"7days\"\n").unwrap();
        assert_eq!(alone.maintenance.schedule(), None);
        assert_eq!(
            alone.maintenance.max_age(),
            Some(Duration::from_secs(7 * 86_400))
        );
        let mut env = Config::default();
        env.apply_env(&[(
            "KOMPRESSOR__MAINTENANCE__BACKUP_MAX_AGE".into(),
            "36h".into(),
        )]);
        assert_eq!(
            env.maintenance.max_age(),
            Some(Duration::from_secs(36 * 3_600))
        );
    }

    #[test]
    fn a_loaded_configuration_remembers_its_file() {
        assert_eq!(Config::default().source, None);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k.toml");
        std::fs::write(&path, "[stats]\nenabled = false\n").unwrap();
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(config.source.as_deref(), Some(path.as_path()));
        assert!(!toml::to_string(&config).unwrap().contains("source"));
    }

    #[test]
    fn the_ref_browser_is_off_until_a_file_or_the_environment_turns_it_on() {
        assert!(!Config::default().refs.browse);
        let from_file: Config = toml::from_str("[refs]\nbrowse = true\n").expect("parses");
        assert!(from_file.refs.browse);
        let mut config = Config::default();
        config.apply_env(&[("KOMPRESSOR__REFS__BROWSE".into(), "true".into())]);
        assert!(config.refs.browse);
    }

    #[test]
    fn the_off_site_copy_is_off_by_default_and_set_from_a_file_or_the_environment() {
        assert!(!Config::default().maintenance.s3.enabled);
        let from_file: Config = toml::from_str(
            "[maintenance.s3]\nenabled = true\nbucket = \"b\"\naccess_key_id = \"k\"\nsecret_access_key = \"s\"\nkeep = 3\n",
        )
        .expect("parses");
        assert!(from_file.maintenance.s3.enabled);
        assert_eq!(from_file.maintenance.s3.keep, 3);
        assert_eq!(from_file.maintenance.s3.location(), "s3://b/kompressor/");
        assert!(from_file.validate().is_ok());
        assert!(toml::from_str::<Config>("[maintenance.s3]\nbuckets = \"x\"\n").is_err());

        let mut config = Config::default();
        config.apply_env(&[
            ("KOMPRESSOR__MAINTENANCE__S3__ENABLED".into(), "true".into()),
            (
                "KOMPRESSOR__MAINTENANCE__S3__BUCKET".into(),
                "env-bucket".into(),
            ),
            (
                "KOMPRESSOR__MAINTENANCE__S3__ENDPOINT".into(),
                "http://minio:9000".into(),
            ),
            (
                "KOMPRESSOR__MAINTENANCE__S3__ACCESS_KEY_ID".into(),
                "k".into(),
            ),
            (
                "KOMPRESSOR__MAINTENANCE__S3__SECRET_ACCESS_KEY".into(),
                "s".into(),
            ),
            ("KOMPRESSOR__MAINTENANCE__S3__KEEP".into(), "5".into()),
            (
                "KOMPRESSOR__MAINTENANCE__S3__PATH_STYLE".into(),
                "false".into(),
            ),
        ]);
        let s3 = &config.maintenance.s3;
        assert!(s3.enabled && !s3.path_style);
        assert_eq!(
            (s3.bucket.as_str(), s3.endpoint.as_str(), s3.keep),
            ("env-bucket", "http://minio:9000", 5)
        );
        assert!(config.validate().is_ok());
        assert!(
            config.env.iter().all(|o| o.effect == EnvEffect::Applied),
            "every variable did something: {:?}",
            config.env
        );
    }

    #[test]
    fn an_off_site_copy_that_cannot_work_stops_the_service_from_starting() {
        let mut config = Config::default();
        config.maintenance.s3.enabled = true;
        let error = config.validate().expect_err("no bucket").to_string();
        assert!(error.contains("maintenance.s3.bucket"), "{error}");
        // Off, the same settings are nobody's business.
        config.maintenance.s3.enabled = false;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn the_s3_secret_key_is_never_shown() {
        let mut config = Config::default();
        config.maintenance.s3.secret_access_key = "wJalrXUtnFEMI-secret".into();
        config.maintenance.s3.access_key_id = "AKIAEXAMPLE".into();
        let rendered = config.redacted_toml();
        assert!(!rendered.contains("wJalrXUtnFEMI-secret"), "{rendered}");
        assert!(rendered.contains("<redacted>"));
        assert!(!format!("{config:?}").contains("wJalrXUtnFEMI-secret"));
    }
}
