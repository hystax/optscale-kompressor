//! What the Maintenance page says is running, beyond the build: where the
//! process listens, how it is set to treat a request, the process itself and
//! what it is in the middle of.
//!
//! Nothing here is a secret or a tenant's: no key, no header value, and an
//! upstream's address with any credentials in it taken out.

use kompressor_core::AppCore;
use kompressor_core::config::{AuthMode, Config, PROVIDERS};
use kompressor_core::runtime::Listener;
use serde::Serialize;
use serde_json::Value;

/// The proxy settings that are off, or at a conservative value, unless a
/// deployment chose otherwise: the ones worth seeing at a glance.
const OPT_IN: [&str; 13] = [
    "compress",
    "retrieve_tool",
    "frozen_prefix",
    "cache_ttl",
    "cache_keep_warm",
    "prompt_cache_retention",
    "prompt_cache_key",
    "tools_order",
    "sticky_beta",
    "tools_compact",
    "cold_recompact",
    "superseded_reads",
    "matured_reads",
];

#[derive(Debug, Serialize)]
pub(crate) struct Running {
    /// Every address the process is bound to. Empty when whoever started it
    /// did not say — a test, or an embedding that binds for itself.
    listeners: Vec<Listener>,
    /// The engine's mode for a request that does not choose one.
    mode: String,
    auth: Auth,
    /// Whether the proxy surface is configured at all.
    proxy_enabled: bool,
    upstreams: Vec<Upstream>,
    /// Opt-in settings that differ from what ships.
    features: Vec<Feature>,
    process: Process,
    load: Load,
}

#[derive(Debug, Serialize)]
struct Auth {
    /// `none` or `api_keys`.
    mode: &'static str,
    /// How many keys are configured. Never the keys.
    keys: usize,
    /// Whether a shared secret for trusted callers is set.
    secret: bool,
}

#[derive(Debug, Serialize)]
struct Upstream {
    name: String,
    path_prefix: String,
    base_url: String,
    api_family: String,
}

/// One setting that is not at its shipped value.
#[derive(Debug, PartialEq, Eq, Serialize)]
struct Feature {
    /// `all` for the deployment's own value, else the provider it is set for.
    scope: String,
    setting: String,
    value: String,
}

/// The process, as the operating system sees it. Each figure is absent where
/// this platform does not offer it.
#[derive(Debug, Default, Serialize)]
struct Process {
    pid: u32,
    user: Option<String>,
    memory_bytes: Option<u64>,
    open_files: Option<u64>,
    open_files_limit: Option<u64>,
}

/// What the process is in the middle of: the answer to "may I restart it".
#[derive(Debug, Default, Serialize)]
pub(crate) struct Load {
    /// Proxied requests taken and not yet answered in full.
    pub(crate) requests: usize,
    /// Those of them that are streams.
    pub(crate) streams: usize,
    /// Engine slots taken, of `engine_limit`.
    pub(crate) engine_busy: usize,
    pub(crate) engine_limit: usize,
    /// A backup archive is being written.
    pub(crate) backup_running: bool,
}

pub(crate) fn running(core: &AppCore, load: Load) -> Running {
    let config = core.config();
    Running {
        listeners: core.runtime().listeners(),
        mode: config.engine.mode.to_string(),
        auth: Auth {
            mode: match config.auth.mode {
                AuthMode::None => "none",
                AuthMode::ApiKeys => "api_keys",
            },
            keys: config.auth.keys.len(),
            secret: !config.auth.secret.is_empty(),
        },
        proxy_enabled: config.proxy.enabled,
        upstreams: config
            .proxy
            .upstreams
            .iter()
            .map(|upstream| Upstream {
                name: upstream.name.clone(),
                path_prefix: upstream.path_prefix.clone(),
                base_url: without_credentials(&upstream.base_url),
                api_family: upstream.api_family.clone(),
            })
            .collect(),
        features: features(core),
        process: process(),
        load,
    }
}

/// `base_url` with any `user:password@` taken out of it.
fn without_credentials(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => url.to_string(),
    }
}

fn shown(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// The opt-in settings that are not at their shipped value: the deployment's
/// against the defaults, then each provider's against the deployment's — its
/// engine overrides included, since a provider is the only place those show.
fn features(core: &AppCore) -> Vec<Feature> {
    let proxy = |config: &Config| serde_json::to_value(&config.proxy).unwrap_or(Value::Null);
    let engine = |config: &Config| serde_json::to_value(&config.engine).unwrap_or(Value::Null);
    let config = core.config();
    let (shipped, deployed) = (proxy(&Config::default()), proxy(config));
    let mut found = Vec::new();
    for setting in OPT_IN {
        if deployed[setting] != shipped[setting] && !deployed[setting].is_null() {
            found.push(Feature {
                scope: "all".into(),
                setting: setting.into(),
                value: shown(&deployed[setting]),
            });
        }
    }
    let deployed_engine = engine(config);
    for provider in PROVIDERS {
        if !config.proxy.providers.contains_key(*provider) {
            continue;
        }
        let theirs = core.config_for(Some(provider));
        let their_proxy = proxy(theirs);
        for setting in OPT_IN {
            if their_proxy[setting] != deployed[setting] && !their_proxy[setting].is_null() {
                found.push(Feature {
                    scope: (*provider).into(),
                    setting: setting.into(),
                    value: shown(&their_proxy[setting]),
                });
            }
        }
        if let (Value::Object(theirs), Value::Object(ours)) = (engine(theirs), &deployed_engine) {
            for (setting, value) in &theirs {
                if ours.get(setting) != Some(value) {
                    found.push(Feature {
                        scope: (*provider).into(),
                        setting: format!("engine.{setting}"),
                        value: shown(value),
                    });
                }
            }
        }
    }
    found
}

fn process() -> Process {
    let mut process = Process {
        pid: std::process::id(),
        user: ["USER", "LOGNAME", "USERNAME"]
            .iter()
            .find_map(|name| std::env::var(name).ok())
            .filter(|name| !name.is_empty()),
        ..Process::default()
    };
    #[cfg(target_os = "linux")]
    linux(&mut process);
    process
}

/// The first number after `label` on a line of a `/proc` file.
#[cfg(any(target_os = "linux", test))]
fn field(text: &str, label: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix(label))?
        .split_whitespace()
        .find_map(|word| word.parse().ok())
}

/// The name `/etc/passwd` gives a user id.
#[cfg(any(target_os = "linux", test))]
fn user_named(passwd: &str, uid: u64) -> Option<String> {
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        (fields.nth(1)?.parse() == Ok(uid)).then(|| name.to_string())
    })
}

#[cfg(target_os = "linux")]
fn linux(process: &mut Process) {
    let read = |path: &str| std::fs::read_to_string(path).ok();
    if let Some(status) = read("/proc/self/status") {
        process.memory_bytes = field(&status, "VmRSS:").map(|kb| kb * 1024);
        // The effective user, which is the one files are created as; the
        // environment's name is what the service was started with.
        let uid = status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .and_then(|ids| ids.split_whitespace().nth(1)?.parse::<u64>().ok());
        if let Some(uid) = uid {
            process.user = read("/etc/passwd")
                .and_then(|passwd| user_named(&passwd, uid))
                .or_else(|| Some(format!("uid {uid}")));
        }
    }
    process.open_files = std::fs::read_dir("/proc/self/fd")
        .ok()
        // Reading the directory holds one descriptor of its own.
        .map(|entries| (entries.count() as u64).saturating_sub(1));
    process.open_files_limit =
        read("/proc/self/limits").and_then(|limits| field(&limits, "Max open files"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use kompressor_core::config::ColdRecompact;
    use kompressor_store::MemoryRefStore;
    use std::sync::Arc;

    fn core(mut config: Config) -> AppCore {
        config.stats.enabled = false;
        AppCore::new(
            config,
            Arc::new(MemoryRefStore::new()),
            kompressor_stats::null_sink(),
        )
    }

    #[test]
    fn an_address_loses_its_credentials_and_nothing_else() {
        for (given, shown) in [
            ("https://api.anthropic.com", "https://api.anthropic.com"),
            ("https://user:pass@host:8443/v1", "https://host:8443/v1"),
            ("https://host/path?to=a@b", "https://host/path?to=a@b"),
            ("not a url", "not a url"),
        ] {
            assert_eq!(without_credentials(given), shown);
        }
    }

    #[test]
    fn proc_files_are_read_by_label() {
        let status = "Name:\tkompressor\nUid:\t999\t998\t998\t998\nVmRSS:\t   51200 kB\n";
        assert_eq!(field(status, "VmRSS:"), Some(51_200));
        assert_eq!(field(status, "VmSwap:"), None);
        let limits = "Limit                     Soft Limit           Hard Limit           Units\n\
                      Max open files            1024                 524288               files\n";
        assert_eq!(field(limits, "Max open files"), Some(1_024));
        let passwd = "root:x:0:0:root:/root:/bin/bash\nkompressor:x:998:998::/var/lib/kompressor:/usr/sbin/nologin\n";
        assert_eq!(user_named(passwd, 998).as_deref(), Some("kompressor"));
        assert_eq!(user_named(passwd, 5), None);
    }

    #[test]
    fn a_stock_configuration_has_no_features_to_show() {
        let seen = running(&core(Config::default()), Load::default());
        assert_eq!(seen.features, []);
        assert_eq!(seen.auth.mode, "none");
        assert_eq!((seen.auth.keys, seen.auth.secret), (0, false));
        assert!(seen.listeners.is_empty());
        assert_eq!(seen.process.pid, std::process::id());
    }

    #[test]
    fn what_a_deployment_or_a_provider_chose_is_listed_under_its_scope() {
        let mut config: Config = toml::from_str(
            "[proxy]\ntools_compact = true\n\
             [proxy.providers.anthropic]\ncold_recompact = \"predicted\"\ncache_keep_warm = false\n\
             [proxy.providers.anthropic.engine]\nmin_elision_saving_chars = 4000\n",
        )
        .expect("config");
        config.auth.secret = "s3cret".into();
        assert_eq!(config.proxy.cold_recompact, ColdRecompact::Off);
        let core = Arc::new(core(config));
        core.runtime()
            .listening("127.0.0.1:8817", &["api", "proxy"]);
        let seen = running(&core, Load::default());
        let listed: Vec<(&str, &str, &str)> = seen
            .features
            .iter()
            .map(|f| (f.scope.as_str(), f.setting.as_str(), f.value.as_str()))
            .collect();
        assert_eq!(
            listed,
            [
                ("all", "tools_compact", "true"),
                ("anthropic", "cache_keep_warm", "false"),
                ("anthropic", "cold_recompact", "predicted"),
                ("anthropic", "engine.min_elision_saving_chars", "4000"),
            ]
        );
        assert_eq!(seen.listeners.len(), 1);
        let json = serde_json::to_string(&seen).expect("json");
        assert!(json.contains("\"secret\":true"));
        assert!(!json.contains("s3cret"), "{json}");
    }
}
