#!/usr/bin/env python3
"""Build a conversation corpus from a run's captured request bodies.

    scripts/bench/extract-conversations.py --from <run-dir>... --out fixtures/conversations.jsonl.gz

# Why the bodies and not the ref store

`kompressor bench extract` reads each run's `kompressor-refs.db`, which holds
only content that became a *handle*. That is the right source for the payload
corpus and the wrong one here: a cross-turn fold replaces a repeat with a
back-reference and usually mints nothing, so the compressor this corpus exists
to measure is largely absent from that store by construction. It is also why
`cross_turn` has had no corpus coverage while being one of the two largest real
compressors.

`kompressor-bodies.jsonl` holds whole request bodies as they arrived from the
agent, so a conversation can be reassembled in order — which is the unit
cross-turn folding works on. Each conversation also records the provider its
body was shaped for and the model, which `kompressor bench price` prices it at.

# What it takes and what it drops

The LARGEST inbound body per conversation, because an agent re-sends its whole
history every turn: the last request already contains every earlier message, so
taking every request would be the same text many times over and would flatter
the fold enormously.

Messages keep their order and role. Content blocks are joined; shapes the walker
would not read are dropped rather than guessed at.
"""

from __future__ import annotations

import argparse
import gzip
import json
import sqlite3
import sys
from pathlib import Path

CONVERSATION_VERSION = 1


def text_of(message: dict) -> str:
    """The text a walker would hand the engine, or empty for shapes it would not."""
    content = message.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for block in content:
            if not isinstance(block, dict):
                continue
            for key in ("text", "output", "content"):
                value = block.get(key)
                if isinstance(value, str):
                    parts.append(value)
                    break
        return "\n".join(parts)
    output = message.get("output")
    return output if isinstance(output, str) else ""


def provider_of(body: dict) -> str:
    """The provider a body was shaped for: what `kompressor bench price`
    prices the conversation at."""
    if isinstance(body.get("messages"), list) and ("max_tokens" in body or "system" in body):
        return "anthropic"
    if "input" in body or isinstance(body.get("messages"), list):
        return "openai"
    return ""


def conversations_in(run: Path) -> dict:
    """The richest body per conversation in one run dump."""
    bodies = run / "kompressor-bodies.jsonl"
    stats = run / "kompressor-stats.db"
    if not bodies.exists() or not stats.exists():
        return {}

    con = sqlite3.connect(f"file:{stats}?mode=ro", uri=True)
    owner = {
        request_id: conversation
        for request_id, conversation in con.execute(
            "SELECT request_id, conversation FROM proxy_requests WHERE conversation IS NOT NULL"
        )
    }
    con.close()

    best = {}
    with open(bodies, encoding="utf-8", errors="replace") as handle:
        for line in handle:
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                # A capture truncated mid-write is not a reason to lose the rest.
                continue
            if row.get("dir") != "client->proxy":
                continue
            conversation = owner.get(row.get("request_id"))
            if not conversation:
                continue
            try:
                body = json.loads(row["body"])
            except (KeyError, json.JSONDecodeError):
                continue
            messages = body.get("input") or body.get("messages") or []
            if len(messages) > len(best.get(conversation, ([], "", ""))[0]):
                best[conversation] = (messages, provider_of(body), str(body.get("model") or ""))
    return best


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--from", dest="runs", nargs="+", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument(
        "--min-turns",
        type=int,
        default=8,
        help="drop conversations shorter than this; a fold needs something to fold against",
    )
    parser.add_argument("--task", default="", help="label recorded on every conversation")
    args = parser.parse_args()

    out = []
    for run in args.runs:
        for conversation, (messages, provider, model) in conversations_in(run).items():
            turns = [
                {"role": str(message.get("role") or "user"), "text": text_of(message)}
                for message in messages
                if isinstance(message, dict)
            ]
            turns = [turn for turn in turns if turn["text"].strip()]
            if len(turns) < args.min_turns:
                continue
            out.append(
                {
                    "id": conversation,
                    "task": args.task or run.name,
                    "provider": provider,
                    "model": model,
                    "turns": turns,
                }
            )

    if not out:
        print("no conversations found; was the run captured with bodies?", file=sys.stderr)
        return 1

    args.out.parent.mkdir(parents=True, exist_ok=True)
    opener = gzip.open if args.out.suffix == ".gz" else open
    with opener(args.out, "wt", encoding="utf-8") as handle:
        handle.write(json.dumps({"conversation_version": CONVERSATION_VERSION}) + "\n")
        for conversation in out:
            handle.write(json.dumps(conversation) + "\n")

    chars = sum(len(t["text"]) for c in out for t in c["turns"])
    turns = sum(len(c["turns"]) for c in out)
    print(f"{len(out)} conversation(s), {turns} turns, {chars:,} chars -> {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
