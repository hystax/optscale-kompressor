#!/usr/bin/env python3
"""Replay a run's captured request bodies through a `kompressor` binary.

    scripts/bench/replay-bodies.py --bin target/release/kompressor \
        --capture runs/<job>/kompressor-bodies.jsonl --out /tmp/replay-a

Every `client->proxy` body in a `KOMPRESSOR_BODY_CAPTURE` file is posted, in
capture order, to a proxy started from `--bin` under the packaged
configuration, in front of a local upstream that records what arrives and
answers each call with a minimal, tool-free reply. Nothing reaches a provider
and nothing is billed.

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
        self.out.close()


def write_config(out: Path, upstream_port: int, proxy_port: int) -> Path:
    # Forward slashes: a Windows path in a TOML basic string would read
    # `\U` and friends as escapes. Windows accepts either separator.
    out = out.resolve()
    text = PACKAGED.read_text(encoding="utf-8")
    text = re.sub(
        r'base_url = "https://[^"]+"', f'base_url = "http://127.0.0.1:{upstream_port}"', text
    )
    text = text.replace("/var/lib/kompressor/refs.db", (out / "refs.db").as_posix())
    text = text.replace("/var/lib/kompressor/stats.db", (out / "stats.db").as_posix())
    text = text.replace("/var/lib/kompressor/ca", (out / "ca").as_posix())
    text = text.replace('listen = "127.0.0.1:8817"', f'listen = "127.0.0.1:{proxy_port}"')
    # No intercept listener: nothing here speaks CONNECT, and a fixed port
    # would collide between replays run side by side.
    kept, skipping = [], False
    for line in text.splitlines(keepends=True):
        if re.match(r"\[\[?[A-Za-z_]", line):
            skipping = line.startswith("[proxy.intercept]")
        if not skipping:
            kept.append(line)
    text = "".join(kept)
    text = text.replace(
        "[server]\n", '[server]\nqueue_timeout = "600s"\n', 1
    )
    text = text.replace(
        "[proxy]\n",
        '[proxy]\ncompress_timeout = "600s"\ncache_keep_warm = false\n',
        1,
    )
    path = out / "kompressor.toml"
    path.write_text(text, encoding="utf-8", newline="\n")
    return path


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
    config = write_config(out, upstream.port, proxy_port)
    log = out / "proxy.log"
    env = dict(os.environ)
    env.pop("KOMPRESSOR_BODY_CAPTURE", None)
    env.pop("KOMPRESSOR_CONFIG", None)
    proc = subprocess.Popen(
        [args.bin, "serve", "--proxy", "--api", "--config", str(config)],
        stdout=log.open("w"),
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
            if args.limit and sent >= args.limit:
                break
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
    parser.add_argument("--limit", type=int, default=0)
    parser.add_argument("--compare", nargs=2, metavar=("A", "B"))
    args = parser.parse_args()
    if args.compare:
        sys.exit(compare(Path(args.compare[0]), Path(args.compare[1])))
    if not (args.bin and args.capture and args.out):
        parser.error("--bin, --capture and --out are required")
    print(json.dumps(replay(args), indent=2))


if __name__ == "__main__":
    main()
