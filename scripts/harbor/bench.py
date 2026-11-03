#!/usr/bin/env python3
"""Run Harbor / terminal-bench against this proxy, and measure what it did.

    codex (task container) --http--> kompressor serve --proxy --api --https--> provider

The engine is measured by `make bench`, which replays a corpus. This measures
something the corpus cannot: what happens when a real coding agent works a real
task through the proxy for twenty minutes -- whether compression holds up on
live traffic, whether recoverable mode's handles are ever fetched back, and
whether the proxy changes how often the agent succeeds.

Needs Harbor, a provider key and money. `--skip-harbor` runs every gate and
stops before spending any, which is the right first command on a new machine.

## The gates

Each one corresponds to a way a run completes, reports a number, and means
nothing. They run before Harbor starts.

1. **Effective configuration.** The config is generated into the dump and read
   back with `kompressor check-config`. The run refuses unless the effective
   result is the one asked for -- above all `api_family = "openai"` on the
   upstream mount, because the default `"passthrough"` makes `handler.rs` relay
   every request unparsed while `/healthz` stays green.
2. **`/healthz` + `/readyz`**, the latter per-subsystem: a proxy listening with
   a dead ref store would mint no handles and still look up.
3. **Compression self-test** on `/v1/compress/raw`, over ripgrep output and a
   distinct-line log. It asserts bytes fell *and*, in `recoverable`, that a
   handle was minted -- a recoverable run that mints none is an aggressive run
   wearing the wrong label.
4. **Upstream preflight**: one tiny `/v1/responses` at the provider. The key
   and the model name are real.
5. **Through-proxy preflight**: the same call from the address the agent will
   use. The only check that exercises routing, auth passthrough and the mount
   together. 404 is `path_prefix`, 401 is `forward_auth`, 502 is `base_url`.

## Reading the result

Per-request rows come from the statistics database, joining `proxy_requests` to
`optimization_events` **on `request_id`**. Not on `compression_event_id`: that
column is declared as a foreign key and deliberately never written, and joining
on it reports a fully compressing run as having compressed nothing.

Standard library only, so it runs anywhere the binary does.
"""
import argparse
import datetime as dt
import glob
import hashlib
import json
import os
import re
import shlex
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import time
import urllib.error
import urllib.request


# ------------------------------------------------------------------- plumbing
# Standard library only, deliberately: this ships in a Rust repository where
# nobody has a Python environment set up, and a benchmark that needs `pip
# install` before it will run is a benchmark that does not get run.


def http_json(url, payload=None, headers=None, timeout=60, method=None):
    """One JSON request. Returns (status, parsed-or-text, headers)."""
    data = json.dumps(payload).encode() if payload is not None else None
    request = urllib.request.Request(url, data=data, method=method or
                                     ("POST" if data is not None else "GET"))
    request.add_header("content-type", "application/json")
    for name, value in (headers or {}).items():
        request.add_header(name, value)
    try:
        with urllib.request.urlopen(request, timeout=timeout) as resp:
            body = resp.read().decode("utf-8", "replace")
            status, head = resp.status, dict(resp.headers)
    except urllib.error.HTTPError as exc:
        body = exc.read().decode("utf-8", "replace")
        status, head = exc.code, dict(exc.headers)
    except (urllib.error.URLError, OSError, TimeoutError) as exc:
        raise ConnectionError(str(exc)) from exc
    try:
        return status, json.loads(body), head
    except ValueError:
        return status, body, head


_SECRET_PATTERNS = (
    re.compile(r"\b(sk-[A-Za-z0-9]{2,}-)[A-Za-z0-9_-]{8,}"),   # sk-proj-…, sk-bf-…
    re.compile(r"\b(sk-)[A-Za-z0-9_-]{16,}"),                  # bare sk-…
    re.compile(r"(?i)\b(bearer\s+)[A-Za-z0-9._~+/=-]{16,}"),
    re.compile(r"(?i)\b(x-api-key[:=]\s*)[A-Za-z0-9._~+/=-]{16,}"),
)


def redact(text):
    """Replace secret-shaped substrings with a recognizable placeholder."""
    if not isinstance(text, str):
        return text
    for pattern in _SECRET_PATTERNS:
        text = pattern.sub(lambda m: f"{m.group(1)}REDACTED", text)
    return text


def redact_file(path):
    """Rewrite a file in place with secrets redacted. Returns True if changed.

    The `\b` anchors in _SECRET_PATTERNS are load-bearing, not decoration:
    codex writes encrypted reasoning blobs (`encrypted_content: "gAAAAAB…"`)
    into its rollout jsonl, and such base64 routinely contains "sk-" followed
    by 16+ base64 characters mid-string. Without the word boundary this sweep
    would silently corrupt those artefacts. Verified against a real rollout.
    """
    try:
        with open(path, encoding="utf-8", errors="surrogateescape") as handle:
            before = handle.read()
    except (OSError, UnicodeDecodeError):
        return False
    after = redact(before)
    if after == before:
        return False
    with open(path, "w", encoding="utf-8", errors="surrogateescape") as handle:
        handle.write(after)
    return True


def fmt_pct(x):
    return "n/a" if x is None else f"{x * 100:.1f}%"


def fmt_usd(x):
    return "n/a" if x is None else f"${x:.4f}"


def normalize_upstream(url):
    """The upstream is a base, and the mount adds /v1 itself, so a URL given
    either way has to mean the same thing. One trailing /v1 is stripped, and
    what is printed is then what the proxy will actually forward to."""
    if not url:
        return url
    trimmed = url.rstrip("/")
    return trimmed[:-3].rstrip("/") if trimmed.endswith("/v1") else trimmed


def detect_advertise_host():
    """Pick an address the task CONTAINER can use to reach this host.

    Prefer the docker0 gateway: it is reachable from every docker bridge
    network (containers route to the host) but is not exposed to the LAN.
    Falls back to the primary outbound IP.
    """
    try:
        out = subprocess.run(["ip", "-4", "-json", "addr", "show", "docker0"],
                             capture_output=True, text=True, timeout=10)
        if out.returncode == 0:
            for iface in json.loads(out.stdout or "[]"):
                for addr in iface.get("addr_info") or []:
                    if addr.get("family") == "inet" and addr.get("local"):
                        return addr["local"]
    except (OSError, ValueError, subprocess.SubprocessError):
        pass
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            sock.connect(("1.1.1.1", 53))
            return sock.getsockname()[0]
        finally:
            sock.close()
    except OSError:
        return "127.0.0.1"


def port_is_open(host, port, timeout=1.5):
    try:
        with socket.create_connection((host, port), timeout=timeout):
            return True
    except OSError:
        return False


def read_harbor_result(out_dir, since_ts=0):
    """Parse Harbor's job-level result.json.

    Harbor writes <out_dir>/<UTC-ts>/result.json with:
        n_total_trials, stats.{n_input_tokens,n_cache_tokens,n_output_tokens,
        cost_usd,n_errored_trials, evals.<key>.{n_trials,metrics[0].mean,
        reward_stats.reward{<reward>: [trial_names]}}}
    A trial counts as resolved at reward >= 1.0 (terminal-bench is binary).
    Returns {accuracy, counts:(resolved,total), path, job_dir, stats}.
    """
    miss = {"accuracy": None, "counts": None, "path": None, "job_dir": None,
            "stats": None}
    cands = glob.glob(os.path.join(out_dir, "**", "result.json"), recursive=True)
    for path in sorted(cands, key=os.path.getmtime, reverse=True):
        if os.path.getmtime(path) < since_ts - 5:
            continue                       # a job dir from an earlier run
        try:
            data = json.load(open(path, encoding="utf-8"))
        except (OSError, ValueError):
            continue
        stats = data.get("stats") if isinstance(data, dict) else None
        if not isinstance(stats, dict) or "evals" not in stats:
            continue                       # a per-trial result.json, not the aggregate
        evals = stats.get("evals") or {}
        n_total = data.get("n_total_trials") or sum(
            e.get("n_trials", 0) for e in evals.values())
        n_res, weighted = 0, []
        for eval_entry in evals.values():
            rewards = (eval_entry.get("reward_stats") or {}).get("reward") or {}
            for reward, names in rewards.items():
                try:
                    if float(reward) >= 1.0:
                        n_res += len(names)
                except (TypeError, ValueError):
                    pass
            metrics = eval_entry.get("metrics") or []
            if metrics and isinstance(metrics[0], dict) \
                    and metrics[0].get("mean") is not None:
                weighted.append((eval_entry.get("n_trials", 0), metrics[0]["mean"]))
        denom = sum(w for w, _ in weighted)
        acc = (sum(w * v for w, v in weighted) / denom if denom
               else (n_res / n_total if n_total else None))
        return {"accuracy": acc, "counts": (n_res, n_total), "path": path,
                "job_dir": os.path.dirname(path),
                "stats": {k: stats.get(k) for k in (
                    "n_input_tokens", "n_cache_tokens", "n_output_tokens",
                    "cost_usd", "n_errored_trials")}}
    return miss


HEALTH_TIMEOUT = 90
STOP_GRACE = 30
# realpath, not abspath: this script is symlinked from elsewhere, and abspath
# would resolve the binary relative to the link's directory.
DEFAULT_BIN = os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.realpath(__file__)))),
    "target", "release", "kompressor")



def env(name, default=None):
    value = os.environ.get(name)
    return value if value not in (None, "") else default


# --------------------------------------------------------------- the proxy
def control_arm(mode):
    """The two config lines that depend on `mode` being the control arm.

    Returns `(engine_mode_line, proxy_lines)`.

    "off" is a `--mode` choice argparse accepts and the engine has no variant
    for: it has three and none is a passthrough. Written into `[engine]` it
    fails `check-config` before a run spends anything, which is why the
    uncompressed baseline was never once collected. It goes to the proxy
    instead; the engine keeps a valid mode it is never asked to run.

    `proxy.compress`, not `proxy.enabled`. The second is not a passthrough:
    `serve` does not mount the proxy router when it is false, so every path
    answers 404 and the baseline loses the network hop that is the whole reason
    it runs through the proxy rather than around it. `compress = false` leaves
    the router mounted and the bodies parsed, so usage and cache reads are still
    recorded and the two arms differ in compression and in nothing else.

    One function because the self-test calls it too, and a self-test that
    re-states what the emitter should do only ever checks its own copy.
    """
    engine = "recoverable" if mode == "off" else mode
    return f'mode = "{engine}"', (["compress = false"] if mode == "off" else [])


class KompressorRs:
    """Lifecycle + telemetry client for a local kompressor-rs process.

    Telemetry follows the bind address rather than assuming loopback: the
    proxy is bound to the docker bridge so that it is reachable from the task
    containers and from nowhere else, and on that bind 127.0.0.1 does not
    answer.
    """

    def __init__(self, cfg):
        self.cfg = cfg
        self.proc = None
        self.started = False
        self.effective = ""
        self.telemetry_host = ("127.0.0.1" if cfg.bind_host in ("0.0.0.0", "::")
                               else cfg.bind_host)
        self.local = f"http://{self.telemetry_host}:{cfg.port}"
        self.agent_base = f"http://{cfg.advertise_host}:{cfg.port}/v1"
        self.config_path = os.path.join(cfg.out_dir, "kompressor.toml")
        self.refs_db = os.path.join(cfg.out_dir, "kompressor-refs.db")
        self.stats_db = os.path.join(cfg.out_dir, "kompressor-stats.db")
        # Kept with the run rather than in /tmp. A failing run's bodies are only
        # interpretable against a passing run's, and the passing ones were being
        # thrown away: the first accuracy failure in twenty runs arrived with its
        # own capture and nothing to compare it to. An explicit
        # KOMPRESSOR_BODY_CAPTURE still wins, for the case where someone wants it
        # somewhere specific.
        self.bodies = os.environ.get("KOMPRESSOR_BODY_CAPTURE") or os.path.join(
            cfg.out_dir, "kompressor-bodies.jsonl")
        # The same for relayed websocket frames, and for the same reason. These
        # used to go wherever the operator pointed the variable and nowhere by
        # default, so a relayed run's frames landed in /tmp if anywhere, were
        # overwritten by the next run, and were not in the dump beside the
        # bodies they have to be read against. A capture kept somewhere else is
        # a capture nobody has when the question comes up.
        #
        # The file is only written when `proxy.websocket_relay` is on; naming it
        # unconditionally costs nothing and means a run that turns the relay on
        # needs no second decision.
        self.frames = os.environ.get("KOMPRESSOR_WS_CAPTURE") or os.path.join(
            cfg.out_dir, "kompressor-ws-frames.jsonl")
        self.log_path = None

    # ----------------------------------------------------------- configuration
    def write_config(self):
        cfg = self.cfg
        os.makedirs(cfg.out_dir, exist_ok=True)
        engine_mode, proxy_control = control_arm(cfg.mode)
        # bind on the advertise host so the task containers can reach it; the
        # wrapper still talks to 127.0.0.1 for telemetry.
        listen = f"{cfg.bind_host}:{cfg.port}"
        lines = [
            "# generated by harbor_terminal_bench_kompressor_rs.py -- do not edit",
            f"# run {cfg.stamp}",
            "",
            "[server]",
            f'listen = "{listen}"',
            "",
            "[engine]",
            engine_mode,
            f"min_compress_chars = {cfg.min_compress_chars}",
            f"max_ref_bytes = {cfg.max_ref_bytes}",
            f"ref_ttl_seconds = {cfg.ref_ttl_seconds}",
            "",
            "[refs]",
            'backend = "sqlite"',
            f'path = "{self.refs_db}"',
            "",
            "[stats]",
            "enabled = true",
            f'path = "{self.stats_db}"',
            "",
            "[log]",
            f'level = "{cfg.log_level}"',
            'format = "json"',
            "",
            "[proxy]",
            *proxy_control,
            f"min_tokens = {cfg.min_tokens}",
            f"retrieve_max_rounds = {cfg.retrieve_max_rounds}",
            f'retrieve_tool = "{"never" if cfg.no_retrieve_tool else "auto"}"',
            "",
            "[[proxy.upstreams]]",
            f'name = "{cfg.provider}"',
            # Anthropic clients append `/v1/messages` to a bare host, so the
            # mount is the full path rather than the `/v1` prefix an OpenAI
            # client completes itself. Getting this wrong is the 404.
            f'path_prefix = "{"/v1/messages" if cfg.provider == "anthropic" else "/v1"}"',
            f'base_url = "{cfg.upstream}"',
            "forward_auth = true",
            # THE line. Default "passthrough" means handler.rs never parses the
            # body, so the proxy relays byte-for-byte and reports itself
            # healthy while compressing nothing.
            f'api_family = "{cfg.provider}"',
            "",
        ]
        with open(self.config_path, "w", encoding="utf-8") as handle:
            handle.write("\n".join(lines))
        return self.config_path

    def check_config(self):
        """`kompressor check-config`: the binary's own view of the file."""
        proc = subprocess.run(
            [self.cfg.kompressor_bin, "check-config", "--config", self.config_path],
            capture_output=True, text=True, check=False)
        if proc.returncode != 0:
            sys.exit(f"kompressor check-config failed ({proc.returncode}):\n"
                     f"{proc.stdout}\n{proc.stderr}")
        # Kept so later output can report what the proxy is *actually* running
        # rather than re-deriving it from the request.
        self.effective = proc.stdout
        return proc.stdout

    def assert_effective(self, effective):
        """Refuse to spend money on a configuration that is not the one asked
        for. Every check here corresponds to a way the run can look healthy and
        measure nothing."""
        problems = []
        text = effective
        if self.cfg.mode == "off":
            # The control arm's whole claim is that nothing was compressed.
            # Checking the engine mode here would check the wrong field: the
            # engine is left at a valid mode it is never asked to run.
            if "compress = false" not in text:
                problems.append(
                    "mode=off but the proxy still compresses -- the control arm's "
                    "numbers would be a second compressed run")
        elif f'mode = "{self.cfg.mode}"' not in text:
            problems.append(f'engine mode is not "{self.cfg.mode}"')
        if f'api_family = "{self.cfg.provider}"' not in text:
            problems.append(
                f'the upstream mount is not api_family = "{self.cfg.provider}" -- '
                "the proxy would forward every request unparsed and compress nothing")
        if self.cfg.upstream.rstrip("/") not in text:
            problems.append(f"upstream {self.cfg.upstream} is not in the effective config")
        if "forward_auth = true" not in text:
            problems.append("forward_auth is off -- the agent's key would not reach the provider")
        # "Requested" includes the environment. This guard used to hardcode
        # `auto`, so a run deliberately overriding `retrieve_tool` was refused
        # before it started -- the opposite failure to the one below, and just
        # as expensive in wasted time.
        tool_override = os.environ.get("KOMPRESSOR__PROXY__RETRIEVE_TOOL", "").strip().lower()
        if self.cfg.no_retrieve_tool:
            want_tool = "never"
            if tool_override and tool_override != "never":
                problems.append(
                    f'--no-retrieve-tool and KOMPRESSOR__PROXY__RETRIEVE_TOOL="{tool_override}" '
                    "ask for different things")
        elif tool_override in ("auto", "always", "never"):
            want_tool = tool_override
        else:
            want_tool = "auto"
            if tool_override:
                problems.append(
                    f'KOMPRESSOR__PROXY__RETRIEVE_TOOL="{tool_override}" is not a value this '
                    "build reads; it would be ignored")
        if f'retrieve_tool = "{want_tool}"' not in text:
            problems.append(f'retrieve_tool is not "{want_tool}"')
        # The failure this guard exists for, and the one it was missing: four
        # runs were taken as tests of `first_seen` and were all `marker`,
        # because the override was accepted and silently ignored. A run that
        # cannot answer the question it was started for should not be paid for.
        frozen_override = os.environ.get("KOMPRESSOR__PROXY__FROZEN_PREFIX", "").strip().lower()
        if frozen_override:
            if frozen_override not in ("marker", "first_seen"):
                problems.append(
                    f'KOMPRESSOR__PROXY__FROZEN_PREFIX="{frozen_override}" is not a value this '
                    "build reads; it would be ignored")
            elif f'frozen_prefix = "{frozen_override}"' not in text:
                problems.append(
                    f'frozen_prefix is not "{frozen_override}" -- the run would measure the '
                    "default instead of the setting it was started for")
        # Provider-scoped settings, checked the same way: every
        # KOMPRESSOR__PROXY__PROVIDERS__<NAME>__<SETTING> has to be what the
        # effective config holds for that provider, or the run measures the
        # default under the name of the override.
        problems.extend(provider_override_problems(text, os.environ))
        if self.refs_db not in text or self.stats_db not in text:
            problems.append("refs/stats are not the per-run databases in the dump directory")
        if problems:
            sys.exit("effective configuration is not the requested one:\n  - "
                     + "\n  - ".join(problems))

    # ----------------------------------------------------------------- process
    def start(self):
        cfg = self.cfg
        if port_is_open(self.telemetry_host, cfg.port):
            if not cfg.reuse_proxy:
                sys.exit(
                    f"port {cfg.port} is already serving. That proxy's compression "
                    "settings and its upstream are unknown, so its numbers cannot "
                    "be attributed to this run -- stop it, pick --port, or pass "
                    "--reuse-proxy to accept it as-is.")
            print(f"  reusing the proxy already listening on {self.local}")
            return
        if cfg.reuse_proxy:
            sys.exit("--reuse-proxy given but nothing is listening on "
                     f"{self.telemetry_host}:{cfg.port}")

        cmd = [cfg.kompressor_bin, "serve", "--proxy", "--api",
               "--config", self.config_path]
        self.log_path = os.path.join(cfg.out_dir, "kompressor-proxy.log")
        print("  $ " + " ".join(shlex.quote(c) for c in cmd))
        print(f"  proxy stdout/stderr -> {self.log_path}")
        handle = open(self.log_path, "w", encoding="utf-8")
        env = dict(
            os.environ,
            KOMPRESSOR_BODY_CAPTURE=self.bodies,
            KOMPRESSOR_WS_CAPTURE=self.frames,
        )
        print(f"  request bodies -> {self.bodies}")
        print(f"  websocket frames -> {self.frames}  (written only when the relay is on)")
        self.proc = subprocess.Popen(cmd, stdout=handle, stderr=subprocess.STDOUT,
                                     start_new_session=True, env=env)
        self.started = True
        self.wait_healthy()

    def wait_healthy(self):
        deadline = time.time() + HEALTH_TIMEOUT
        while time.time() < deadline:
            if self.proc is not None and self.proc.poll() is not None:
                sys.exit(f"kompressor exited with code {self.proc.returncode} during "
                         f"startup -- see {self.log_path}")
            try:
                health, _, _ = http_json(f"{self.local}/healthz", timeout=5)
                ready, ready_body, _ = http_json(f"{self.local}/readyz", timeout=5)
                if health == 200 and ready == 200:
                    checks = (ready_body or {}).get("checks", {})
                    if not all(checks.values()):
                        sys.exit(f"kompressor is listening but not ready: {checks}")
                    print(f"  proxy healthy, readyz checks {checks}")
                    return
            except (ConnectionError, ValueError):
                pass
            time.sleep(1)
        sys.exit(f"kompressor did not become ready within {HEALTH_TIMEOUT}s "
                 f"-- see {self.log_path}")

    def stop(self):
        if not self.started or self.proc is None or self.proc.poll() is not None:
            return
        # SIGINT so the graceful shutdown drains the stats queue; statistics are
        # written off the request path, so a hard kill loses the tail of the run.
        try:
            os.killpg(os.getpgid(self.proc.pid), signal.SIGINT)
        except (OSError, ProcessLookupError):
            self.proc.terminate()
        try:
            self.proc.wait(timeout=STOP_GRACE)
        except subprocess.TimeoutExpired:
            print(f"  proxy ignored SIGINT for {STOP_GRACE}s -- killing")
            try:
                os.killpg(os.getpgid(self.proc.pid), signal.SIGKILL)
            except (OSError, ProcessLookupError):
                self.proc.kill()
            self.proc.wait(timeout=10)

    # --------------------------------------------------------------- telemetry
    def _get(self, path):
        try:
            status, body, _ = http_json(f"{self.local}{path}", timeout=60)
        except ConnectionError as exc:
            print(f"  GET {path} failed: {exc}")
            return None
        if status != 200:
            print(f"  GET {path} -> HTTP {status} {str(body)[:200]}")
            return None
        return body

    def server_config(self):
        return self._get("/v1/config")

    def stats(self):
        return {name: self._get(f"/v1/stats/{name}")
                for name in ("summary", "by_type", "retrievals", "store")}

    def selftest(self):
        """Prove the engine compresses AND retains, without spending a token.

        Two shapes, both of which real codex traffic is full of: ripgrep output
        and a distinct-line log. Prose is deliberately not used -- a payload the
        engine correctly declines is indistinguishable from a broken pipeline.
        """
        grep = "\n".join(
            f"src/module_{i // 20}/file_{i % 20}.rs:{i + 3}:"
            f"    let handler = Handler::new(config.clone(), registry);"
            for i in range(400))
        log = "\n".join(
            f"2026-09-18 10:{i // 60:02d}:{i % 60:02d} DEBUG [req-{i:05d}] "
            f"resolved route /api/v{i % 3}/items/{i} in {i % 97}ms "
            f"cache={'hit' if i % 3 else 'miss'}" for i in range(700))
        out = {}
        for label, payload in (("grep", grep), ("log", log)):
            try:
                status, data, _ = http_json(
                    f"{self.local}/v1/compress/raw", timeout=120,
                    headers={"x-kompressor-mode": self.cfg.mode},
                    payload={"content": payload})
            except ConnectionError as exc:
                print(f"  self-test {label}: request failed: {exc}")
                out[label] = None
                continue
            if status != 200:
                print(f"  self-test {label}: HTTP {status} {str(data)[:200]}")
                out[label] = None
                continue
            stats = data.get("stats") or {}
            chars_in, chars_out = len(payload), len(data.get("content") or "")
            entry = {"chars_in": chars_in, "chars_out": chars_out,
                     "removed_pct": (1 - chars_out / chars_in) if chars_in else 0.0,
                     "refs_emitted": stats.get("refs_emitted"),
                     "ref_bytes_emitted": stats.get("ref_bytes_emitted"),
                     "by_type": stats.get("by_type")}
            out[label] = entry
            print(f"  self-test {label:<5} {chars_in:>7,} -> {chars_out:>6,} chars "
                  f"({fmt_pct(entry['removed_pct'])} removed)  "
                  f"refs={entry['refs_emitted']}  type={entry['by_type']}")
        return out

    def assert_selftest(self, result):
        problems = []
        for label, entry in result.items():
            if entry is None:
                problems.append(f"{label}: the compress API did not answer")
                continue
            if entry["removed_pct"] < 0.20:
                problems.append(f"{label}: only {fmt_pct(entry['removed_pct'])} removed "
                                "-- the engine is not compressing this shape")
            if self.cfg.mode == "recoverable" and not entry["refs_emitted"]:
                problems.append(
                    f"{label}: mode is recoverable but no handle was minted -- "
                    "elisions would be unrecoverable, which is aggressive mode "
                    "under another name")
        if problems:
            sys.exit("compression self-test failed:\n  - " + "\n  - ".join(problems))


# ------------------------------------------------------------------ preflight
def probe(cfg):
    """The smallest real call each provider accepts, and how to authenticate it.

    Two providers, two shapes: Anthropic takes `/v1/messages` with `x-api-key`
    and a `max_tokens` it refuses to default, OpenAI takes `/v1/responses` with
    a bearer. A preflight written for one reports the other as broken.
    """
    if cfg.provider == "anthropic":
        return (
            "/v1/messages",
            {"model": cfg.model, "max_tokens": 16,
             "messages": [{"role": "user", "content": "reply with the single word: ok"}]},
            {"x-api-key": cfg.api_key, "anthropic-version": "2023-06-01"},
        )
    return (
        "/v1/responses",
        {"model": cfg.model, "input": "reply with the single word: ok",
         "max_output_tokens": 16, "store": False},
        {"authorization": f"Bearer {cfg.api_key}"},
    )


def preflight_upstream(cfg):
    """One tiny call straight at the provider: proves the key works and the
    model name is real, before a suite spends an hour finding out."""
    path, body, headers = probe(cfg)
    url = cfg.upstream.rstrip("/") + path
    try:
        status, data, _ = http_json(url, payload=body, timeout=120, headers=headers)
    except ConnectionError as exc:
        sys.exit(f"upstream preflight could not reach {url}: {exc}")
    if status != 200:
        sys.exit(f"upstream preflight {url} -> HTTP {status} "
                 f"{redact(str(data)[:400])}")
    print(f"  upstream preflight  {url} -> 200")
    return True


def preflight_through_proxy(krs, cfg):
    """The same call through the proxy, from the address the agent will use.

    This is the only check that exercises routing, auth passthrough and the
    upstream mount together. A 404 here is the `path_prefix` mistake; a 401 is
    forward_auth; a 502 is the upstream URL.
    """
    path, body, headers = probe(cfg)
    # `agent_base` already ends in /v1, which is the shape an OpenAI client is
    # given; the probe path carries its own /v1, so one of them has to go.
    url = krs.agent_base.rstrip("/").removesuffix("/v1") + path
    try:
        status, data, head = http_json(url, payload=body, timeout=120, headers=headers)
    except ConnectionError as exc:
        sys.exit(f"through-proxy preflight could not reach {url}: {exc}")
    if status != 200:
        sys.exit(f"through-proxy preflight {url} -> HTTP {status} "
                 f"{redact(str(data)[:400])}")
    mode = {k.lower(): v for k, v in head.items()}.get("x-kompressor-mode")
    print(f"  proxy preflight     {url} -> 200  x-kompressor-mode={mode}")
    if mode is None:
        sys.exit("the proxy answered but set no x-kompressor-mode header -- "
                 "the request did not go through the compression path")
    lower = {k.lower(): v for k, v in head.items()}
    return {"status": status, "mode": mode,
            "saved_tokens": lower.get("x-kompressor-saved-tokens")}


# ---------------------------------------------------------------------- harbor
def harbor_run(krs, cfg):
    os.makedirs(cfg.out_dir, exist_ok=True)
    subs = {"model": cfg.model, "openai_model": f"openai/{cfg.model}",
            "dataset": cfg.dataset, "out_dir": cfg.out_dir,
            "base_url": krs.agent_base,
            # Without the `/v1`. An Anthropic client appends `/v1/messages`
            # itself, so handing it the OpenAI-shaped base produces `/v1/v1/…`.
            "base_url_root": krs.agent_base.rstrip("/").removesuffix("/v1"),
            "proxy": f"http://{cfg.advertise_host}:{cfg.port}",
            "advertise": cfg.advertise_host, "vk": cfg.api_key,
            "upstream": cfg.upstream}
    try:
        cmd_str = cfg.harbor_cmd.format(**subs)
    except (KeyError, IndexError, ValueError) as exc:
        # Loudly. This used to fall back to the unsubstituted string, which
        # does not fail — it runs, with the literal text `{vk}` as the API key
        # and `{out_dir}` as a path, and the first sign is a provider 401 an
        # hour later. One unknown placeholder is a typo, and a typo here is
        # worth a stopped run rather than a spent one.
        sys.exit(f"--harbor-cmd has a placeholder this script cannot fill: {exc}\n"
                 f"  known: {', '.join(sorted(subs))}")
    cmd_str = cmd_str.replace("\\\r\n", " ").replace("\\\n", " ")
    cmd = shlex.split(cmd_str)
    if not cmd:
        sys.exit("--harbor-cmd is empty")
    if not (shutil.which(cmd[0]) or os.path.exists(cmd[0])):
        sys.exit(f"`{cmd[0]}` not found -- fix --harbor-cmd.")

    run_env = dict(os.environ)
    if cfg.provider == "anthropic":
        run_env.update({
            "ANTHROPIC_BASE_URL": krs.agent_base.rstrip("/").removesuffix("/v1"),
            "ANTHROPIC_API_KEY": cfg.api_key,
        })
    else:
        run_env.update({"OPENAI_API_BASE": krs.agent_base,
                        "OPENAI_BASE_URL": krs.agent_base,
                        "OPENAI_API_KEY": cfg.api_key})
    print(f"\n=== kompressor-rs {cfg.mode.upper()} via Harbor ===")
    agent_base = (krs.agent_base.rstrip("/").removesuffix("/v1")
                  if cfg.provider == "anthropic" else krs.agent_base)
    print(f"  agent  -> {cfg.provider} base_url={agent_base}  "
          f"key=...{cfg.api_key[-6:]}  model={cfg.model}")
    # From the effective config, not from `cfg`: this line hardcoded "auto" and
    # so contradicted a run that had deliberately set something else --
    # printing `retrieve_tool=auto` while the proxy was running `always`, on the
    # very setting the run existed to test.
    tool = re.search(r'^retrieve_tool\s*=\s*"([^"]+)"', krs.effective or "", re.M)
    print(f"  proxy  -> upstream {cfg.upstream}  mode={cfg.mode}  "
          f"retrieve_tool={tool.group(1) if tool else 'unknown'}")
    print("  $ " + cmd_str)
    start = int(time.time())
    subprocess.run(cmd, env=run_env, check=False)
    end = int(time.time())
    return cfg.out_dir, start, end, cmd_str


# ------------------------------------------------------------------ efficiency
def per_request_rows(stats_db, start_ms, end_ms):
    """proxy_requests joined to the compression event that produced them.

    The join key is `request_id`, NOT the `compression_event_id` column.
    That column is declared as a foreign key in the schema and is deliberately
    never written (kompressor-stats/src/writer.rs: the rowid it would point at
    may not exist yet, because the proxy can finish a request before the
    compression event it queued has been committed). Joining on it returns
    NULL for every row and reports a fully working run as "nothing was
    compressed".
    """
    if not os.path.exists(stats_db):
        return []
    con = sqlite3.connect(f"file:{stats_db}?mode=ro", uri=True)
    con.row_factory = sqlite3.Row
    # Schema v11 split the cache write by TTL. A run recorded before it has no
    # such column, and reads as "no split reported" rather than failing.
    columns = {row[1] for row in con.execute("PRAGMA table_info(proxy_requests)")}
    write_1h = ("pr.cache_write_1h_tokens" if "cache_write_1h_tokens" in columns
                else "NULL AS cache_write_1h_tokens")
    try:
        rows = con.execute(
            f"""
            SELECT pr.time_ms, pr.request_id, pr.model, pr.status, pr.stream,
                   pr.prompt_tokens, pr.completion_tokens, pr.cache_read_tokens,
                   pr.cache_write_tokens, {write_1h},
                   pr.retrieve_rounds, pr.retrieve_tokens_injected,
                   pr.upstream_ms, pr.total_ms, pr.error,
                   oe.mode, oe.tokens_in, oe.tokens_out, oe.chars_in, oe.chars_out,
                   oe.messages_total, oe.messages_compressed, oe.messages_skipped,
                   oe.frozen_messages, oe.skipped_by_reason, oe.outcome,
                   oe.skipped_tokens_by_reason, pr.conversation,
                   oe.refs_emitted, oe.ref_bytes_emitted, oe.content_types,
                   oe.engine_ms
              FROM proxy_requests pr
              LEFT JOIN optimization_events oe
                     ON oe.request_id = pr.request_id AND oe.surface = 'proxy'
             WHERE pr.time_ms BETWEEN ? AND ?
             ORDER BY pr.time_ms
            """, (start_ms, end_ms)).fetchall()
    except sqlite3.Error as exc:
        print(f"  could not read {stats_db}: {exc}")
        return []
    finally:
        con.close()
    return [dict(row) for row in rows]


def abandoned_rows(stats_db, start_ms, end_ms):
    """Compression events with no proxy_requests row: requests the proxy
    compressed and forwarded, and that never completed.

    The proxy writes its `proxy_requests` row at the END of a request, so a
    client that disconnects mid-stream leaves no row at all. Counting only
    `proxy_requests` therefore reports a run full of timeouts as "41 requests,
    all 200, no errors" -- which is what the first smoke did.
    """
    if not os.path.exists(stats_db):
        return []
    con = sqlite3.connect(f"file:{stats_db}?mode=ro", uri=True)
    con.row_factory = sqlite3.Row
    try:
        rows = con.execute(
            """
            SELECT oe.time_ms, oe.request_id, oe.model, oe.tokens_in, oe.tokens_out,
                   oe.refs_emitted, oe.content_types
              FROM optimization_events oe
              LEFT JOIN proxy_requests pr ON pr.request_id = oe.request_id
             WHERE oe.surface = 'proxy' AND pr.request_id IS NULL
               AND oe.time_ms BETWEEN ? AND ?
             ORDER BY oe.time_ms
            """, (start_ms, end_ms)).fetchall()
    except sqlite3.Error:
        return []
    finally:
        con.close()
    return [dict(row) for row in rows]


def retrieval_rows(stats_db, start_ms, end_ms):
    if not os.path.exists(stats_db):
        return []
    con = sqlite3.connect(f"file:{stats_db}?mode=ro", uri=True)
    con.row_factory = sqlite3.Row
    try:
        # `withheld` is schema v10: an ask the per-request token budget refused
        # before the store was read. Probed rather than selected outright
        # because the `except` below answers a missing column with an empty
        # list, which would report a run's retrievals as zero -- a worse lie
        # than the one this column exists to end.
        has_withheld = bool(con.execute(
            "SELECT COUNT(*) FROM pragma_table_info('retrieval_events') "
            "WHERE name = 'withheld'").fetchone()[0])
        withheld = "withheld" if has_withheld else "0 AS withheld"
        rows = con.execute(
            "SELECT time_ms, request_id, ref, found, expired, ranged, "
            f"fragment_chars, fragment_tokens, returned_chars, returned_tokens, {withheld} "
            "FROM retrieval_events WHERE time_ms BETWEEN ? AND ? ORDER BY time_ms",
            (start_ms, end_ms)).fetchall()
    except sqlite3.Error:
        return []
    finally:
        con.close()
    return [dict(row) for row in rows]


# How much of a run must report zero usage before it stops being noise.
# One preflight probe in 32 requests (3%) is not a finding; the quota failure
# this guards against took out 17 of 20.
ZERO_USAGE_SHARE = 0.10


def cache_losses(rows):
    """Turns where the provider served less of the prefix from cache than it
    did the turn before.

    If the prompt prefix only ever grows, its cached part can only grow too.
    When the cached part shrinks, everything after the first missing byte is
    billed at full price again, and nothing in the run says so: the request
    succeeds, the agent is unaffected, and the only trace is a cost that no
    longer compares with the run beside it. That is the definition of a
    quiet-invalid condition, and it was the one not being checked.

    **Two causes were found in this proxy and fixed; a third is the provider's.**
    Each was settled by diffing the captured bodies at the losing turn, and none
    of them by reasoning from these numbers -- several confident readings taken
    from aggregates alone had to be retracted first.

    * *The tools block appearing mid-conversation.* `retrieve_tool = "auto"`
      used to offer `optscale_kompressor_retrieve` on the turn a handle first appeared.
      A tools block is part of the prompt the provider caches, so it moved. At
      one measured turn the conversation was byte-identical and the agent's own
      tools unchanged while ours went from 10,622 bytes without the tool to
      13,207 with it. Fixed: the tool is offered from the first request of a
      recoverable conversation.
    * *A keep-inline promotion rewriting history.* Turning a marker the provider
      has already cached back into full text rewrites the prompt at that
      message. Measured: a 175-token fragment promoted 17% into the prompt cost
      6,144 tokens. Priced now, and refused when the break cannot pay back.
    * *The provider.* A turn whose tools, instructions and conversation all diff
      clean against the turn before, and the cache fell anyway. Seen dropping
      the whole prefix (16,768 -> 0, recovering the next turn to 19,840) and
      dropping 1,024.

    **The magnitude does not identify the cause.** `11,648 -> 10,624` was the
    tools block five times and the provider once, after the fix, with every byte
    we sent identical. Diff, do not infer.

    **A rewrite is billed twice where the provider bills cache writes.** The
    tokens that stopped being cache reads are the same tokens the next request
    has to write into the cache again, and on Anthropic a write costs 12.5x a
    read -- so the write side is the larger half. Measured on the 2026-10-07
    `claude-opus-5` run: 291 tokens lost cost $0.00131 in forfeited reads and
    $0.00182 to rewrite. Reporting only the read side understates that loss by
    about 2.4x. OpenAI does not bill writes, so there the read side is the whole
    cost and `wrote` comes back NULL.

    The re-written span is bounded, not guessed: it is at least `lost` (the part
    that was cached and no longer is) and at most the turn's whole
    `cache_write_tokens` (which also covers content that was new regardless).
    `lost` is the marginal cost of the loss and is what the report prices.

    Two exclusions, both load-bearing:

    * **Grouped by conversation.** Two conversations interleaved on one proxy
      produce a sawtooth that is not a loss; the second one's cold start is not
      the first one's eviction.
    * **The row before must have run no retrieve rounds.** A retrieve-round row
      aggregates every submission of that turn, so its `cache_read_tokens` is a
      sum over several calls and the next ordinary turn is always lower. Run A
      of the 2026-10-05 smokes had two such rows and no real loss; counting them
      would have reported two.
    * **The later row must not hold fewer messages.** A conversation's history
      only grows, so a request carrying fewer messages than the one before it
      is a different agent session filed under the same fingerprint -- which is
      the prefix tracker's hash of the conversation head, and two sessions
      whose heads agree collide. Measured on the 89-task run of 2026-10-13: one
      comparison went `messages 92 -> 5` and reported 27,008 tokens lost and
      attributed them to us, a third of that run's OURS total, for a second
      session's cold start. 21 of 88 conversations on the run before contained
      such a boundary, so the shape is not rare. The grouping above catches two
      *interleaved* conversations; this catches two that share an id.
    """
    out = []
    by_conversation = {}
    for row in rows:
        key = row.get("conversation")
        if key:
            by_conversation.setdefault(key, []).append(row)
    for key, group in by_conversation.items():
        group = sorted(group, key=lambda r: r.get("time_ms") or 0)
        # The baseline is the last row whose `cache_read_tokens` means one
        # request, which is not always the row immediately before. A
        # retrieve-round row sums its submissions, so it can be neither compared
        # against nor compared from -- but it can be *stepped over*. The prefix
        # only grows across it (a round appends the fetched fragment and
        # re-submits), so a later ordinary row reading less than an earlier one
        # has lost cache whatever happened in between. Skipping the turn
        # entirely, which this used to do, was the coarser answer: of sixteen
        # measured runs one had its first-compression turn masked exactly that
        # way, and reported a clean run it had not earned.
        previous = None
        for current in group:
            if current.get("retrieve_rounds"):
                continue
            if previous is None:
                previous = current
                continue
            # A session boundary, not a loss: see the third exclusion above.
            # `messages_total` is NULL on a row the engine never saw, and an
            # unknown count must not be read as a drop.
            before_n = previous.get("messages_total")
            now_n = current.get("messages_total")
            if before_n is not None and now_n is not None and now_n < before_n:
                previous = current
                continue
            was = previous.get("cache_read_tokens")
            now = current.get("cache_read_tokens")
            # `now == 0` is the *largest* possible loss, not the absence of one,
            # and an earlier version of this skipped it with a falsy test --
            # `not now` reads a total prefix loss as nothing to report. Measured
            # 2026-10-06: a run went 11,648 -> 0 at its first-compression turn
            # and this function returned no losses at all for it, which was then
            # read as the run being clean. NULL still means unknown and is
            # skipped; zero means zero.
            if was is None or now is None or not was or now >= was:
                previous = current
                continue
            out.append({
                "conversation": key,
                "before_request_id": previous.get("request_id"),
                "request_id": current.get("request_id"),
                "was": was,
                "now": now,
                "lost": was - now,
                "gap_ms": (current.get("time_ms") or 0) - (previous.get("time_ms") or 0),
                # What that turn then wrote back into the cache. NULL where the
                # provider does not bill writes at all (OpenAI reports none);
                # a number where it does (Anthropic reports one on every row).
                "wrote": current.get("cache_write_tokens"),
            })
            previous = current
    return sorted(out, key=lambda loss: -loss["lost"])


# What a re-send costs relative to a first send.
#
# An agent re-sends its whole history every turn, so a byte is fresh input once
# and a cache read on every later request -- and a cache read is a tenth of the
# input rate on both providers measured here (gpt-5.5 $5.00 fresh against $0.50
# cached; claude-opus-5 $5.00 against $0.50). Summing `tokens_in` across
# requests therefore prices a byte that survived 300 turns at 300 first-sends
# when it cost about 31.
#
# Borrowed, constant and all, from the OptScale kompressor's `bench/metrics.py`,
# which met this first and wrote down why: "one carried 261 times costs 27
# first-sends, not 261". A pricing assumption rather than a property of the
# run, which is why it is a constant here and the unweighted total is still
# reported beside it.
CACHE_READ_RATE = 0.1

# A conversation's prompt only grows. A drop past this fraction is a different
# agent session filed under the same fingerprint -- measured on the 89-task run
# of 2026-10-12, where 21 of 88 conversations contained one. Segmenting at the
# drop keeps each session's first send counted as a first send; without it the
# second session's content reads as a re-send of the first and is priced at a
# tenth of what it cost.
SESSION_BREAK = 0.5


def billed_tokens(rows, field, provider="openai"):
    """`field` summed with re-sends priced at the cache-read rate.

    This is a monotonic-history estimate, not a provider billing ledger:
    for one monotonic session, the content new at turn i is re-sent (N - i)
    times, so the weighted total is `unique + (flat - unique) * rate` -- the
    same number a per-byte walk would reach, without needing byte identity the
    stats do not record.

    Returns `(weighted, flat, unique)`. `unique` is the first-send content:
    what the conversation held at its largest, summed over sessions.
    """
    sessions = []
    by_conversation = {}
    for row in rows:
        if row.get(field) is None:
            continue
        by_conversation.setdefault(row.get("conversation") or "", []).append(row)
    for group in by_conversation.values():
        group = sorted(group, key=lambda r: r.get("time_ms") or 0)
        current = []
        for row in group:
            def prompt_size(item):
                size = item.get("prompt_tokens") or 0
                if provider == "anthropic":
                    size += (item.get("cache_read_tokens") or 0) + (item.get("cache_write_tokens") or 0)
                return size

            prompt = prompt_size(row)
            previous = prompt_size(current[-1]) if current else 0
            count = row.get("messages_total")
            old_count = current[-1].get("messages_total") if current else None
            # Counts, when present, distinguish a new session from a cache
            # transition. Anthropic's uncached remainder alone cannot do so.
            boundary = (count < old_count if count is not None and old_count is not None
                        else bool(previous and prompt < previous * SESSION_BREAK))
            if current and boundary:
                sessions.append(current)
                current = []
            current.append(row)
        if current:
            sessions.append(current)
    weighted = flat = unique = 0.0
    for session in sessions:
        values = [(r.get("tokens_in") if field == "tokens_out" and r.get("outcome") == "discarded"
                   else r[field]) or 0 for r in session]
        total, peak = sum(values), max(values)
        flat += total
        unique += peak
        weighted += peak + (total - peak) * CACHE_READ_RATE
    return weighted, flat, unique


def efficiency(rows, retrievals, abandoned=(), provider="openai"):
    """What compression did over the window, from the per-request rows."""
    compressed = [r for r in rows if r.get("tokens_in") is not None]
    # A rewrite the proxy threw away is not a saving. The engine's row is
    # written when the engine finishes; the proxy then serializes the result
    # and forwards the ORIGINAL if it came out no smaller, marking the row
    # `outcome = 'discarded'`. The `v_daily_savings` view has excluded those
    # since the v3 migration -- this script read the raw table and so kept
    # claiming them. On the 2026-09-27 Anthropic smoke that was the entire
    # headline: "removed 695 tokens / saved net 695" for a request that went
    # to the provider byte-for-byte as it arrived.
    #
    # `tokens_in` still counts it, because the engine did look at those tokens;
    # only the output is held to what actually left the machine.
    discarded = [r for r in compressed if (r.get("outcome") or "") == "discarded"]
    discarded_tokens = sum((r["tokens_in"] or 0) - (r["tokens_out"] or 0) for r in discarded)
    tokens_in = sum(r["tokens_in"] or 0 for r in compressed)
    tokens_out = sum(
        (r["tokens_in"] if (r.get("outcome") or "") == "discarded" else r["tokens_out"]) or 0
        for r in compressed
    )
    chars_in = sum(r["chars_in"] or 0 for r in compressed)
    chars_out = sum(
        (r["chars_in"] if (r.get("outcome") or "") == "discarded" else r["chars_out"]) or 0
        for r in compressed
    )
    # The same two totals with re-sends priced as cache reads. Reported beside
    # the flat ones, never instead of them: the flat sum is what the engine
    # looked at and the weighted one is what that work was worth, and a reader
    # comparing two runs needs to know which they are holding.
    billed_in, _, unique_in = billed_tokens(compressed, "tokens_in", provider)
    billed_out, _, unique_out = billed_tokens(compressed, "tokens_out", provider)
    losses = cache_losses(rows)
    prompt = sum(r["prompt_tokens"] or 0 for r in rows)
    completion = sum(r["completion_tokens"] or 0 for r in rows)
    cache_read = sum(r["cache_read_tokens"] or 0 for r in rows)
    # `r["..."]` rather than `r.get(...)`: this column was absent from the
    # SELECT for the life of the billed-prompt formula below, and `.get`
    # answered `None` for every row, so the third term was silently always
    # zero. Measured on the 2026-09-27 smoke: 951,357 reported against a true
    # 1,001,321, which printed the cache-read share as 100.0% where it was
    # 95.0% -- the one number the whole formula exists to produce. A KeyError
    # would have said so on the first run.
    cache_write = sum(r["cache_write_tokens"] or 0 for r in rows)
    # The two providers mean different things by their input counter, and the
    # billed prompt has to be assembled differently for each.
    #
    # OpenAI's `prompt_tokens` is the whole billed prompt, cached part
    # included. Anthropic's `input_tokens` is the *uncached remainder* — 16
    # tokens against a 41,116-token cache read — so its billed prompt is the
    # sum of the three input counters.
    #
    # Getting this wrong is visible in both directions, which is the only
    # reason it was caught: dividing by the remainder printed `billed prompt
    # 480 tokens, of which 2,133,628 were provider cache reads (444505.8%)` on
    # Anthropic, and summing unconditionally halved a known-good OpenAI run
    # from 95.1% to 48.8% by counting its cached tokens twice.
    billed_prompt = prompt + cache_read + cache_write if provider == "anthropic" else prompt
    injected = sum(r["retrieve_tokens_injected"] or 0 for r in rows)
    rounds = sum(r["retrieve_rounds"] or 0 for r in rows)
    refs = sum(
        0 if (r.get("outcome") or "") == "discarded" else (r["refs_emitted"] or 0)
        for r in compressed
    )
    # Since the 2026-09-18 fix, a stream the client walked out on DOES get a
    # row, carrying an `error` note and usually no usage. Before it, such a
    # request left only a compression event -- which is what `abandoned` counts.
    # Both shapes are reported, because a dump may have been taken either side
    # of that change and "abandoned" must not quietly become "failed".
    walked_out = [r for r in rows if r.get("error")
                  and "disconnect" in str(r["error"]).lower()]
    errors = [r for r in rows if r.get("error") and r not in walked_out]
    bad_status = [r for r in rows if r.get("status") and int(r["status"]) >= 400]
    # A 200 that produced no output at all. Streaming providers send headers
    # before they know they will refuse, so a quota or policy refusal arrives
    # as an `error` frame INSIDE a 200 -- invisible to `status` and to the
    # relay, which reads a well-formed frame and ends the round normally.
    # Measured 2026-09-29: a quota-exhausted run reported 143/143 OK and
    # "no quiet-invalid conditions detected" while 17 of 20 trials died. The
    # same predicate fires exactly once on the healthy 637-request re-run, and
    # that once is the client-disconnect already reported above -- which is
    # why `walked_out` is excluded rather than lumped in.
    # `prompt_tokens` is the discriminator, not the size of anything. A refusal
    # inside the stream reports NO usage at all, because the provider stopped
    # before doing work: all 17 of the quota run's zero-output rows carry NULL.
    # A real request that simply had nothing to say still reports what it read
    # -- Claude Code's 2-token startup ping came back 200 with `prompt_tokens =
    # 2`, and flagging that as a silent failure is how a check starts crying
    # wolf.
    # NULL and 0 are different answers and the predicate used to read them as
    # one. `prompt_tokens` is an `Option<u32>` written from the response's own
    # usage: NULL means no usage reached the proxy at all, which is the quota
    # signature described above, while an explicit 0 means a usage object was
    # parsed and reported nothing. Measured 2026-10-06: one preflight probe of
    # 32 requests came back 200 with 0/0 -- non-streaming, 1.3s like every
    # healthy run's probe, with the other 31 requests normal and the task
    # resolved. Calling that "the agent got nothing" is how a check starts
    # crying wolf, and the failure this exists for was 17 of 20 trials dead.
    # A row that names no model AND that the engine never parsed is not a
    # completion, so it has no usage to be missing. Measured 2026-10-14 on an
    # 1,893-request run: one request arrived with an empty body, was forwarded,
    # and came back 200 in 1.1s with every other column NULL -- and this check
    # reported "the agent got nothing" for the whole run on the strength of it.
    # Neither field alone is the discriminator. Of the two requests that run's
    # engine never saw, the other named a model and reported usage; and a quota
    # refusal always names the model, because the model is read from the
    # *request* and the refusal happens after. Requiring either keeps the
    # signature this check exists for and drops the bodyless pass-through.
    answered_nothing = [r for r in rows
                        if int(r.get("status") or 0) == 200
                        and r.get("completion_tokens") is None
                        and r.get("prompt_tokens") is None
                        and (r.get("model") or r.get("messages_total") is not None)
                        and not r.get("error")]
    zero_usage = [r for r in rows
                  if int(r.get("status") or 0) == 200
                  and r.get("completion_tokens") == 0
                  and r.get("prompt_tokens") == 0
                  and r not in walked_out]
    types = {}
    for row in compressed:
        try:
            for name, count in json.loads(row["content_types"] or "{}").items():
                types[name] = types.get(name, 0) + count
        except (TypeError, ValueError):
            pass
    # Which of the two ceilings refused. `Budget::is_spent()` is an OR --
    # `rounds >= max_rounds || tokens >= max_tokens` -- and the report named
    # only the token one, every time, whichever had fired.
    #
    # They are told apart by where the request stopped. A refusal by the round
    # cap leaves `retrieve_rounds` ONE PAST the cap, because the refusing round
    # is still charged after it is built; a refusal by the token ceiling
    # happens inside a round the budget had not yet spent, so its rounds stay
    # at or below the cap. Measured across the three full runs of 2026-10-12,
    # -13 and -14: 30 of 32 refusals were the round cap, and the line sent
    # every reader to `retrieve_max_tokens_per_request`, where raising the
    # number changes nothing.
    #
    # Recorded here as one rounds-value per withheld ask rather than as a
    # verdict, because the cap it has to be compared against lives in the
    # effective config, which this function is not given.
    rounds_by_request = {r.get("request_id"): r.get("retrieve_rounds") or 0
                         for r in rows}
    withheld_rounds = [rounds_by_request.get(r.get("request_id"), 0)
                       for r in retrievals if r.get("withheld")]
    returned = sum(r["returned_tokens"] or 0 for r in retrievals)
    return {
        "requests": len(rows),
        "requests_abandoned": len(abandoned) + len(walked_out),
        "requests_abandoned_no_row": len(abandoned),
        "requests_abandoned_with_row": len(walked_out),
        "requests_attempted": len(rows) + len(abandoned),
        "requests_compressed": len(compressed),
        "requests_discarded": len(discarded),
        "discarded_tokens": discarded_tokens,
        "frozen_messages": sum(r.get("frozen_messages") or 0 for r in compressed),
        "messages_total": sum(r.get("messages_total") or 0 for r in compressed),
        "skipped_by_reason": _skip_reasons(compressed, "skipped_by_reason"),
        "skipped_tokens_by_reason": _skip_reasons(compressed, "skipped_tokens_by_reason"),
        "conversations": len({r["conversation"] for r in rows if r.get("conversation")}),
        "retrieval_withheld_rounds": withheld_rounds,
        "requests_answered_nothing": len(answered_nothing),
        "requests_answered_nothing_streamed": sum(
            1 for r in answered_nothing if r.get("stream")),
        "requests_zero_usage": len(zero_usage),
        "requests_failed": len(errors) + len(bad_status),
        "status_counts": _counts(r.get("status") for r in rows),
        "tokens_in": tokens_in, "tokens_out": tokens_out,
        "saved_gross": tokens_in - tokens_out,
        "reduction_pct": (1 - tokens_out / tokens_in) if tokens_in else None,
        "billed_in": billed_in, "billed_out": billed_out,
        "billed_reduction_pct": (1 - billed_out / billed_in) if billed_in else None,
        "unique_in": unique_in, "unique_out": unique_out,
        "unique_reduction_pct": (1 - unique_out / unique_in) if unique_in else None,
        "chars_in": chars_in, "chars_out": chars_out,
        "prompt_tokens": prompt, "completion_tokens": completion,
        "cache_read_tokens": cache_read,
        "cache_write_tokens": cache_write,
        "billed_prompt": billed_prompt,
        "cache_read_share": (cache_read / billed_prompt) if billed_prompt else None,
        "cache_losses": losses,
        "cache_lost_tokens": sum(loss["lost"] for loss in losses),
        # Where the provider bills cache writes, the same tokens are paid for
        # twice: forfeited as reads and written again. NULL `wrote` means the
        # provider does not bill writes, not that nothing was rewritten.
        "cache_lost_rewritten": sum(loss["lost"] for loss in losses
                                    if loss.get("wrote") is not None),
        "refs_emitted": refs,
        "retrieve_rounds": rounds,
        "retrieve_tokens_injected": injected,
        # Refusals are counted apart from every rate, not inside them. A
        # withheld ask never reached the store, so counting it as a retrieval
        # makes the hit rate divide by asks the store was never given, and the
        # miss it implies never happened. Measured on the Harbor run of
        # 2026-10-09 15:56, which reported `12 (11 hits)` for twelve asks of
        # which eleven were served and all eleven hit.
        "retrievals": sum(1 for r in retrievals if not r.get("withheld")),
        "retrieval_hits": sum(1 for r in retrievals
                              if r.get("found") and not r.get("withheld")),
        "retrieval_ranged": sum(1 for r in retrievals
                                if r.get("ranged") and not r.get("withheld")),
        "retrieval_withheld": sum(1 for r in retrievals if r.get("withheld")),
        "retrieved_tokens": returned,
        # Keep the legacy field for consumers; neither measure is dollar savings.
        "saved_net": (tokens_in - tokens_out) - returned,
        "saved_after_injection": (tokens_in - tokens_out) - injected,
        "content_types": types,
    }


def _skip_reasons(rows, column="skipped_by_reason"):
    """The partition of skipped messages, summed over the window.

    Printed because "0.3% of compressible tokens removed" is unreadable without
    it: the denominator is every token the engine was SHOWN, and on a Claude
    Code run almost all of them were frozen before it could touch one.
    """
    out = {}
    for row in rows:
        try:
            for reason, count in json.loads(row.get(column) or "{}").items():
                out[reason] = out.get(reason, 0) + count
        except (TypeError, ValueError):
            pass
    return dict(sorted(out.items(), key=lambda kv: -kv[1]))


def _counts(values):
    out = {}
    for value in values:
        key = str(value)
        out[key] = out.get(key, 0) + 1
    return dict(sorted(out.items()))


def validity_flags(eff, acc, selftest, mode="recoverable"):
    """The quiet-invalid modes, named. Each one is a run that completes and
    reports a number that means nothing.

    `mode` because the control arm compresses nothing *on purpose*, and a flag
    that fires on it is worse than no flag: it says NO COMPRESSION in the one
    run where that is the point, and names `api_family` as the cause, which is
    not what happened. A reader who has to know which flags to disbelieve is
    reading a report that cannot be trusted anywhere.

    The guard is on the whole group and not on one branch of it. Excluding
    `off` from NO COMPRESSION alone just drops the control arm through to
    NEAR-ZERO COMPRESSION, which fires on the same runs for the same reason and
    prints "n/a" where its percentage should be -- the flag moves, it does not
    go away.
    """
    flags = []
    if eff["requests"] == 0:
        flags.append("NO PROXY TRAFFIC: the agent never reached the proxy -- "
                     "whatever this run measured, it was not compression")
    elif mode == "off":
        # A control arm that compressed something would be the broken one.
        if eff["requests_compressed"]:
            flags.append(
                f"CONTROL ARM COMPRESSED: {eff['requests_compressed']} request(s) "
                "went through the engine -- this is not a baseline")
    elif eff["requests_compressed"] == 0:
        flags.append("NO COMPRESSION: every request was relayed unparsed "
                     "(check api_family on the upstream mount)")
    elif (eff["reduction_pct"] or 0) < 0.01:
        flags.append(f"NEAR-ZERO COMPRESSION: {fmt_pct(eff['reduction_pct'])} of "
                     "compressible tokens removed")
    if eff.get("requests_abandoned"):
        detail = []
        if eff.get("requests_abandoned_with_row"):
            detail.append(f"{eff['requests_abandoned_with_row']} recorded with a "
                          "client-disconnect note")
        if eff.get("requests_abandoned_no_row"):
            detail.append(f"{eff['requests_abandoned_no_row']} left no "
                          "proxy_requests row at all (pre-fix proxy build)")
        flags.append(
            f"{eff['requests_abandoned']} request(s) were compressed and forwarded "
            "but the client gave up mid-stream (" + "; ".join(detail) + "). "
            "Their tokens were billed and reached nobody")
    if eff["requests_failed"]:
        flags.append(f"{eff['requests_failed']} proxy requests failed or returned "
                     f">=400 ({eff['status_counts']})")
    if eff.get("requests_answered_nothing"):
        flags.append(
            f"{eff['requests_answered_nothing']} of {eff['requests']} request(s) "
            "returned HTTP 200 without reported usage and without a recorded "
            "transport error. Inspect provider error frames and agent logs; "
            "missing usage alone does not establish a quota or policy refusal. "
            "Known transport failures are counted separately above.")
    # A usage object that parsed and said zero is a weaker signal than no usage
    # at all, so it is reported only when it is widespread enough to mean
    # something. One probe in a run is noise; most of a run is the same quota
    # failure wearing a different shape.
    if eff.get("requests_zero_usage") and eff["requests"]:
        share = eff["requests_zero_usage"] / eff["requests"]
        if share >= ZERO_USAGE_SHARE:
            flags.append(
                f"{eff['requests_zero_usage']} of {eff['requests']} request(s) "
                f"({fmt_pct(share)}) returned HTTP 200 with usage reported as "
                "zero on both sides. One such request is ordinary noise; this "
                "many means the provider answered without doing the work, and "
                "every token figure below is short by whatever they would have "
                "carried")
    if eff.get("cache_losses"):
        losses = eff["cache_losses"]
        worst = losses[0]
        flags.append(
            f"PROVIDER CACHE LOSS: {len(losses)} turn(s) were served less of the "
            f"prefix from cache than the turn before, {eff['cache_lost_tokens']:,} "
            f"tokens in total (worst: {worst['lost']:,} lost "
            f"{worst['gap_ms'] / 1000:.1f}s after the previous turn, "
            f"{worst['was']:,} -> {worst['now']:,}"
            + (f", and {eff['cache_lost_rewritten']:,} of those tokens were "
               "associated with cache writes; price writes separately from reads"
               if eff.get("cache_lost_rewritten") else "")
            + "). Compare ingress and egress bodies before attributing the loss. "
            "Run `scripts/harbor/cache-loss-diff.py <run-dir>` to distinguish "
            "proxy rewrites, client changes, and unresolved cache metadata or "
            "expiry candidates. Cache-read drops alone do not establish cause "
            "or incremental billing cost.")
    if any(entry is None for entry in (selftest or {}).values()):
        flags.append("the compression self-test did not complete")
    if acc.get("counts") and acc["counts"][1] == 0:
        flags.append("Harbor recorded no trials")
    return flags


# ---------------------------------------------------------------------- report
def report(cfg, krs, acc, eff, selftest, effective, window):
    line = "=" * 78
    out = [line, "  kompressor-rs / Harbor terminal-bench run", line]
    add = out.append
    add(f"  run              {cfg.stamp}")
    add(f"  binary           {cfg.kompressor_bin}")
    add(f"  version          {cfg.kompressor_version}")
    add(f"  source           {cfg.kompressor_commit}"
        f"{' +dirty' if cfg.kompressor_dirty else ''}")
    # Read back from the effective config, not the environment. Four runs in a
    # row were taken as tests of `first_seen` and were all `marker`: the env
    # override was a silent no-op, and the only way to tell afterwards was to
    # grep the config dump. The header says what the proxy actually used.
    # The value this run's provider ran under: `proxy.providers.<provider>`
    # wins over the deployment's, so reading the first `frozen_prefix` line
    # reported `marker` on a run that was `first_seen` for every request.
    frozen = re.search(r'^frozen_prefix\s*=\s*"([^"]+)"', effective or "", re.M)
    frozen = frozen.group(1) if frozen else "unknown"
    scoped = provider_overrides(effective or "").get(cfg.provider, {}).get("frozen_prefix")
    where = f"  ({cfg.provider} only; the deployment default is {frozen})" if scoped else ""
    frozen = scoped or frozen
    add(f"  frozen prefix    {frozen}"
        + ("  (default: the client's marker freezes the tail too)"
           if frozen == "marker" else "  <-- compressing the uncached tail") + where)
    for name, settings in provider_overrides(effective or "").items():
        add(f"  {name + ' only':16} " + ", ".join(f"{k}={v}" for k, v in settings.items()))
    # The gloss describes what the mode does, so "off" cannot borrow CCR's.
    add(f"  mode             {cfg.mode}" + (
        "  (control arm: forwarded unchanged, measured, never compressed)"
        if cfg.mode == "off" else
        "  (retrieve tool OFF)" if cfg.no_retrieve_tool else
        "  (CCR: handles + server-side retrieve)"))
    add(f"  upstream         {cfg.upstream}")
    add(f"  agent base_url   {krs.agent_base}")
    add(f"  model            {cfg.model}")
    add(f"  window           {window['start_iso']} .. {window['end_iso']} "
        f"({window['minutes']:.1f} min)")
    add("")
    add("  -- accuracy " + "-" * 64)
    if acc.get("counts"):
        resolved, total = acc["counts"]
        add(f"  resolved         {resolved}/{total}"
            f"   ({fmt_pct(acc['accuracy']) if acc.get('accuracy') is not None else 'n/a'})")
    else:
        add("  resolved         n/a (no Harbor result.json found)")
    hstats = (acc.get("stats") or {})
    if hstats.get("cost_usd") is not None:
        add(f"  harbor cost      {fmt_usd(hstats['cost_usd'])}")
    if hstats.get("n_errored_trials"):
        add(f"  errored trials   {hstats['n_errored_trials']}")
    if hstats.get("n_input_tokens") is not None:
        add(f"  harbor tokens    in {hstats['n_input_tokens']:,}  "
            f"cached {hstats.get('n_cache_tokens') or 0:,}  "
            f"out {hstats.get('n_output_tokens') or 0:,}")
    add("")
    add("  -- compression (proxy's own per-request rows) " + "-" * 31)
    add(f"  proxy requests   {eff['requests']:,} completed"
        + (f", {eff['requests_abandoned']:,} abandoned mid-stream"
           if eff.get("requests_abandoned") else "")
        + f"  ({eff['requests_compressed']:,} went through the engine)")
    add(f"  compressible     {eff['tokens_in']:,} tokens -> {eff['tokens_out']:,}")
    add(f"  removed          {eff['saved_gross']:,} tokens "
        f"({fmt_pct(eff['reduction_pct']) if eff['reduction_pct'] is not None else 'n/a'} "
        "of what the engine looked at)")
    # The same work, priced. The line above sums what the engine saw on every
    # request, so a byte carried through 300 turns is counted 300 times -- true
    # of what the engine did and useless as a size. On the 89-task run of
    # 2026-10-12 it read 415,987,844 where the billed-weighted figure is
    # 45,758,861, a 9.1x overstatement, 79% of it one 302-turn conversation.
    # Both are printed because they answer different questions and the flat one
    # alone has been misread as a corpus size.
    if eff.get("billed_in"):
        add(f"  billed-weighted  {eff['billed_in']:,.0f} tokens -> "
            f"{eff['billed_out']:,.0f}  "
            f"({fmt_pct(eff['billed_reduction_pct'])} removed) "
            f"<- re-sends priced as cache reads at {CACHE_READ_RATE:g}x; "
            "compare runs on THIS, not the line above")
        add(f"  first send only  {eff['unique_in']:,.0f} tokens -> "
            f"{eff['unique_out']:,.0f}  "
            f"({fmt_pct(eff['unique_reduction_pct'])} removed) "
            "<- session-peak estimate; not unique byte tracking")
    if eff.get("requests_discarded"):
        add(f"  discarded        {eff['requests_discarded']:,} request(s), "
            f"{eff['discarded_tokens']:,} tokens the engine removed and the proxy "
            "threw away (the rewrite came out no smaller, so the original was "
            "forwarded); not counted above")
    # Without this the reduction figure is unreadable: its denominator is every
    # token the engine was SHOWN, and a client that pins its whole prompt with
    # `cache_control` leaves the engine nothing it is allowed to touch. A run
    # that compressed nothing because there was nothing to compress looks
    # identical to one that compressed nothing because it is broken.
    if eff.get("messages_total"):
        skips = eff.get("skipped_by_reason") or {}
        add(f"  messages         {eff['messages_total']:,} seen, "
            f"{eff['frozen_messages']:,} inside the frozen prefix")
        if skips:
            # Tokens beside counts, because a count cannot rank: `below_min_chars`
            # is a fifth of all skipped messages and tiny by construction, while
            # `opaque` is another fifth and unmeasurable from inside the engine.
            # A bare count makes those two look like the same problem.
            weights = eff.get("skipped_tokens_by_reason") or {}
            ranked = sorted(skips.items(), key=lambda kv: -weights.get(kv[0], 0))
            add("  skipped because  " + ", ".join(
                f"{reason} {count:,}msg"
                + (f"/{weights[reason]:,}tok" if weights.get(reason) else
                   ("/unmeasurable" if reason == "opaque" else ""))
                for reason, count in ranked))
    if eff.get("conversations"):
        add(f"  conversations    {eff['conversations']:,} distinct "
            "(group by it to get unique bytes rather than the per-turn sum)")
    add(f"  billed prompt    {eff['billed_prompt']:,} tokens, of which "
        f"{eff['cache_read_tokens']:,} were provider cache reads "
        f"({fmt_pct(eff['cache_read_share']) if eff['cache_read_share'] is not None else 'n/a'})"
        + (f", {eff['cache_write_tokens']:,} cache writes"
           if eff.get("cache_write_tokens") else ""))
    # Per request, because per run is unusable for comparing two runs. How many
    # turns the agent takes swamps everything else: two control runs of one task
    # 11 minutes apart came to $2.2234 and $1.5151. Dividing by the request
    # count removes that term, and the same four runs that need ~655 per arm to
    # separate on cost per run need ~16 on cost per request.
    #
    # It is half the comparison, not the whole one. The turn count is the other
    # half and belongs to compression too -- a run that needs more turns pays
    # for them -- so it is printed right here rather than left to be looked up.
    if eff["requests"]:
        add(f"  per request      {eff['billed_prompt'] // eff['requests']:,} billed "
            f"prompt tokens over {eff['requests']:,} request(s)"
            "  <- compare runs on this, and on the request count beside it")
    # Printed on every run, zero included. A reader comparing two runs has to
    # be able to see that one had no cache loss, and a line that only appears
    # when something is wrong cannot say that -- its absence reads the same as
    # a version of this script that never measured it.
    lost = eff.get("cache_lost_tokens") or 0
    fresh = eff["billed_prompt"] - eff["cache_read_tokens"]
    rewritten = eff.get("cache_lost_rewritten") or 0
    add(f"  cache lost       {lost:,} tokens over "
        f"{len(eff.get('cache_losses') or ()):,} turn(s)"
        + (f" -- {fmt_pct(lost / fresh)} of everything billed at full price here"
           if lost and fresh else "")
        + ("" if lost else " (the prefix only ever grew)"))
    # Printed only where it applies. On a provider that does not bill cache
    # writes the line above is the whole cost, and a second line saying "0
    # rewritten" would invite the reader to wonder what went uncounted.
    if rewritten:
        add(f"  of which rewrit  {rewritten:,} tokens were also written back into "
            "the cache; price cache writes separately from cache reads")
    add(f"  handles minted   {eff['refs_emitted']:,}")
    add(f"  retrievals       {eff['retrievals']:,} "
        f"({eff['retrieval_hits']:,} hits, {eff['retrieval_ranged']:,} narrowed) "
        f"over {eff['retrieve_rounds']:,} rounds")
    # Only where it applies, like the cache-write line above: on a run the
    # budget never bound, a "0 withheld" line invites the reader to wonder what
    # it is for. Where it is non-zero it is the one place that budget is
    # visible, and it is deliberately not folded into the counts above.
    if eff.get("retrieval_withheld"):
        # Name the ceiling that actually fired. Both are read from the
        # effective config rather than from `cfg`, for the reason the frozen
        # prefix is: an environment override reaches the proxy without passing
        # through this script. See `withheld_rounds` in `efficiency`.
        cap_rounds = re.search(r"^retrieve_max_rounds\s*=\s*(\d+)",
                               effective or "", re.M)
        cap_tokens = re.search(r"^retrieve_max_tokens_per_request\s*=\s*(\d+)",
                               effective or "", re.M)
        total = eff["retrieval_withheld"]
        # NOT `line`: that name holds the `=` banner this function opens and
        # closes the report with, and reusing it here replaced the closing
        # banner with this sentence — visible in the run of 2026-10-15 12:45,
        # which printed the withheld line twice and no bottom rule.
        withheld_line = f"  withheld         {total:,} refused before the store was read -- "
        if cap_rounds:
            limit = int(cap_rounds.group(1))
            by_rounds = sum(1 for n in eff.get("retrieval_withheld_rounds") or []
                            if n > limit)
            by_tokens = total - by_rounds
            parts = []
            if by_rounds:
                parts.append(f"{by_rounds:,} by proxy.retrieve_max_rounds ({limit})")
            if by_tokens:
                parts.append(
                    f"{by_tokens:,} by proxy.retrieve_max_tokens_per_request"
                    + (f" ({int(cap_tokens.group(1)):,})" if cap_tokens else ""))
            withheld_line += ", ".join(parts) + ". "
        else:
            # No cap to compare against: say which two it could have been
            # rather than pick one, which is the bug this replaced.
            withheld_line += ("proxy.retrieve_max_rounds or "
                              "proxy.retrieve_max_tokens_per_request was spent. ")
        add(withheld_line + "Not counted above: asks the model made and did not get")
    add(f"  put back         {eff['retrieved_tokens']:,} fragment tokens; "
        f"{eff['retrieve_tokens_injected']:,} injected tokens including overhead")
    add(f"  after injection  {eff['saved_after_injection']:,} tokens  "
        "(gross minus injected tokens; excludes extra upstream rounds and billing effects)")
    if eff["content_types"]:
        top = sorted(eff["content_types"].items(), key=lambda kv: -kv[1])[:8]
        add("  by type          " + ", ".join(f"{k} {v}" for k, v in top))
    add("")
    flags = validity_flags(eff, acc, selftest, cfg.mode)
    if flags:
        add("  !! " + "-" * 72)
        for flag in flags:
            add(f"  !! {flag}")
        add("  !! " + "-" * 72)
    else:
        add("  validity         no quiet-invalid conditions detected")
    add(line)
    return "\n".join(out)


# ------------------------------------------------------------------------ dump
def file_sha256(path):
    """Identify the executable actually used, independently of checkout HEAD."""
    try:
        digest = hashlib.sha256()
        with open(path, "rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
        return digest.hexdigest()
    except OSError:
        return None


PROVIDER_ENV = "KOMPRESSOR__PROXY__PROVIDERS__"


def provider_overrides(effective: str) -> dict:
    """`[proxy.providers.*]` from an effective config, flattened per provider:
    `{"anthropic": {"cache_ttl": "adaptive", "engine.enable_code_aware": False}}`."""
    import tomllib
    try:
        providers = tomllib.loads(effective).get("proxy", {}).get("providers", {})
    except tomllib.TOMLDecodeError:
        return {}
    out = {}
    for name, settings in providers.items():
        flat = {}
        for key, value in settings.items():
            if isinstance(value, dict):
                flat.update({f"{key}.{k}": v for k, v in value.items()})
            else:
                flat[key] = value
        out[name] = flat
    return out


def provider_override_problems(effective: str, environ) -> list:
    """Each provider-scoped override in `environ` the effective config does not hold."""
    held = provider_overrides(effective)
    problems = []
    for key, value in sorted(environ.items()):
        if not key.startswith(PROVIDER_ENV) or not value:
            continue
        name, _, setting = key[len(PROVIDER_ENV):].lower().partition("__")
        setting = setting.replace("__", ".")
        got = held.get(name, {}).get(setting)
        want = value.strip().lower()
        if got is None or str(got).lower() != want:
            problems.append(
                f'{key}="{value}" is not in the effective config (proxy.providers.{name} '
                f'has {setting} = {got!r}) -- the run would measure the default instead')
    return problems


def is_build_input(path: str) -> bool:
    """Whether a repository path can change what `cargo build` produces."""
    return path.startswith("crates/") or path in ("Cargo.toml", "Cargo.lock")


def capture_provenance(cfg):
    repo = os.path.dirname(os.path.dirname(os.path.dirname(cfg.kompressor_bin)))
    def git(*args):
        try:
            return subprocess.run(["git", "-C", repo, *args], capture_output=True,
                                  text=True, check=False).stdout.strip()
        except OSError:
            return ""
    cfg.kompressor_commit = git("rev-parse", "HEAD") or "unknown"
    cfg.kompressor_dirty = bool(git("status", "--porcelain"))
    try:
        cfg.kompressor_version = subprocess.run(
            [cfg.kompressor_bin, "version"], capture_output=True, text=True,
            check=False).stdout.strip().replace("\n", " | ")
    except OSError:
        cfg.kompressor_version = "unknown"
    # A dirty checkout's commit does not say what the source was. Hash the
    # uncommitted changes and untracked (non-ignored) files, so two runs with the
    # same commit and the same hash ran the same source; and say whether any
    # *build input* is newer than the binary, in which case the binary may not be
    # built from it -- the one thing an executable hash cannot tell you. Build
    # inputs only: a doc edited after the build flagged a binary that was
    # exactly the one built.
    digest = hashlib.sha256(git("diff", "HEAD", "--binary").encode())
    untracked = [path for path in git("ls-files", "--others", "--exclude-standard").splitlines() if path]
    newest_source = 0.0
    for path in sorted(untracked) + git("diff", "HEAD", "--name-only").splitlines():
        full = os.path.join(repo, path)
        if path in untracked:
            digest.update(path.encode())
            digest.update((file_sha256(full) or "").encode())
        if is_build_input(path) and os.path.exists(full):
            newest_source = max(newest_source, os.path.getmtime(full))
    binary_mtime = os.path.getmtime(cfg.kompressor_bin) if os.path.exists(cfg.kompressor_bin) else None
    return {"repo": repo, "commit": cfg.kompressor_commit,
            "dirty": cfg.kompressor_dirty, "version": cfg.kompressor_version,
            "source_diff_sha256": digest.hexdigest() if cfg.kompressor_dirty else None,
            "binary_older_than_source": bool(binary_mtime and newest_source > binary_mtime),
            "binary_sha256": file_sha256(cfg.kompressor_bin),
            "binary_mtime": dt.datetime.fromtimestamp(
                os.path.getmtime(cfg.kompressor_bin)).isoformat(timespec="seconds")
            if os.path.exists(cfg.kompressor_bin) else None}


def write_dump(cfg, krs, summary, rows, retrievals, abandoned=()):
    dump = cfg.out_dir
    os.makedirs(dump, exist_ok=True)
    with open(os.path.join(dump, "kompressor_rs_harbor_summary.json"), "w",
              encoding="utf-8") as handle:
        json.dump(summary, handle, indent=2, default=str)
    with open(os.path.join(dump, "proxy_requests.json"), "w", encoding="utf-8") as handle:
        json.dump(rows, handle, indent=2, default=str)
    with open(os.path.join(dump, "retrieval_events.json"), "w", encoding="utf-8") as handle:
        json.dump(retrievals, handle, indent=2, default=str)
    with open(os.path.join(dump, "abandoned_requests.json"), "w", encoding="utf-8") as handle:
        json.dump(list(abandoned), handle, indent=2, default=str)
    with open(os.path.join(dump, "REPORT.txt"), "w", encoding="utf-8") as handle:
        handle.write(summary["report"] + "\n")

    # Secrets: harbor writes the key into its own run metadata via --ae, and a
    # dump is a thing people copy off the machine.
    swept = 0
    skipped = []
    for path in glob.glob(os.path.join(dump, "**", "*"), recursive=True):
        if not os.path.isfile(path):
            continue
        if os.path.getsize(path) > 64 * 1024 * 1024:
            # Said out loud rather than skipped quietly. The cap exists so one
            # enormous artefact cannot stall the sweep, but a dump is a thing
            # people copy off the machine and the body capture that now lands
            # here holds entire prompts. A file too big to redact is a file to
            # know about.
            skipped.append((path, os.path.getsize(path)))
            continue
        if path.endswith((".db", ".sqlite", ".png", ".jpg", ".gz", ".zip")):
            continue
        try:
            if redact_file(path):
                swept += 1
        except (OSError, UnicodeDecodeError):
            pass
    print(f"  redaction sweep: {swept} file(s) rewritten")
    for path, size in skipped:
        print(f"  !! NOT REDACTED, too large to sweep: {path} ({size / 1e6:.0f} MB). "
              "Check it before copying this dump anywhere")
    return dump


# ------------------------------------------------------------------------ main
def parse_args():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--kompressor-bin", default=env("KOMPRESSOR_BIN", DEFAULT_BIN))
    p.add_argument("--upstream", default=env("UPSTREAM", "https://api.openai.com"))
    p.add_argument("--api-key", default=env("OPENAI_API_KEY") or env("VK"))
    p.add_argument("--model", default=env("MODEL", "gpt-5.5"))
    # Decides the upstream mount, the preflight shape and the assertion that
    # the mount is parsed rather than passed through. Not inferred from the
    # upstream URL: a gateway can serve either family from a host whose name
    # says nothing, and guessing wrong is the silent passthrough this whole
    # script exists to catch.
    p.add_argument("--provider", default=env("PROVIDER", "openai"),
                   choices=["openai", "anthropic"])
    p.add_argument("--dataset", default="terminal-bench/terminal-bench-2-1")
    p.add_argument("--mode", default=env("KOMPRESSOR_MODE", "recoverable"),
                   choices=["recoverable", "aggressive", "lossless", "off"])
    p.add_argument("--no-retrieve-tool", action="store_true",
                   help="do not offer optscale_kompressor_retrieve (the no-CCR arm)")
    p.add_argument("--port", type=int, default=int(env("KOMPRESSOR_PORT", "8817")))
    # Default is the docker bridge address, not 0.0.0.0: kompressor's auth
    # only *selects a tenant* -- resolve_identity falls back to anonymous on
    # failure rather than rejecting -- so the proxy cannot be closed with a
    # token the way headroom's could. Binding it to the bridge keeps it off
    # the LAN while the task containers can still reach it.
    p.add_argument("--bind-host", default=env("BIND_HOST"))
    p.add_argument("--advertise-host", default=env("ADVERTISE_HOST"))
    p.add_argument("--out-dir", required=True)
    p.add_argument("--harbor-cmd", default=env("HARBOR_CMD"))
    p.add_argument("--min-tokens", type=int, default=int(env("KOMPRESSOR_MIN_TOKENS", "512")))
    p.add_argument("--min-compress-chars", type=int,
                   default=int(env("KOMPRESSOR_MIN_CHARS", "512")))
    p.add_argument("--max-ref-bytes", type=int,
                   default=int(env("KOMPRESSOR_MAX_REF_BYTES", "65536")),
                   help="fragments larger than this are elided WITHOUT a handle, "
                        "which silently turns recoverable into aggressive for them")
    p.add_argument("--ref-ttl-seconds", type=int,
                   default=int(env("KOMPRESSOR_REF_TTL", "14400")))
    p.add_argument("--retrieve-max-rounds", type=int, default=3)
    p.add_argument("--log-level", default=env("KOMPRESSOR_LOG", "info"))
    p.add_argument("--reuse-proxy", action="store_true")
    p.add_argument("--keep-proxy", action="store_true")
    p.add_argument("--skip-harbor", action="store_true",
                   help="run every gate and stop before spending money")
    cfg = p.parse_args()
    if not cfg.api_key:
        sys.exit("no API key: pass --api-key or set OPENAI_API_KEY")
    if not os.path.exists(cfg.kompressor_bin):
        sys.exit(f"kompressor binary not found at {cfg.kompressor_bin} "
                 "(cargo build --release)")
    if not cfg.harbor_cmd and not cfg.skip_harbor:
        sys.exit("no --harbor-cmd / HARBOR_CMD")
    cfg.upstream = normalize_upstream(cfg.upstream)
    cfg.advertise_host = cfg.advertise_host or detect_advertise_host()
    cfg.bind_host = cfg.bind_host or cfg.advertise_host
    cfg.stamp = dt.datetime.now().strftime("%Y-%m-%d__%H-%M-%S")
    cfg.out_dir = os.path.join(cfg.out_dir, cfg.stamp)
    cfg.kompressor_commit = cfg.kompressor_version = "unknown"
    cfg.kompressor_dirty = False
    return cfg


def main():
    cfg = parse_args()
    os.makedirs(cfg.out_dir, exist_ok=True)
    krs = KompressorRs(cfg)

    print("=== kompressor-rs harbor run ===")
    print(f"  dump -> {cfg.out_dir}")
    provenance = capture_provenance(cfg)
    print(f"  binary {provenance['version']}")
    print(f"  source {provenance['commit'][:12]}"
          f"{' +dirty' if provenance['dirty'] else ''}")

    print("\n--- configuration ---")
    krs.write_config()
    effective = krs.check_config()
    with open(os.path.join(cfg.out_dir, "kompressor-effective-config.toml"), "w",
              encoding="utf-8") as handle:
        handle.write(effective)
    krs.assert_effective(effective)
    print(f"  effective config verified (mode={cfg.mode}, api_family={cfg.provider}, "
          f"upstream={cfg.upstream})")

    print("\n--- proxy ---")
    krs.start()
    try:
        server_config = krs.server_config()

        print("\n--- gates ---")
        selftest = krs.selftest()
        krs.assert_selftest(selftest)
        preflight_upstream(cfg)
        proxy_preflight = preflight_through_proxy(krs, cfg)
    except BaseException:
        # a gate refused (or the user interrupted): leaving the proxy running
        # would make the next run find a stranger on the port
        if not cfg.keep_proxy:
            krs.stop()
        raise

    if cfg.skip_harbor:
        print("\n--skip-harbor: every gate passed, stopping before Harbor.")
        if not cfg.keep_proxy:
            krs.stop()
        return

    start_ts = int(time.time())
    try:
        _, start_ts, end_ts, cmd_str = harbor_run(krs, cfg)
    finally:
        pass

    print("\n--- collecting ---")
    # Give the stats writer its flush interval before reading the tail.
    time.sleep(2)
    stats = krs.stats()
    if not cfg.keep_proxy:
        krs.stop()          # SIGINT drains the queue; read the DB afterwards
        time.sleep(1)

    # The window is the Harbor run, widened by a few seconds at each end so a
    # request in flight at the boundary is still attributed to it.
    start_ms, end_ms = (start_ts - 5) * 1000, (end_ts + 15) * 1000
    rows = per_request_rows(krs.stats_db, start_ms, end_ms)
    retrievals = retrieval_rows(krs.stats_db, start_ms, end_ms)
    abandoned = abandoned_rows(krs.stats_db, start_ms, end_ms)
    eff = efficiency(rows, retrievals, abandoned, cfg.provider)
    acc = read_harbor_result(cfg.out_dir, since_ts=start_ts)
    window = {"start": start_ts, "end": end_ts,
              "start_iso": dt.datetime.fromtimestamp(start_ts).isoformat(timespec="seconds"),
              "end_iso": dt.datetime.fromtimestamp(end_ts).isoformat(timespec="seconds"),
              "minutes": (end_ts - start_ts) / 60.0}

    text = report(cfg, krs, acc, eff, selftest, effective, window)
    print("\n" + text)

    summary = {
        "stamp": cfg.stamp,
        "tool": "kompressor-rs",
        "provenance": provenance,
        "config": {k: v for k, v in vars(cfg).items() if k != "api_key"},
        "effective_config_toml": effective,
        "server_config": server_config,
        "agent_base_url": krs.agent_base,
        "harbor_cmd": redact(cmd_str),
        "window": window,
        "selftest": selftest,
        "proxy_preflight": proxy_preflight,
        "accuracy": acc,
        "efficiency": eff,
        "abandoned_requests": abandoned,
        "stats_endpoints": stats,
        "validity_flags": validity_flags(eff, acc, selftest, cfg.mode),
        "report": text,
    }
    write_dump(cfg, krs, summary, rows, retrievals, abandoned)
    print(f"\n  dump: {cfg.out_dir}")


def self_test():
    """Check the measurements that have no other way of being wrong out loud.

    There is no Python test runner in this repo, and the two exclusions in
    `cache_losses` are precisely the kind that rot quietly: remove either and
    the check still runs, still prints a number, and the number is wrong in the
    direction that looks like a finding. Run with `--self-test`.
    """
    failures = []
    # Counted, not hardcoded. The total used to be the literal 43 in the line
    # that prints it, so adding a check left the denominator behind and the
    # report said 43/43 while running more -- a self-test that cannot count
    # itself is the wrong thing to trust a measurement to.
    ran = 0

    def check(name, got, want):
        nonlocal ran
        ran += 1
        if got != want:
            failures.append(f"{name}: got {got!r}, want {want!r}")

    # A real loss: the prefix shrank, same conversation, no rounds in between.
    real = [
        {"time_ms": 1000, "conversation": "a", "cache_read_tokens": 11648, "retrieve_rounds": 0},
        {"time_ms": 4954, "conversation": "a", "cache_read_tokens": 2432, "retrieve_rounds": 0},
    ]
    check("a real loss is found", [(l["lost"], l["gap_ms"]) for l in cache_losses(real)],
          [(9216, 3954)])

    # The write side. Where the provider bills writes the lost tokens are paid
    # for twice, and where it does not the read side is the whole cost -- so
    # `wrote` must come back NULL rather than 0, which would read as "nothing
    # was rewritten" on a provider that simply does not report it.
    writes = [
        {"time_ms": 1000, "conversation": "a", "cache_read_tokens": 11648,
         "retrieve_rounds": 0, "cache_write_tokens": 4096},
        {"time_ms": 4954, "conversation": "a", "cache_read_tokens": 2432,
         "retrieve_rounds": 0, "cache_write_tokens": 9216},
    ]
    check("a losing turn carries what it wrote back",
          [l["wrote"] for l in cache_losses(writes)], [9216])
    check("and carries NULL where the provider bills no writes",
          [l["wrote"] for l in cache_losses(real)], [None])
    # The marginal cost is `lost`, not the turn's whole write: the rest of that
    # write is content that was new regardless.
    check("the rewritten span priced is the lost span",
          [(l["lost"], l["lost"] <= l["wrote"]) for l in cache_losses(writes)],
          [(9216, True)])

    # Two conversations interleaved. The second one's cold start is not the
    # first one's eviction, and ordering by time alone reads it as one.
    interleaved = [
        {"time_ms": 1000, "conversation": "a", "cache_read_tokens": 11648, "retrieve_rounds": 0},
        {"time_ms": 2000, "conversation": "b", "cache_read_tokens": 2432, "retrieve_rounds": 0},
        {"time_ms": 3000, "conversation": "a", "cache_read_tokens": 12000, "retrieve_rounds": 0},
    ]
    check("a second conversation is not a loss", cache_losses(interleaved), [])

    # A retrieve-round row aggregates every submission of its turn, so its
    # cache_read is a sum and the next ordinary turn is always lower.
    rounds = [
        {"time_ms": 1000, "conversation": "a", "cache_read_tokens": 78720, "retrieve_rounds": 4},
        {"time_ms": 2000, "conversation": "a", "cache_read_tokens": 15744, "retrieve_rounds": 0},
    ]
    check("a retrieve-round aggregate is not a loss", cache_losses(rounds), [])

    # ...but it is stepped over, not skipped past. A loss either side of a round
    # still has to be seen: one measured run had its first-compression turn run
    # two rounds, and the old code reported it clean on that basis alone.
    spanned = [
        {"time_ms": 1000, "conversation": "a", "cache_read_tokens": 11648, "retrieve_rounds": 0},
        {"time_ms": 2000, "conversation": "a", "cache_read_tokens": 46208, "retrieve_rounds": 2},
        {"time_ms": 3000, "conversation": "a", "cache_read_tokens": 2432, "retrieve_rounds": 0},
    ]
    check("a loss across a retrieve round is still a loss",
          [(l["was"], l["now"]) for l in cache_losses(spanned)], [(11648, 2432)])
    grew = [
        {"time_ms": 1000, "conversation": "a", "cache_read_tokens": 11648, "retrieve_rounds": 0},
        {"time_ms": 2000, "conversation": "a", "cache_read_tokens": 46208, "retrieve_rounds": 2},
        {"time_ms": 3000, "conversation": "a", "cache_read_tokens": 16768, "retrieve_rounds": 0},
    ]
    check("and growth across one is not", cache_losses(grew), [])

    # A drop to zero is the whole prefix going, which is the worst case and was
    # once skipped by a falsy test. NULL is genuinely unknown -- a relayed
    # websocket row reports it -- and stays skipped.
    total = [
        {"time_ms": 1000, "conversation": "a", "cache_read_tokens": 11648, "retrieve_rounds": 0},
        {"time_ms": 2000, "conversation": "a", "cache_read_tokens": 0, "retrieve_rounds": 0},
    ]
    check("a drop to zero is the largest loss, not no loss",
          [l["lost"] for l in cache_losses(total)], [11648])
    unknown = [
        {"time_ms": 1000, "conversation": "a", "cache_read_tokens": 11648, "retrieve_rounds": 0},
        {"time_ms": 2000, "conversation": "a", "cache_read_tokens": None, "retrieve_rounds": 0},
    ]
    check("NULL is unknown, not a loss", cache_losses(unknown), [])
    cold = [
        {"time_ms": 1000, "conversation": "a", "cache_read_tokens": 0, "retrieve_rounds": 0},
        {"time_ms": 2000, "conversation": "a", "cache_read_tokens": 2432, "retrieve_rounds": 0},
    ]
    check("a cold start warming up is not a loss", cache_losses(cold), [])

    # Growth is the normal case and must never register.
    growth = [
        {"time_ms": 1000, "conversation": "a", "cache_read_tokens": 1000, "retrieve_rounds": 0},
        {"time_ms": 2000, "conversation": "a", "cache_read_tokens": 2000, "retrieve_rounds": 0},
    ]
    check("a growing prefix is not a loss", cache_losses(growth), [])

    # The 200-with-nothing split. NULL usage is the quota signature; an
    # explicit zero is a weaker observation that must not raise the same alarm.
    # Built from the full column set rather than the handful this check reads:
    # `efficiency` accesses several columns with `[]` on purpose, so a partial
    # row would fail with a KeyError that says nothing about the thing under
    # test.
    row_keys = (
        "time_ms", "request_id", "model", "status", "stream", "prompt_tokens",
        "completion_tokens", "cache_read_tokens", "cache_write_tokens",
        "retrieve_rounds", "retrieve_tokens_injected", "upstream_ms",
        "total_ms", "error", "mode", "tokens_in", "tokens_out", "chars_in",
        "chars_out", "messages_total", "messages_compressed",
        "messages_skipped", "frozen_messages", "skipped_by_reason", "outcome",
        "skipped_tokens_by_reason", "conversation", "refs_emitted",
        "ref_bytes_emitted", "content_types",
    )

    def row(prompt=None, completion=None, stream=1, model="gpt-5.5",
            messages=None):
        # `model` defaults to a real one because a completion always carries
        # one: it is read from the request body, so even a provider refusal
        # that reports no usage has it. The fixture used to leave it NULL,
        # which made the quota case indistinguishable from a request that was
        # never a completion at all.
        made = dict.fromkeys(row_keys)
        made.update(status=200, stream=stream, cache_read_tokens=0,
                    retrieve_rounds=0, prompt_tokens=prompt,
                    completion_tokens=completion, model=model,
                    messages_total=messages)
        return made

    quota = ([row() for _ in range(17)]
             + [row(prompt=100, completion=10) for _ in range(3)])
    eff = efficiency(quota, [])
    check("no usage at all is flagged", eff["requests_answered_nothing"], 17)
    check("and it is not confused with zero usage", eff["requests_zero_usage"], 0)

    # The measured false positive: one non-streaming preflight reporting 0/0
    # among 31 healthy requests.
    probe = ([row(prompt=0, completion=0, stream=0)]
             + [row(prompt=1000, completion=50) for _ in range(31)])
    eff = efficiency(probe, [])
    check("an explicit zero is not 'answered nothing'", eff["requests_answered_nothing"], 0)
    check("it is counted separately", eff["requests_zero_usage"], 1)

    # The other measured false positive, 2026-10-14: a request that arrived
    # with an empty body, was forwarded, and came back 200 with no model and
    # no engine pass. It is not a completion, so there is no usage missing
    # from it -- and one of them used to light the whole run's warning.
    passthrough = ([row(model=None)]
                   + [row(prompt=1000, completion=50) for _ in range(9)])
    eff = efficiency(passthrough, [])
    check("a bodyless pass-through is not 'answered nothing'",
          eff["requests_answered_nothing"], 0)
    # ...but one the engine did parse still is, model or no model: the two
    # fields are an OR, so neither alone can wave a quota refusal through.
    parsed = [row(model=None, messages=12)]
    check("a request the engine parsed still counts",
          efficiency(parsed, [])["requests_answered_nothing"], 1)
    check("and one probe in 32 raises no flag",
          [f for f in validity_flags(eff, {"counts": (1, 1)}, {}) if "zero" in f], [])

    # Widespread zero usage is a different matter.
    broad = [row(prompt=0, completion=0, stream=0) for _ in range(8)] + \
            [row(prompt=1000, completion=50) for _ in range(12)]
    eff = efficiency(broad, [])
    flagged = [f for f in validity_flags(eff, {"counts": (1, 1)}, {}) if "zero" in f]
    check("but 40% of a run is", len(flagged), 1)

    # The control arm's config. `off` is a --mode choice argparse accepts and
    # the engine has no variant for, so for as long as it was written into
    # [engine] every uncompressed run died at check-config -- which is why
    # thirty-odd compressed runs have no baseline to be compared against.
    # The control arm compresses nothing on purpose, so the flag that exists to
    # catch an accidentally-uncompressed run must not fire on it -- and must
    # still fire on the same numbers in any other mode.
    silent = efficiency([row(prompt=1000, completion=50) for _ in range(4)], [])
    # Both flags, not just the first: excluding `off` from one drops it into
    # the other, which is the bug this pair exists to catch.
    check("mode=off raises no compression flag at all",
          [f for f in validity_flags(silent, {"counts": (1, 1)}, {}, "off")
           if "COMPRESSION" in f], [])
    check("but the same run in recoverable does",
          len([f for f in validity_flags(silent, {"counts": (1, 1)}, {}, "recoverable")
               if "COMPRESSION" in f]), 1)
    # And the control arm has its own failure: compressing when it must not.
    compressed_control = efficiency(
        [row(prompt=1000, completion=50) for _ in range(4)], [])
    compressed_control["requests_compressed"] = 3
    check("a control arm that compressed is flagged",
          len([f for f in validity_flags(compressed_control, {"counts": (1, 1)}, {}, "off")
               if "CONTROL ARM COMPRESSED" in f]), 1)

    off_engine, off_proxy = control_arm("off")
    on_engine, on_proxy = control_arm("recoverable")
    check("mode=off does not ask the engine for a mode it lacks",
          off_engine, 'mode = "recoverable"')
    check("mode=off stops the proxy compressing instead", off_proxy,
          ["compress = false"])
    # `enabled = false` would unmount the router: 404, no hop, no telemetry.
    check("and does not reach for the switch that unmounts the router",
          "enabled" in "".join(off_proxy), False)
    check("while any other mode leaves compression alone", (on_engine, on_proxy),
          ('mode = "recoverable"', []))

    # A session boundary is not a loss. A conversation's history only grows, so
    # a request with fewer messages than the one before it is a second agent
    # session under the same fingerprint. Measured on the 89-task run of
    # 2026-10-13: `messages 92 -> 5` was reported as 27,008 tokens lost and
    # attributed to the proxy.
    boundary = [
        {"conversation": "a", "cache_read_tokens": 37632, "retrieve_rounds": 0,
         "messages_total": 92, "time_ms": 1000},
        {"conversation": "a", "cache_read_tokens": 10624, "retrieve_rounds": 0,
         "messages_total": 5, "time_ms": 2000},
    ]
    check("a session boundary is not a loss", cache_losses(boundary), [])
    grew = [dict(boundary[0]), dict(boundary[1], messages_total=99)]
    check("the same drop within one session still is",
          [l["lost"] for l in cache_losses(grew)], [27008])
    # An unknown count must not be read as a drop: a row the engine never saw
    # has `messages_total` NULL, and the tool that reads `proxy_requests` alone
    # saw NULL for every row until it learned to join.
    unknown = [dict(boundary[0], messages_total=None),
               dict(boundary[1], messages_total=None)]
    check("an unknown message count does not suppress a loss",
          [l["lost"] for l in cache_losses(unknown)], [27008])

    # A refusal is not a miss. `retrieval_events` records both as `found = 0`,
    # separated only by `withheld`, and counting them together reports a store
    # failure that never happened: the Harbor run of 2026-10-09 15:56 asked for
    # twelve fragments, was refused one by the budget, and the other eleven all
    # hit -- reported as `12 (11 hits)`, which reads as one lost fragment.
    served = {"found": 1, "ranged": 1, "returned_tokens": 100, "withheld": 0}
    refused = {"found": 0, "ranged": 0, "returned_tokens": 0, "withheld": 1}
    eff = efficiency([], [served, served, refused])
    check("a withheld ask is not an ask", eff["retrievals"], 2)
    check("a withheld ask is not a miss", eff["retrieval_hits"], 2)
    check("a withheld ask is not a narrowing", eff["retrieval_ranged"], 2)
    check("a withheld ask is counted on its own", eff["retrieval_withheld"], 1)
    # A database older than schema v10 has no column; `retrieval_rows`
    # substitutes a literal 0, so every row reads as served and the run reports
    # no refusals rather than failing to report retrievals at all.
    old = efficiency([], [dict(served), dict(refused, withheld=0)])
    check("an old row without the column still counts", old["retrievals"], 2)
    check("an old row reports no refusals", old["retrieval_withheld"], 0)

    # Which ceiling refused. `Budget::is_spent()` is an OR and the report used
    # to name the token one whichever had fired; across the three full runs of
    # 2026-10-12, -13 and -14, 30 of 32 refusals were the round cap. A refusal
    # by the round cap leaves `retrieve_rounds` one past it, because the
    # refusing round is still charged; one by the token ceiling is refused
    # inside a round the budget had not spent, so its rounds stay at or below.
    # Carried as the rounds each refusal reached, since the cap to compare
    # against lives in the effective config that `efficiency` is not given.
    capped = efficiency(
        [dict(row(prompt=10, completion=1), request_id="by-rounds",
              retrieve_rounds=4),
         dict(row(prompt=10, completion=1), request_id="by-tokens",
              retrieve_rounds=3)],
        [dict(refused, request_id="by-rounds"),
         dict(refused, request_id="by-tokens"),
         dict(served, request_id="by-tokens")])
    check("a refusal carries the rounds its request reached",
          sorted(capped["retrieval_withheld_rounds"]), [3, 4])
    check("and only refusals are carried", len(capped["retrieval_withheld_rounds"]), 2)

    # Re-sends priced as cache reads. The flat sum counts a byte once per turn
    # it survived, which on the 89-task run of 2026-10-12 read 415,987,844
    # against a billed-weighted 45,758,861 and was taken for a corpus size.
    def turn(conversation, prompt, tokens_in, at):
        return {"conversation": conversation, "prompt_tokens": prompt,
                "tokens_in": tokens_in, "time_ms": at}

    # One session, history growing 100 -> 200 -> 300. Flat 600; the content is
    # 300 and the 300 of re-sends price at a tenth: 300 + 30 = 330.
    one = [turn("a", 100, 100, 1), turn("a", 200, 200, 2), turn("a", 300, 300, 3)]
    w, flat, uniq = billed_tokens(one, "tokens_in")
    check("the flat sum counts every re-send", flat, 600)
    check("the content is counted once", uniq, 300)
    check("re-sends price at the cache-read rate", round(w, 6), 330.0)

    # The long-conversation case this exists for: 1 token of content carried
    # 300 turns costs about 31 first-sends, not 300.
    long = [turn("b", 10, 10, i) for i in range(300)]
    w, flat, uniq = billed_tokens(long, "tokens_in")
    check("a byte carried 300 turns", round(w, 1), round(10 + 2990 * 0.1, 1))
    check("against a flat sum of", flat, 3000)

    # Two sessions under one fingerprint -- 21 of 88 conversations on that run.
    # The second session's content is a first send, not a re-send of the first.
    two = [turn("c", 100, 100, 1), turn("c", 200, 200, 2),
           turn("c", 20, 20, 3), turn("c", 40, 40, 4)]
    w, flat, uniq = billed_tokens(two, "tokens_in")
    check("a session break starts a new first send", uniq, 240)
    check("and the flat sum spans both", flat, 360)
    # 200 + 100*0.1 for the first session, 40 + 20*0.1 for the second. The
    # shortcut `unique + (flat - unique) * rate` is exact per SESSION and not
    # across a break -- applying it to the conversation as a whole gives 246
    # and prices the second session's content as a re-send of the first.
    check("and prices each session's re-sends apart", round(w, 6), 252.0)

    # A row the engine never saw contributes nothing rather than a zero-weight
    # session that would drag the peak down.
    skip = [turn("d", 100, None, 1), turn("d", 200, 200, 2)]
    _, flat, uniq = billed_tokens(skip, "tokens_in")
    check("rows without the field are dropped", (flat, uniq), (200, 200))

    for failure in failures:
        print(f"  FAIL  {failure}")
    print(f"  self-test: {'FAILED' if failures else 'ok'}"
          f" ({ran - len(failures)}/{ran} checks)")
    return 1 if failures else 0


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        sys.exit(self_test())
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(130)
