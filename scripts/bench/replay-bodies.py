#!/usr/bin/env python3
"""Replay a run's captured request bodies through a `kompressor` binary.

    scripts/bench/replay-bodies.py --bin target/release/kompressor \
        --capture runs/<job>/kompressor-bodies.jsonl --out /tmp/replay-a

Every `client->proxy` body in a `KOMPRESSOR_BODY_CAPTURE` file is posted, in
capture order, to a proxy started from `--bin` under the packaged
configuration, in front of a local upstream that records what arrives and
answers each call with a minimal, tool-free reply. Nothing reaches a provider
and nothing is billed.

Use `--config <run>/kompressor-effective-config.toml` to preserve a run's
compression settings. Listeners, database paths and upstream URLs are redirected
locally; keep-warm is disabled, including provider overrides. Environment
configuration overrides are removed. This replays compression, not elapsed
cache lifetimes or the provider's original usage and tool decisions.

What it writes to `--out`:

* `upstream.jsonl` -- one line per upstream call: the request's index, the
  path, the byte length and the SHA-256 of the body the proxy sent upstream;
* `summary.json` -- totals: requests, client bytes, upstream bytes, the
  reduction, compression timeouts seen in the proxy log, and the same
  reduction measured on the original run's own `proxy->upstream` bodies.

Two binaries given the same capture are equivalent on that traffic exactly when
their `upstream.jsonl` files are identical, which `--compare` checks. That is
the regression gate this exists for: the engine promises the same input
compresses to the same bytes, so any difference is a behaviour change, not
noise -- provided nothing time-dependent intervenes. The compression deadline
is therefore raised far past anything a request needs (a deadline that fires
forwards the original, which depends on machine load), keep-warm pings are off,
and statistics are kept so the run can be inspected afterwards.

Standard library only.
"""

from __future__ import annotations

import argparse
import hashlib
import http.server
import json
import os
import re
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
PACKAGED = REPO / "packaging" / "kompressor.toml"


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def classify(body: dict) -> str | None:
    """The path a captured body was sent to, from its shape."""
    if isinstance(body.get("messages"), list) and "max_tokens" in body:
        return "/v1/messages"
    if "input" in body:
        return "/v1/responses"
    if isinstance(body.get("messages"), list):
        return "/v1/chat/completions"
    return None


def reply_for(path: str, model: str) -> dict:
    if path.endswith("/messages"):
        return {
            "id": "msg_replay",
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 1,
                "output_tokens": 1,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0,
            },
        }
    if path.endswith("/responses"):
        return {
            "id": "resp_replay",
            "object": "response",
            "status": "completed",
            "model": model,
            "output": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "ok"}],
                }
            ],
            "usage": {
                "input_tokens": 1,
                "output_tokens": 1,
                "total_tokens": 2,
                "input_tokens_details": {"cached_tokens": 0},
            },
        }
    return {
        "id": "chatcmpl-replay",
        "object": "chat.completion",
        "model": model,
        "choices": [
            {
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop",
            }
        ],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    }


class Upstream:
    """A recording upstream on a background thread."""

    def __init__(self, out: Path):
        # LF on every platform: Windows text mode would write CRLF, and the
        # file's hash is what two machines' replays are compared by.
        self.out = out.open("w", encoding="utf-8", newline="\n")
        self.lock = threading.Lock()
        self.current = -1
        self.bytes = 0
        self.calls = 0
        upstream = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def do_POST(self):
                length = int(self.headers.get("content-length") or 0)
                raw = self.rfile.read(length)
                try:
                    model = json.loads(raw).get("model", "replay")
                except Exception:
                    model = "replay"
                with upstream.lock:
                    upstream.calls += 1
                    upstream.bytes += len(raw)
                    upstream.out.write(
                        json.dumps(
                            {
                                "i": upstream.current,
                                "path": self.path,
                                "bytes": len(raw),
                                "sha256": hashlib.sha256(raw).hexdigest(),
                            }
                        )
                        + "\n"
                    )
                payload = json.dumps(reply_for(self.path, model)).encode()
                self.send_response(200)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def do_GET(self):
                self.send_response(404)
                self.send_header("content-length", "0")
                self.end_headers()

        self.port = free_port()
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", self.port), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.out.close()


def write_config(out: Path, upstream_port: int, proxy_port: int, source: Path = PACKAGED) -> Path:
    """`source`, pointed at this replay and at nothing else.

    What is replayed is the compression settings. Everything that says where
    the service listens, what it writes and whom it calls is replaced: a run's
    effective configuration names the bench host's bridge address, that run's
    own databases and the provider, and the packaged one names the paths a
    live service on this machine is using.
    """
    # Forward slashes: a Windows path in a TOML basic string would read
    # `\U` and friends as escapes. Windows accepts either separator.
    out = out.resolve()
    upstream = f"http://127.0.0.1:{upstream_port}"
    text = source.read_text(encoding="utf-8")
    # `[ \t]`, not `\s`: that also matches the newline before the key, and
    # would join it to whatever line came before.
    text = re.sub(r'(?m)^([ \t]*)base_url[ \t]*=.*$', rf'\1base_url = "{upstream}"', text)
    overrides = {
        "server": dict(listen=f"127.0.0.1:{proxy_port}", api_listen="", proxy_listen="",
                       mcp_listen="", stats_listen="", queue_timeout="600s"),
        "refs": dict(path=(out / "refs.db").as_posix()),
        "stats": dict(path=(out / "stats.db").as_posix()),
        "proxy": dict(compress_timeout="600s", cache_keep_warm=False),
        # A scheduled backup would be written wherever the source said.
        "maintenance": dict(backup_dir=""),
    }
    assign = lambda table: [f"{key} = {json.dumps(value)}\n" for key, value in table.items()]
    kept, skipping, section, seen = [], False, "", set()
    for line in text.splitlines(keepends=True):
        header = re.match(r"\s*\[([A-Za-z_][\w.]*)\]\s*(?:#.*)?$", line)
        if header or re.match(r"\s*\[\[", line):
            section = header.group(1) if header else ""
            # No intercept listener: nothing here speaks CONNECT, and a fixed
            # port would collide between replays run side by side.
            skipping = section == "proxy.intercept" or section.startswith("proxy.intercept.")
            if skipping:
                continue
            # A provider's own switch wins over `[proxy]`'s, so turning
            # keep-warm off there alone leaves it on for that provider.
            if section.startswith("proxy.providers.") and section.count(".") == 2:
                overrides[section] = dict(cache_keep_warm=False)
            seen.add(section)
            kept.append(line)
            kept.extend(assign(overrides.get(section, {})))
            continue
        key = re.match(r"\s*(\w+)\s*=", line)
        if not skipping and not (key and key.group(1) in overrides.get(section, {})):
            kept.append(line)
    if kept and not kept[-1].endswith("\n"):
        kept.append("\n")
    # A section the source leaves to its defaults still has to be redirected:
    # the default listener is the one a live service on this machine holds.
    for section in ("server", "refs", "stats", "proxy", "maintenance"):
        if section not in seen:
            kept.append(f"[{section}]\n")
            kept.extend(assign(overrides[section]))
    text = "".join(kept)
    isolated(text, out, upstream, proxy_port)
    path = out / "kompressor.toml"
    path.write_text(text, encoding="utf-8", newline="\n")
    return path


def isolated(text: str, out: Path, upstream: str, proxy_port: int) -> None:
    """Refuse a configuration that still reaches outside the replay.

    The rewrite above edits text, line by line, and reads the spelling the
    packaged file and `kompressor config` produce. A hand-written file can say
    the same things another way -- `providers.anthropic.cache_keep_warm = true`
    under `[proxy]`, an inline table -- and would pass through untouched. So the
    result is parsed and checked, and a replay that would ping a provider or
    open someone else's database does not start.
    """
    try:
        import tomllib
    except ImportError:  # before Python 3.11: nothing to parse it with
        return
    try:
        config = tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        sys.exit(f"replay: the rewritten configuration does not parse: {error}")
    server, proxy = config.get("server", {}), config.get("proxy", {})
    wrong = []
    if server.get("listen") != f"127.0.0.1:{proxy_port}":
        wrong.append("server.listen is not the replay's port")
    wrong += [f"server.{key} is set" for key in
              ("api_listen", "proxy_listen", "mcp_listen", "stats_listen") if server.get(key)]
    for name in ("refs", "stats"):
        if Path(config.get(name, {}).get("path", "")).parent != out:
            wrong.append(f"{name}.path is outside the replay directory")
    if config.get("maintenance", {}).get("backup_dir"):
        wrong.append("maintenance.backup_dir is set")
    if "intercept" in proxy:
        wrong.append("proxy.intercept is present")
    if proxy.get("cache_keep_warm") is not False:
        wrong.append("proxy.cache_keep_warm is not off")
    wrong += [f"proxy.providers.{name}.cache_keep_warm is on"
              for name, provider in proxy.get("providers", {}).items()
              if isinstance(provider, dict) and provider.get("cache_keep_warm")]

    def urls(node, where):
        if isinstance(node, dict):
            for key, value in node.items():
                if key == "base_url" and value != upstream:
                    wrong.append(f"{where}.base_url is {value!r}, not the local upstream")
                urls(value, f"{where}.{key}")
        elif isinstance(node, list):
            for index, value in enumerate(node):
                urls(value, f"{where}[{index}]")

    urls(config, "config")
    if wrong:
        sys.exit("replay: the configuration would reach outside this replay:\n  "
                 + "\n  ".join(wrong))


def wait_ready(port: int, proc: subprocess.Popen, log: Path):
    deadline = time.time() + 60
    while time.time() < deadline:
        if proc.poll() is not None:
            sys.exit(f"the proxy exited:\n{log.read_text()[-3000:]}")
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/healthz", timeout=1)
            return
        except Exception:
            time.sleep(0.2)
    sys.exit("the proxy did not come up")


def records(capture: Path):
    with capture.open("rb") as f:
        for line in f:
            if b'"body"' not in line:
                continue
            try:
                yield json.loads(line)
            except Exception:
                continue


def replay(args) -> dict:
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    for stale in ("refs.db", "stats.db", "upstream.jsonl"):
        for p in out.glob(stale + "*"):
            p.unlink()
    upstream = Upstream(out / "upstream.jsonl")
    proxy_port = free_port()
    config = write_config(out, upstream.port, proxy_port,
                          Path(args.config) if args.config else PACKAGED)
    log = out / "proxy.log"
    # Every one of them, not the two that were known about when this was
    # written: `KOMPRESSOR__PROXY__…` overrides the file key by key, so a shell
    # left set up for a bench run would replay that run's settings instead of
    # the file's, and could point the child back at a provider.
    env = {key: value for key, value in os.environ.items()
           if not key.startswith("KOMPRESSOR_")}
    with log.open("w") as sink:
        proc = subprocess.Popen(
            [args.bin, "serve", "--proxy", "--api", "--config", str(config)],
            stdout=sink,
            stderr=subprocess.STDOUT,
            env=env,
            cwd=out,
        )
    try:
        wait_ready(proxy_port, proc, log)
        sent = client_bytes = errors = 0
        orig_in = orig_out = 0
        by_path: dict[str, int] = {}
        pending_original: dict[str, int] = {}
        for record in records(Path(args.capture)):
            direction = record.get("dir")
            raw = record.get("body", "")
            if direction == "proxy->upstream":
                rid = record.get("request_id")
                if rid in pending_original:
                    orig_in += pending_original.pop(rid)
                    orig_out += len(raw.encode())
                continue
            if direction != "client->proxy":
                continue
            if args.limit and sent >= args.limit:
                # Still consume outbound records to account for the last
                # selected request's original body pair.
                continue
            try:
                body = json.loads(raw)
            except Exception:
                continue
            if not isinstance(body, dict):
                continue
            path = classify(body)
            if path is None:
                continue
            pending_original[record.get("request_id")] = len(raw.encode())
            body["stream"] = False
            data = json.dumps(body, ensure_ascii=False).encode()
            headers = {"content-type": "application/json"}
            if path == "/v1/messages":
                headers["x-api-key"] = "sk-ant-api03-replay"
                headers["anthropic-version"] = "2023-06-01"
            else:
                headers["authorization"] = "Bearer sk-proj-replay"
            with upstream.lock:
                upstream.current = sent
            request = urllib.request.Request(
                f"http://127.0.0.1:{proxy_port}{path}", data=data, headers=headers
            )
            try:
                with urllib.request.urlopen(request, timeout=900) as response:
                    response.read()
            except urllib.error.HTTPError as e:
                errors += 1
                e.read()
            except Exception:
                errors += 1
            sent += 1
            client_bytes += len(data)
            by_path[path] = by_path.get(path, 0) + 1
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            proc.kill()
        upstream.close()
    log_text = log.read_text(encoding="utf-8", errors="replace")
    summary = {
        "capture": str(args.capture),
        "bin": str(args.bin),
        "requests": sent,
        "by_path": by_path,
        "errors": errors,
        "upstream_calls": upstream.calls,
        "client_bytes": client_bytes,
        "upstream_bytes": upstream.bytes,
        "reduction_pct": round(100 * (1 - upstream.bytes / client_bytes), 3) if client_bytes else 0,
        "compress_timeouts": len(re.findall(r"timed out|compress_timeout|deadline", log_text)),
        "panics": log_text.count("panicked"),
        "original_run_reduction_pct": round(100 * (1 - orig_out / orig_in), 3) if orig_in else None,
    }
    (out / "summary.json").write_text(
        json.dumps(summary, indent=2), encoding="utf-8", newline="\n"
    )
    return summary


def compare(a: Path, b: Path) -> int:
    la = (a / "upstream.jsonl").read_text(encoding="utf-8").splitlines()
    lb = (b / "upstream.jsonl").read_text(encoding="utf-8").splitlines()
    differ = [i for i, (x, y) in enumerate(zip(la, lb)) if x != y]
    result = {
        "a": str(a),
        "b": str(b),
        "calls_a": len(la),
        "calls_b": len(lb),
        "differing_calls": len(differ) + abs(len(la) - len(lb)),
        "first_differences": differ[:10],
        "identical": la == lb,
    }
    print(json.dumps(result, indent=2))
    return 0 if la == lb else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--bin")
    parser.add_argument("--capture")
    parser.add_argument("--out")
    parser.add_argument("--config", help="configuration to replay (default: packaged)")
    parser.add_argument("--limit", type=int, default=0)
    parser.add_argument("--compare", nargs=2, metavar=("A", "B"))
    args = parser.parse_args()
    if args.compare:
        sys.exit(compare(Path(args.compare[0]), Path(args.compare[1])))
    if not (args.bin and args.capture and args.out):
        parser.error("--bin, --capture and --out are required")
    summary = replay(args)
    print(json.dumps(summary, indent=2))
    sys.exit(1 if summary["errors"] else 0)


if __name__ == "__main__":
    main()
