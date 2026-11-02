#!/usr/bin/env python3
"""Cut a few whole Claude conversations out of a body capture, for the cache test.

    scripts/bench/extract-cache-capture.py --from <run>/kompressor-bodies.jsonl \
        --out fixtures/claude-capture.jsonl.gz [--conversations k1,k2,...]

`crates/kompressor-proxy/tests/cachesavings.rs` replays the result through a
simulated Anthropic cache, once direct and once through the proxy, and gates on
the input-side cost. It needs the client's requests in order, whole: each one
repeats the conversation so far, and the cache is a question about exactly
those bytes. So this keeps whole conversations, and only the `client->proxy`
records, each tagged with the conversation it belongs to.

# What is changed, and why the cache cannot tell

Claude Code's system prompt and the descriptions in its tool list are its own
text, not the run's, and the proxy compresses neither. Each is replaced with
filler of the same length, the same filler wherever the same text appeared, so
every request caches exactly as it did. `metadata` (a client user id) is
dropped; nothing caches on it. Task text and tool output are kept as captured,
because they are what the proxy compresses.

A conversation containing anything shaped like a credential is left out
whole, fake or not: terminal-bench's sanitize-git-repo plants decoy AWS and
GitHub tokens, and secret scanners do not know they are decoys.

# Picking conversations

Without `--conversations` every conversation is written; run the replay on
that with `KOMPRESSOR_REPLAY_DETAIL=1` to see each one's saving, then choose a
set that keeps the run's overall figure. The committed capture is 31 of the 97
conversations of the 1.5.12 Opus run of 2026-11-01 (13-41-26): the six where
compression came out ahead less the one carrying decoy tokens, and the rest
losing, as nearly all conversations did, together at -1.00% against -1.03%
for the whole run.
"""
import argparse
import gzip
import hashlib
import json
import re
import sys

FILLER = ("This text stands in for client-supplied instructions of the same length, "
          "kept identical on every request so that it caches exactly as the original did. ")

CREDENTIAL = re.compile(
    r"AKIA[0-9A-Z]{16}|ghp_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|hf_[A-Za-z0-9]{20,}"
    r"|sk-[A-Za-z0-9_-]{20,}|xox[abp]-[A-Za-z0-9-]{10,}|-----BEGIN [A-Z ]*PRIVATE KEY")


def filler(text):
    """Same length, and the same for the same input."""
    seed = hashlib.sha256(text.encode()).hexdigest()[:8]
    base = (seed + " " + FILLER) * (len(text) // len(FILLER) + 2)
    return base[:len(text)]


def scrub_descriptions(value):
    if isinstance(value, dict):
        return {k: filler(v) if k == "description" and isinstance(v, str) else scrub_descriptions(v)
                for k, v in value.items()}
    if isinstance(value, list):
        return [scrub_descriptions(v) for v in value]
    return value


def conversation_of(body):
    """What the conversation opened with, inside its model."""
    opening = json.dumps(body["messages"][0])[:2000]
    return hashlib.sha1((body.get("model", "") + opening).encode()).hexdigest()[:10]


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--from", dest="source", required=True, help="kompressor-bodies.jsonl")
    parser.add_argument("--out", required=True, help="output .jsonl.gz")
    parser.add_argument("--conversations", help="comma-separated conversation keys to keep")
    args = parser.parse_args()
    keep = set(args.conversations.split(",")) if args.conversations else None

    rows, tainted = [], set()
    with open(args.source) as source:
        for line in source:
            record = json.loads(line)
            if record.get("dir") != "client->proxy":
                continue
            try:
                body = json.loads(record["body"])
            except (KeyError, ValueError):
                continue
            if not isinstance(body.get("messages"), list) or "max_tokens" not in body:
                continue
            key = conversation_of(body)
            if keep is not None and key not in keep:
                continue
            if CREDENTIAL.search(record["body"]):
                tainted.add(key)
            body.pop("metadata", None)
            if "tools" in body:
                body["tools"] = scrub_descriptions(body["tools"])
            if isinstance(body.get("system"), str):
                body["system"] = filler(body["system"])
            elif isinstance(body.get("system"), list):
                for part in body["system"]:
                    if isinstance(part, dict) and isinstance(part.get("text"), str):
                        part["text"] = filler(part["text"])
            rows.append({"at_ms": record.get("at_ms", 0), "request_id": record.get("request_id", ""),
                         "dir": "client->proxy", "conversation": key,
                         "body": json.dumps(body, separators=(",", ":"))})

    rows = [row for row in rows if row["conversation"] not in tainted]
    if tainted:
        print(f"left out {len(tainted)} conversation(s) carrying credential-shaped strings: "
              + ",".join(sorted(tainted)), file=sys.stderr)
    with gzip.open(args.out, "wt", compresslevel=9) as out:
        for row in rows:
            out.write(json.dumps(row, separators=(",", ":")) + "\n")
    raw = sum(len(row["body"]) for row in rows)
    print(f"{len(rows)} requests, {len({row['conversation'] for row in rows})} conversations, "
          f"{raw // 1000} KB of bodies -> {args.out}")


if __name__ == "__main__":
    main()
