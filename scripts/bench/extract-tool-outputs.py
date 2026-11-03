#!/usr/bin/env python3
"""Every tool output a run sent, as corpus payloads -- including the declined ones.

    scripts/bench/extract-tool-outputs.py --from <run-dir>... --out tool-outputs.jsonl.gz \
        [--exclude-corpus fixtures/corpus.jsonl.gz]

`kompressor bench extract` reads a run's ref store, which holds only what the
engine elided, so it cannot yield a payload the engine declined. This reads the
captured *ingress* bodies (`kompressor-bodies.jsonl`, `client->proxy`), so a
tool result the engine left alone is extracted exactly like one it compressed.
Anthropic Messages bodies only: each `tool_result` block of at least
`--min-chars` (default 512, the size gate) becomes one payload, deduplicated by
content across every run given, with `requests` counting how many request
bodies carried it -- which is what the replay's billed coverage weights by.

Captured agent output is not safe to commit as it stands, so two filters run
before anything is written:

* **credential-shaped strings** -- API keys, tokens, private keys, bearer
  headers, JWTs. A task about scrubbing secrets plants fake ones, and a fake key
  in a fixture still trips push protection and secret scanners;
* **personal data** -- any email address outside a short allow-list of
  placeholder and symbol-version domains (`example.com`, `@GLIBC_…`). Git
  history, a CV, a package author: real people, dropped.

The run paths and the operator's own name are not secrets, but they are not
the corpus's business either, and are caught by `--exclude-text`.

Standard library only.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import re
import sys
from pathlib import Path

CREDENTIAL = re.compile(
    r"sk-ant-|\bsk-[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9]{20,}|xox[abpr]-"
    r"|-----BEGIN [A-Z ]*PRIVATE KEY-----|eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\."
    r"|[Bb]earer\s+[A-Za-z0-9._\-]{20,}"
)
EMAIL = re.compile(r"[\w.+-]+@([\w-]+\.[\w.]+)")
PLACEHOLDER_DOMAIN = re.compile(r"^(?:example\.(?:com|org|net)|[a-z]\.com|local\.edu|GLIBC_[\d.]+)$")


def tool_results(body: dict):
    """(tool name, text) for every tool_result in a Messages body."""
    names = {}
    for message in body.get("messages") or []:
        content = message.get("content")
        if not isinstance(content, list):
            continue
        for block in content:
            if isinstance(block, dict) and block.get("type") == "tool_use":
                names[block.get("id")] = block.get("name") or "unknown"
    for message in body.get("messages") or []:
        content = message.get("content")
        if not isinstance(content, list):
            continue
        for block in content:
            if not isinstance(block, dict) or block.get("type") != "tool_result":
                continue
            inner = block.get("content")
            if isinstance(inner, str):
                text = inner
            else:
                text = "\n".join(part.get("text", "") for part in inner or []
                                 if isinstance(part, dict) and part.get("type") == "text")
            yield names.get(block.get("tool_use_id"), "unknown"), text


def safe(text: str, excluded: list[str]) -> str | None:
    """Why `text` must not be committed, or None."""
    if CREDENTIAL.search(text):
        return "credential"
    if any(not PLACEHOLDER_DOMAIN.match(domain) for domain in EMAIL.findall(text)):
        return "personal data"
    if any(needle in text for needle in excluded):
        return "excluded text"
    return None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--from", dest="runs", nargs="+", required=True, help="run directories")
    ap.add_argument("--out", required=True)
    ap.add_argument("--min-chars", type=int, default=512)
    ap.add_argument("--exclude-corpus", help="skip payloads whose content is already in this corpus")
    ap.add_argument("--exclude-text", action="append", default=[], help="drop payloads containing this")
    ap.add_argument("--tag", default="claude-code")
    args = ap.parse_args()

    existing = set()
    if args.exclude_corpus:
        for line in gzip.open(args.exclude_corpus, "rt", encoding="utf-8"):
            record = json.loads(line)
            if "content" in record:
                existing.add(record["content"])

    payloads, dropped = {}, {"duplicate of corpus": 0, "credential": 0, "personal data": 0, "excluded text": 0}
    for run in map(Path, args.runs):
        name = run.resolve().parent.name.split("anthropic_")[-1]
        bodies = run / "kompressor-bodies.jsonl"
        if not bodies.exists():
            print(f"no captured bodies in {run}", file=sys.stderr)
            continue
        for line in open(bodies, encoding="utf-8", errors="replace"):
            if '"client->proxy"' not in line[:400]:
                continue
            try:
                record = json.loads(line)
                body = record["body"]
                body = json.loads(body) if isinstance(body, str) else body
            except (ValueError, KeyError):
                continue
            for tool, text in tool_results(body or {}):
                if len(text) < args.min_chars:
                    continue
                cid = hashlib.blake2b(text.encode(), digest_size=8).hexdigest()
                if cid in payloads:
                    if payloads[cid] is not None:
                        payloads[cid]["requests"] += 1
                    continue
                if text in existing:
                    dropped["duplicate of corpus"] += 1
                    existing.discard(text)
                    continue
                reason = safe(text, args.exclude_text)
                if reason:
                    dropped[reason] += 1
                    payloads[cid] = None
                    continue
                payloads[cid] = {
                    "content_id": cid, "content": text, "source_tool": tool,
                    "preceding_user_text": "", "role": "tool", "occurrences": 1, "requests": 1,
                    "run": name, "arm": "on", "task": f"{args.tag}/{name}", "trial": "", "tags": [args.tag],
                }
    kept = [p for p in payloads.values() if p is not None]
    with gzip.open(args.out, "wt", encoding="utf-8") as out:
        out.write(json.dumps({"corpus_version": 1}) + "\n")
        for payload in kept:
            out.write(json.dumps(payload, separators=(",", ":")) + "\n")
    print(f"{len(kept)} payloads -> {args.out}; dropped {dropped}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
