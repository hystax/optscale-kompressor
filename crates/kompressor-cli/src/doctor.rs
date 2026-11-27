//! `kompressor doctor`: what is wrong with this setup, in one screen.
//!
//! Every check here is one that turns into silence when it fails: a proxy that
//! is not running is a wrapped agent that cannot reach its provider, a base-URL
//! variable left pointing at a dead port is an agent that fails every request,
//! a fragment store that cannot be written is a handle that never resolves.
//! Headroom's `headroom doctor` has the same shape (one line per check, a hint
//! under each failure, `--json`, exit 0/1/2); the checks are ours.
//!
//! Nothing here talks to a provider: a doctor that spent tokens or sent a key
//! somewhere would be a worse problem than the one it was looking for.
//!
//! The checks are pure functions over what was observed ([`Observed`]), so the
//! tests state each classification without a network or a filesystem; `run`
//! is the only part that looks at the machine.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use kompressor_core::config::{Config, RefsBackend};
use serde::Serialize;

use crate::error::{CliError, CliResult};
use crate::wrap::agents::{AGENTS, Wiring};

/// How one check came out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "FAIL",
        }
    }
}

/// One line of the report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Check {
    pub check: String,
    pub status: Status,
    pub detail: String,
    /// What to do about it; empty when nothing needs doing.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub remedy: String,
}

fn check(
    name: &str,
    status: Status,
    detail: impl Into<String>,
    remedy: impl Into<String>,
) -> Check {
    Check {
        check: name.to_string(),
        status,
        detail: detail.into(),
        remedy: remedy.into(),
    }
}

/// What a listener answered on `GET /healthz`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    /// Nothing accepted the connection.
    Refused,
    /// A kompressor answered: status 200 with `{"status":"ok"}`.
    Kompressor,
    /// Something else is there: the status line it gave, or why it gave none.
    Other(String),
}

/// What a store's location looks like on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OnDisk {
    /// The file exists, this many bytes.
    Exists(u64),
    /// No file yet, and its directory can be created and written.
    Creatable,
    /// No file, and it cannot be made here: why.
    Blocked(String),
}

/// A base-URL variable currently set in this shell, and what answered there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvRoute {
    pub name: String,
    pub value: String,
    /// `None` when the value is not a loopback URL, which this does not probe.
    pub probe: Option<Probe>,
}

/// Everything the checks read, gathered once by [`observe`].
#[derive(Clone, Debug)]
pub struct Observed {
    pub listener: String,
    pub probe: Probe,
    pub refs: Option<OnDisk>,
    pub redis: Option<Result<(), String>>,
    pub stats: Option<OnDisk>,
    /// Catalogue agents found on `PATH`, by name.
    pub agents_found: Vec<&'static str>,
    pub env_routes: Vec<EnvRoute>,
}

/// The variables an agent reads its provider's address from, checked for a
/// value left pointing at a dead proxy.
const ROUTE_VARIABLES: [&str; 6] = [
    "ANTHROPIC_BASE_URL",
    "OPENAI_BASE_URL",
    "OPENAI_API_BASE",
    "ANTHROPIC_HOST",
    "OPENAI_HOST",
    "COPILOT_PROVIDER_BASE_URL",
];

/// Every check, in report order.
pub fn checks(config: &Config, observed: &Observed) -> Vec<Check> {
    let mut out = vec![check("config", Status::Ok, "loads and validates", "")];
    out.push(listener_check(config, observed));
    out.push(upstreams_check(config));
    out.push(refs_check(config, observed));
    out.push(stats_check(config, observed));
    out.push(agents_check(&observed.agents_found));
    out.extend(observed.env_routes.iter().map(env_check));
    out
}

fn listener_check(config: &Config, observed: &Observed) -> Check {
    if !config.proxy.enabled {
        return check(
            "proxy",
            Status::Warn,
            "proxy.enabled = false: the proxy surface is not served",
            "set [proxy] enabled = true to route agents through it",
        );
    }
    match &observed.probe {
        Probe::Kompressor => check(
            "proxy",
            Status::Ok,
            format!("answering on {}", observed.listener),
            "",
        ),
        Probe::Refused => check(
            "proxy",
            Status::Warn,
            format!("nothing is listening on {}", observed.listener),
            "start it with `kompressor serve`, or let `kompressor wrap <agent>` start one",
        ),
        Probe::Other(what) => check(
            "proxy",
            Status::Fail,
            format!(
                "{} answers, but not as kompressor: {what}",
                observed.listener
            ),
            "another service holds the port; set server.listen or proxy_listen to a free one",
        ),
    }
}

fn upstreams_check(config: &Config) -> Check {
    let upstreams = &config.proxy.upstreams;
    if upstreams.is_empty() {
        return check(
            "upstreams",
            Status::Warn,
            "none configured: a served proxy answers 404 to every path",
            "`kompressor wrap` adds the ones an agent needs; a standalone `serve` needs \
             [[proxy.upstreams]]",
        );
    }
    let names: Vec<String> = upstreams
        .iter()
        .map(|u| {
            format!(
                "{} → {}",
                if u.name.is_empty() { "?" } else { &u.name },
                u.base_url
            )
        })
        .collect();
    check("upstreams", Status::Ok, names.join(", "), "")
}

fn on_disk_check(name: &str, path: &Path, seen: Option<&OnDisk>, what: &str) -> Check {
    match seen {
        Some(OnDisk::Exists(bytes)) => check(
            name,
            Status::Ok,
            format!("{} ({bytes} bytes)", path.display()),
            "",
        ),
        Some(OnDisk::Creatable) | None => check(
            name,
            Status::Ok,
            format!("{} will be created on first use", path.display()),
            "",
        ),
        Some(OnDisk::Blocked(why)) => check(
            name,
            Status::Fail,
            format!("{} cannot be created: {why}", path.display()),
            format!("point {what} at a writable directory"),
        ),
    }
}

fn refs_check(config: &Config, observed: &Observed) -> Check {
    match config.refs.backend {
        RefsBackend::Memory => check(
            "refs",
            Status::Warn,
            "in memory: fragments are lost on restart and not shared between proxies",
            "use backend = \"sqlite\" (one host) or \"redis\" (several)",
        ),
        RefsBackend::Sqlite => on_disk_check(
            "refs",
            &config.refs.path,
            observed.refs.as_ref(),
            "refs.path",
        ),
        RefsBackend::Redis => match &observed.redis {
            Some(Ok(())) | None => check(
                "refs",
                Status::Ok,
                format!("redis at {}", redact_url(&config.refs.redis_url)),
                "",
            ),
            Some(Err(why)) => check(
                "refs",
                Status::Fail,
                format!(
                    "redis at {} does not answer: {why}",
                    redact_url(&config.refs.redis_url)
                ),
                "check refs.redis_url and that the server is reachable from here",
            ),
        },
    }
}

fn stats_check(config: &Config, observed: &Observed) -> Check {
    if !config.stats.enabled {
        return check(
            "stats",
            Status::Warn,
            "statistics are off: no savings, cache or advisor reports",
            "set [stats] enabled = true",
        );
    }
    on_disk_check(
        "stats",
        &config.stats.path,
        observed.stats.as_ref(),
        "stats.path",
    )
}

fn agents_check(found: &[&str]) -> Check {
    if found.is_empty() {
        return check(
            "agents",
            Status::Ok,
            "no wrappable agent on PATH",
            "`kompressor wrap --list` shows the ones kompressor knows",
        );
    }
    check(
        "agents",
        Status::Ok,
        format!("on PATH: {}", found.join(", ")),
        "",
    )
}

fn env_check(route: &EnvRoute) -> Check {
    let name = format!("env {}", route.name);
    match &route.probe {
        None => check(
            &name,
            Status::Ok,
            format!("{} (not local, not probed)", route.value),
            "",
        ),
        Some(Probe::Kompressor) => check(
            &name,
            Status::Ok,
            format!("{} → kompressor", route.value),
            "",
        ),
        Some(Probe::Refused) => check(
            &name,
            Status::Fail,
            format!(
                "{} has nothing listening: every request from an agent reading it fails",
                route.value
            ),
            format!("unset {} or start the proxy it names", route.name),
        ),
        Some(Probe::Other(what)) => check(
            &name,
            Status::Warn,
            format!(
                "{} is answered by something other than kompressor ({what})",
                route.value
            ),
            "fine if that is another proxy on purpose",
        ),
    }
}

/// `redis://user:secret@host` with the password replaced.
fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    match rest.rsplit_once('@') {
        Some((auth, host)) => {
            let user = auth.split_once(':').map_or("", |(user, _)| user);
            format!("{scheme}://{user}:***@{host}")
        }
        None => url.to_string(),
    }
}

/// The exit code a report earns: 0 all well, 1 warnings only, 2 any failure.
pub fn exit_code(checks: &[Check]) -> i32 {
    if checks.iter().any(|c| c.status == Status::Fail) {
        2
    } else if checks.iter().any(|c| c.status == Status::Warn) {
        1
    } else {
        0
    }
}

/// The loopback `host:port` a URL names, when it names one.
pub fn loopback_target(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split('/').next()?;
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.ends_with(']') || authority.starts_with('[') => {
            (host, port.parse::<u16>().ok()?)
        }
        _ => (authority, if url.starts_with("https") { 443 } else { 80 }),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let local = host.eq_ignore_ascii_case("localhost") || host == "::1" || host.starts_with("127.");
    local.then(|| {
        if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        }
    })
}

/// `GET /healthz` on `addr`, by hand: no runtime, a short timeout.
pub fn probe(addr: &str) -> Probe {
    let Some(target) = addr.to_socket_addrs().ok().and_then(|mut a| a.next()) else {
        return Probe::Other(format!("{addr} does not resolve"));
    };
    probe_socket(target)
}

fn probe_socket(target: SocketAddr) -> Probe {
    let timeout = Duration::from_millis(1500);
    let mut stream = match TcpStream::connect_timeout(&target, timeout) {
        Ok(stream) => stream,
        Err(_) => return Probe::Refused,
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let request = format!(
        "GET /healthz HTTP/1.1\r\nHost: {target}\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return Probe::Other("the connection closed before a request could be sent".into());
    }
    let mut response = Vec::new();
    let _ = stream.take(64 * 1024).read_to_end(&mut response);
    classify_health(&String::from_utf8_lossy(&response))
}

/// What a `/healthz` response says about who answered.
pub fn classify_health(response: &str) -> Probe {
    let status = response.lines().next().unwrap_or("").trim();
    if status.is_empty() {
        return Probe::Other("no HTTP answer".into());
    }
    let ok = status.split_whitespace().nth(1) == Some("200");
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("");
    if ok && body.contains("\"status\"") && body.contains("\"ok\"") {
        Probe::Kompressor
    } else {
        Probe::Other(status.to_string())
    }
}

fn on_disk(path: &Path) -> OnDisk {
    if let Ok(meta) = std::fs::metadata(path) {
        return OnDisk::Exists(meta.len());
    }
    // The nearest ancestor that exists decides whether the directories can be
    // made; nothing is created here.
    let mut dir: PathBuf = path.parent().map(Path::to_path_buf).unwrap_or_default();
    if dir.as_os_str().is_empty() {
        dir = PathBuf::from(".");
    }
    while !dir.exists() {
        match dir.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => dir = parent.to_path_buf(),
            _ => {
                dir = PathBuf::from(".");
                break;
            }
        }
    }
    match std::fs::metadata(&dir) {
        Ok(meta) if meta.permissions().readonly() => {
            OnDisk::Blocked(format!("{} is read-only", dir.display()))
        }
        Ok(meta) if !meta.is_dir() => {
            OnDisk::Blocked(format!("{} is not a directory", dir.display()))
        }
        Ok(_) => OnDisk::Creatable,
        Err(error) => OnDisk::Blocked(error.to_string()),
    }
}

/// Look at the machine.
fn observe(config: &Config) -> Observed {
    let listener = if config.server.proxy_listen.is_empty() {
        config.server.listen.clone()
    } else {
        config.server.proxy_listen.clone()
    };
    let probe = if config.proxy.enabled {
        probe(&listener)
    } else {
        Probe::Refused
    };
    let refs = (config.refs.backend == RefsBackend::Sqlite).then(|| on_disk(&config.refs.path));
    let redis = (config.refs.backend == RefsBackend::Redis).then(|| {
        kompressor_store::redis::ping(&config.refs.redis_url, Duration::from_secs(2))
            .map_err(|e| e.to_string())
    });
    let stats = config.stats.enabled.then(|| on_disk(&config.stats.path));
    let path = std::env::var_os("PATH");
    let agents_found = AGENTS
        .iter()
        .filter(|a| !matches!(a.wiring, Wiring::Unsupported { .. } | Wiring::Manual { .. }))
        .filter(|a| crate::wrap::hooks::find_on_path(&[a.binary], path.clone()).is_some())
        .map(|a| a.name)
        .collect();
    let env_routes = ROUTE_VARIABLES
        .iter()
        .filter_map(|name| {
            let value = std::env::var(name).ok().filter(|v| !v.trim().is_empty())?;
            let probe = loopback_target(&value).map(|target| self::probe(&target));
            Some(EnvRoute {
                name: name.to_string(),
                value,
                probe,
            })
        })
        .collect();
    Observed {
        listener,
        probe,
        refs,
        redis,
        stats,
        agents_found,
        env_routes,
    }
}

/// `kompressor doctor`.
pub fn run(config_path: Option<&Path>, json: bool) -> CliResult {
    let config = match Config::load(config_path) {
        Ok(config) => config,
        Err(error) => {
            let failed = vec![check(
                "config",
                Status::Fail,
                error.to_string(),
                "`kompressor check-config` shows the full error",
            )];
            report(&failed, json);
            return Err(CliError::Quiet(2));
        }
    };
    let checks = checks(&config, &observe(&config));
    report(&checks, json);
    match exit_code(&checks) {
        0 => Ok(()),
        code => Err(CliError::Quiet(code)),
    }
}

fn report(checks: &[Check], json: bool) {
    if json {
        let out = serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "exit_code": exit_code(checks),
            "checks": checks,
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return;
    }
    let width = checks.iter().map(|c| c.check.len()).max().unwrap_or(0);
    for c in checks {
        println!("{:>4}  {:width$}  {}", c.status.label(), c.check, c.detail);
        if c.status != Status::Ok && !c.remedy.is_empty() {
            println!("      {:width$}  → {}", "", c.remedy);
        }
    }
    let fails = checks.iter().filter(|c| c.status == Status::Fail).count();
    let warns = checks.iter().filter(|c| c.status == Status::Warn).count();
    println!();
    if fails + warns == 0 {
        println!("all checks passed");
    } else {
        println!("{fails} failure(s), {warns} warning(s)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observed() -> Observed {
        Observed {
            listener: "127.0.0.1:8817".into(),
            probe: Probe::Kompressor,
            refs: Some(OnDisk::Exists(4096)),
            redis: None,
            stats: Some(OnDisk::Creatable),
            agents_found: vec!["claude"],
            env_routes: Vec::new(),
        }
    }

    fn healthy_config() -> Config {
        let mut config = Config::default();
        config.proxy.upstreams = vec![kompressor_core::config::Upstream {
            name: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            ..Default::default()
        }];
        config.stats.enabled = true;
        config
    }

    fn status_of(checks: &[Check], name: &str) -> Status {
        checks.iter().find(|c| c.check == name).unwrap().status
    }

    #[test]
    fn password_only_redis_url_is_redacted() {
        assert_eq!(
            redact_url("redis://secret-password@cache/0"),
            "redis://:***@cache/0"
        );
    }

    #[test]
    fn a_healthy_setup_passes_every_check() {
        let got = super::checks(&healthy_config(), &observed());
        assert!(got.iter().all(|c| c.status == Status::Ok), "{got:#?}");
        assert_eq!(exit_code(&got), 0);
    }

    #[test]
    fn a_stopped_proxy_is_a_warning_and_a_stranger_on_its_port_a_failure() {
        let mut seen = observed();
        seen.probe = Probe::Refused;
        let got = super::checks(&healthy_config(), &seen);
        assert_eq!(status_of(&got, "proxy"), Status::Warn);
        assert_eq!(exit_code(&got), 1);

        seen.probe = Probe::Other("HTTP/1.1 404 Not Found".into());
        let got = super::checks(&healthy_config(), &seen);
        assert_eq!(status_of(&got, "proxy"), Status::Fail);
        assert_eq!(exit_code(&got), 2);
    }

    #[test]
    fn no_upstreams_memory_refs_and_no_stats_are_warnings() {
        let mut config = healthy_config();
        config.proxy.upstreams.clear();
        config.refs.backend = RefsBackend::Memory;
        config.stats.enabled = false;
        let got = super::checks(&config, &observed());
        for name in ["upstreams", "refs", "stats"] {
            assert_eq!(status_of(&got, name), Status::Warn, "{name}");
        }
    }

    #[test]
    fn a_store_that_cannot_be_created_fails() {
        let mut seen = observed();
        seen.refs = Some(OnDisk::Blocked("/ is read-only".into()));
        let got = super::checks(&healthy_config(), &seen);
        assert_eq!(status_of(&got, "refs"), Status::Fail);
    }

    #[test]
    fn an_unreachable_redis_fails_and_its_password_is_not_printed() {
        let mut config = healthy_config();
        config.refs.backend = RefsBackend::Redis;
        config.refs.redis_url = "redis://app:s3cret@10.0.0.5:6379/0".into();
        let mut seen = observed();
        seen.redis = Some(Err("connection refused".into()));
        let got = super::checks(&config, &seen);
        let refs = got.iter().find(|c| c.check == "refs").unwrap();
        assert_eq!(refs.status, Status::Fail);
        assert!(!refs.detail.contains("s3cret"), "{}", refs.detail);
        assert!(refs.detail.contains("app:***@10.0.0.5"));
    }

    #[test]
    fn a_base_url_left_pointing_at_a_dead_port_fails() {
        let mut seen = observed();
        seen.env_routes = vec![
            EnvRoute {
                name: "ANTHROPIC_BASE_URL".into(),
                value: "http://127.0.0.1:9999".into(),
                probe: Some(Probe::Refused),
            },
            EnvRoute {
                name: "OPENAI_BASE_URL".into(),
                value: "https://gateway.example/v1".into(),
                probe: None,
            },
        ];
        let got = super::checks(&healthy_config(), &seen);
        assert_eq!(status_of(&got, "env ANTHROPIC_BASE_URL"), Status::Fail);
        assert_eq!(status_of(&got, "env OPENAI_BASE_URL"), Status::Ok);
    }

    #[test]
    fn only_loopback_urls_are_probed() {
        assert_eq!(
            loopback_target("http://127.0.0.1:8817/v1").as_deref(),
            Some("127.0.0.1:8817")
        );
        assert_eq!(
            loopback_target("http://localhost:8817").as_deref(),
            Some("localhost:8817")
        );
        assert_eq!(
            loopback_target("http://[::1]:8817").as_deref(),
            Some("[::1]:8817")
        );
        assert_eq!(
            loopback_target("http://127.0.0.1").as_deref(),
            Some("127.0.0.1:80")
        );
        assert_eq!(loopback_target("https://api.openai.com/v1"), None);
    }

    #[test]
    fn a_kompressor_health_answer_is_told_from_any_other() {
        let ours = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"status\":\"ok\"}";
        assert_eq!(classify_health(ours), Probe::Kompressor);
        let other = "HTTP/1.1 200 OK\r\n\r\n<html>hello</html>";
        assert!(matches!(classify_health(other), Probe::Other(_)));
        assert!(matches!(classify_health(""), Probe::Other(_)));
    }

    #[test]
    fn the_exit_code_ranks_failures_over_warnings() {
        let ok = check("a", Status::Ok, "", "");
        let warn = check("b", Status::Warn, "", "");
        let fail = check("c", Status::Fail, "", "");
        assert_eq!(exit_code(std::slice::from_ref(&ok)), 0);
        assert_eq!(exit_code(&[ok.clone(), warn.clone()]), 1);
        assert_eq!(exit_code(&[warn, fail, ok]), 2);
    }

    #[test]
    fn json_checks_carry_check_status_detail_and_remedy() {
        let value = serde_json::to_value(check("refs", Status::Fail, "x", "y")).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"check": "refs", "status": "fail", "detail": "x", "remedy": "y"})
        );
    }

    #[test]
    fn a_probe_of_a_closed_port_is_refused() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        assert_eq!(probe(&addr.to_string()), Probe::Refused);
    }
}
