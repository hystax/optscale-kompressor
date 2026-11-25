#!/usr/bin/env python3
"""Would attaching sibling handles have saved the second and later retrieve rounds?

    scripts/bench/sibling-rounds.py --capture runs/<job>/kompressor-bodies.jsonl \\
        [--stats runs/<job>/kompressor-stats.db] [--refs runs/<job>/kompressor-refs.db] [--json]

The measurement DESIGN §20 item 13 asks for before `proxy.retrieve_attach_siblings`
is turned on: for each second-or-later retrieve round of a request, was the
handle it asked for a *sibling* of a handle an earlier round of the same request
had already fetched -- another handle of the same tool output, present in the
submission before it? If so, the rule "answer a call with the rest of its
output" would have attached it, and the round would not have been needed.
HEADROOM-PLAN Tier 6 sets the bar: at least half of second-or-later rounds.

# Where the rounds come from

A request's hidden rounds never reach the client, so they are in no
`client->proxy` body. Two sources, in order of preference:

* `retrieve-round` lines in the capture (written since the proxy started
  recording them): the submission the proxy built for each round. The model's
  retrieve calls are the assistant turns appended after the original messages.
* With `--stats`, `retrieval_events` rows of the `proxy` surface, grouped by
  request and split into rounds where consecutive asks are more than `--gap-ms`
  apart: asks within one round are answered back to back, while a round
  boundary waits on a model call. An inference, and labelled as one.

The tool outputs a sibling is looked for in are the strings of the request's
`proxy->upstream` body: one string is one tool output (or one text block of
one).

# What it prices

Per provider, the difference sibling attachment would have made, in tokens at
the uncached input price:

* **saved**: each covered later round avoided, at `r * prompt + output_cost`
  -- the prompt re-read from cache plus the model's call. `prompt` is the
  request's billed prompt per upstream call from `proxy_requests` when
  `--stats` is given, else the body's length / 4.
* **cost**: every sibling the rule would have attached on every round, inside
  `--budget`, at `w` (written into the cache with the round) plus `r` for each
  later round of the request that re-reads it. Sizes from `--refs` (the
  fragment store), else from `retrieval_events` where the handle was asked for
  at some point, else `--default-sibling-tokens`.

`w` and `r` are the provider's published cache prices, as the proxy uses them.
Standard library only.
"""

from __future__ import annotations

import argparse
import json
import re
import sqlite3
import sys
from collections import defaultdict
from pathlib import Path

RETRIEVE_TOOL = "optscale_kompressor_retrieve"
HANDLE = re.compile(r"ref:(k_[A-Za-z0-9_-]{12})(?![A-Za-z0-9_-])")
BARE = re.compile(r"^k_[A-Za-z0-9_-]{12}$")
ACCEPTANCE = 0.5


def provider_of(body: dict) -> str:
    if isinstance(body.get("messages"), list) and ("max_tokens" in body or "system" in body):
        return "anthropic"
    if "input" in body or isinstance(body.get("messages"), list):
        return "openai"
    return "unknown"


def prices(provider: str, model: str) -> tuple[float, float]:
    """(w, r): a cache write and a cache read, as multiples of uncached input."""
    model = model.removeprefix("anthropic/")
    if provider == "openai":
        return 1.0, 0.1
    if model.startswith(("claude-opus-5-5", "claude-sonnet-5-5")):
        return 1.25, 0.05
    if model.startswith(("claude-fable-5-1", "claude-mythos-5-1")):
        return 1.25, 0.025
    return 1.25, 0.1


def items_of(body: dict) -> list:
    items = body.get("messages")
    if not isinstance(items, list):
        items = body.get("input")
    return items if isinstance(items, list) else []


def asked_ref(arguments) -> str | None:
    if isinstance(arguments, str):
        try:
            arguments = json.loads(arguments)
        except json.JSONDecodeError:
            return None
    if not isinstance(arguments, dict):
        return None
    ref = arguments.get("ref")
    return ref if isinstance(ref, str) and BARE.match(ref) else None


def rounds_in(appended: list) -> list[list[str]]:
    """The handles each appended round asked for, in order, for any family."""
    rounds: list[list[str]] = []
    run: list[str] | None = None  # an open Responses run of function calls
    for item in appended:
        if not isinstance(item, dict):
            run = None
            continue
        if item.get("type") == "function_call":
            ref = asked_ref(item.get("arguments")) if item.get("name") == RETRIEVE_TOOL else None
            if run is None:
                run = []
                rounds.append(run)
            if ref:
                run.append(ref)
            continue
        run = None
        if item.get("role") != "assistant":
            continue
        asked: list[str] = []
        content = item.get("content")
        for block in content if isinstance(content, list) else []:
            if isinstance(block, dict) and block.get("type") == "tool_use" \
                    and block.get("name") == RETRIEVE_TOOL:
                ref = asked_ref(block.get("input"))
                if ref:
                    asked.append(ref)
        for call in item.get("tool_calls") or []:
            function = (call.get("function") or {}) if isinstance(call, dict) else {}
            if function.get("name") == RETRIEVE_TOOL:
                ref = asked_ref(function.get("arguments"))
                if ref:
                    asked.append(ref)
        if asked or item.get("tool_calls") or isinstance(content, list):
            rounds.append(asked)
    return [r for r in rounds if r]


def strings_of(value, out: list[str]) -> None:
    if isinstance(value, str):
        out.append(value)
    elif isinstance(value, list):
        for item in value:
            strings_of(item, out)
    elif isinstance(value, dict):
        for item in value.values():
            strings_of(item, out)


def siblings_of(outputs: list[str], asked: str) -> list[str]:
    """Every other handle of each tool output that names `asked`, in order."""
    needle = f"ref:{asked}"
    found: list[str] = []
    for text in outputs:
        if needle not in text:
            continue
        for handle in HANDLE.findall(text):
            if handle != asked and handle not in found:
                found.append(handle)
    return found


def read_capture(path: Path) -> tuple[dict, dict]:
    """`proxy->upstream` bodies and the latest `retrieve-round` body, by request."""
    bases: dict[str, dict] = {}
    last_round: dict[str, tuple[int, dict]] = {}
    with path.open(encoding="utf-8", errors="replace") as handle:
        for line in handle:
            if '"body"' not in line:
                continue
            try:
                row = json.loads(line)
                direction = row.get("dir")
                if direction not in ("proxy->upstream", "retrieve-round"):
                    continue
                body = json.loads(row["body"])
            except (json.JSONDecodeError, KeyError, TypeError):
                continue
            request = row.get("request_id")
            if not request or not isinstance(body, dict):
                continue
            if direction == "proxy->upstream":
                bases.setdefault(request, body)
            else:
                number = int(row.get("round") or 0)
                if number >= last_round.get(request, (-1, None))[0]:
                    last_round[request] = (number, body)
    return bases, {request: body for request, (_, body) in last_round.items()}


def stats_rounds(stats: Path, gap_ms: int) -> dict[str, list[list[str]]]:
    con = sqlite3.connect(f"file:{stats}?mode=ro", uri=True)
    rows = con.execute(
        "SELECT request_id, time_ms, ref FROM retrieval_events "
        "WHERE surface = 'proxy' AND request_id IS NOT NULL ORDER BY request_id, time_ms, id"
    ).fetchall()
    con.close()
    out: dict[str, list[list[str]]] = defaultdict(list)
    last: dict[str, int] = {}
    for request, at, ref in rows:
        if request not in last or at - last[request] > gap_ms:
            out[request].append([])
        out[request][-1].append(ref)
        last[request] = at
    return dict(out)


def stats_prompts(stats: Path) -> dict[str, tuple[float, int]]:
    """Billed prompt per upstream call, and the rounds the request ran."""
    con = sqlite3.connect(f"file:{stats}?mode=ro", uri=True)
    rows = con.execute(
        "SELECT request_id, COALESCE(prompt_tokens, 0), COALESCE(cache_read_tokens, 0), "
        "COALESCE(cache_write_tokens, 0), COALESCE(retrieve_rounds, 0), api_family "
        "FROM proxy_requests WHERE request_id IS NOT NULL"
    ).fetchall()
    con.close()
    out = {}
    for request, prompt, read, written, rounds, family in rows:
        # Anthropic reports the uncached remainder; OpenAI the whole prompt.
        whole = prompt + read + written if family == "anthropic_messages" else prompt
        out[request] = (whole / (rounds + 1), rounds)
    return out


def fragment_sizes(stats: Path | None, refs: Path | None) -> dict[str, int]:
    sizes: dict[str, int] = {}
    if stats:
        con = sqlite3.connect(f"file:{stats}?mode=ro", uri=True)
        for ref, tokens in con.execute(
            "SELECT ref, MAX(fragment_tokens) FROM retrieval_events GROUP BY ref"
        ):
            if tokens:
                sizes[ref] = int(tokens)
        con.close()
    if refs:
        con = sqlite3.connect(f"file:{refs}?mode=ro", uri=True)
        for ref, size in con.execute("SELECT ref, MAX(bytes) FROM refs GROUP BY ref"):
            sizes[ref] = max(1, int(size) // 4)
        con.close()
    return sizes


def measure(args) -> dict:
    bases, last_rounds = read_capture(Path(args.capture))
    from_stats = stats_rounds(Path(args.stats), args.gap_ms) if args.stats else {}
    prompts = stats_prompts(Path(args.stats)) if args.stats else {}
    sizes = fragment_sizes(Path(args.stats) if args.stats else None,
                           Path(args.refs) if args.refs else None)

    totals = defaultdict(lambda: defaultdict(float))
    source_counts = defaultdict(int)
    for request, base in bases.items():
        if request in last_rounds:
            appended = items_of(last_rounds[request])[len(items_of(base)):]
            rounds = rounds_in(appended)
            source = "capture"
        elif request in from_stats:
            rounds = from_stats[request]
            source = "stats"
        else:
            continue
        if not rounds:
            continue
        source_counts[source] += 1
        provider = provider_of(base)
        w, r = prices(provider, str(base.get("model") or ""))
        prompt = prompts.get(request, (len(json.dumps(base)) / 4, 0))[0]
        round_cost = r * prompt + args.output_cost_tokens
        outputs: list[str] = []
        strings_of(base, outputs)

        t = totals[provider]
        t["requests"] += 1
        t["rounds"] += len(rounds)
        t["multi_round_requests"] += len(rounds) > 1
        fetched: list[str] = []
        held: set[str] = set()
        left = args.budget
        for k, asked in enumerate(rounds):
            if k >= 1:
                new = [h for h in asked if h not in fetched]
                reachable: set[str] = set()
                for earlier in fetched:
                    reachable.update(siblings_of(outputs, earlier))
                t["later_rounds"] += 1
                if not new:
                    t["repeat_only"] += 1
                elif all(h in reachable for h in new):
                    t["covered_all"] += 1
                    t["saved"] += round_cost
                if any(h in reachable for h in new):
                    t["covered_any"] += 1
            # What the rule would have attached on this round, inside the
            # budget, and what carrying it costs the rest of the request.
            held.update(asked)
            for h in asked:
                left -= sizes.get(h, 0)
            later = len(rounds) - k - 1
            for h in asked:
                for sibling in siblings_of(outputs, h):
                    if sibling in held:
                        continue
                    size = sizes.get(sibling, args.default_sibling_tokens)
                    if size > left:
                        continue
                    left -= size
                    held.add(sibling)
                    t["attached"] += 1
                    t["attached_tokens"] += size
                    t["cost"] += size * (w + r * later)
            fetched.extend(h for h in asked if h not in fetched)

    providers = {}
    later = covered = 0
    for provider, t in sorted(totals.items()):
        later += t["later_rounds"]
        covered += t["covered_all"]
        providers[provider] = {
            "requests": int(t["requests"]),
            "multi_round_requests": int(t["multi_round_requests"]),
            "rounds": int(t["rounds"]),
            "later_rounds": int(t["later_rounds"]),
            "covered_all": int(t["covered_all"]),
            "covered_any": int(t["covered_any"]),
            "repeat_only": int(t["repeat_only"]),
            "attached": int(t["attached"]),
            "attached_tokens": int(t["attached_tokens"]),
            "saved": round(t["saved"], 1),
            "cost": round(t["cost"], 1),
            "net": round(t["saved"] - t["cost"], 1),
        }
    share = covered / later if later else None
    return {
        "capture": str(args.capture),
        "round_source": dict(source_counts),
        "later_rounds": int(later),
        "covered_all": int(covered),
        "share": share,
        "acceptance": ACCEPTANCE,
        "verdict": None if share is None else ("build" if share >= ACCEPTANCE else "do not build"),
        "providers": providers,
    }


def render(result: dict) -> str:
    lines = []
    share = result["share"]
    lines.append(
        f"second-or-later rounds: {result['later_rounds']}, a sibling of an earlier fetch: "
        f"{result['covered_all']}"
        + (f" ({share:.1%}; bar {result['acceptance']:.0%}: {result['verdict']})"
           if share is not None else " (no second-or-later rounds found)")
    )
    lines.append(f"rounds read from: {result['round_source'] or 'nothing'}")
    lines.append(f"{'provider':<10} {'later':>6} {'covered':>8} {'attached':>9} "
                 f"{'saved':>12} {'cost':>12} {'net':>12}")
    for provider, p in result["providers"].items():
        lines.append(f"{provider:<10} {p['later_rounds']:>6} {p['covered_all']:>8} "
                     f"{p['attached']:>9} {p['saved']:>12.0f} {p['cost']:>12.0f} {p['net']:>+12.0f}")
    lines.append("saved / cost / net: siblings on minus off, tokens at the uncached input price")
    return "\n".join(lines)


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--capture", required=True, help="kompressor-bodies.jsonl")
    parser.add_argument("--stats", help="the run's stats database (rounds, prompts, sizes)")
    parser.add_argument("--refs", help="the run's fragment store (sibling sizes)")
    parser.add_argument("--gap-ms", type=int, default=1500,
                        help="asks further apart than this are different rounds (stats only)")
    parser.add_argument("--budget", type=int, default=16384,
                        help="proxy.retrieve_max_tokens_per_request")
    parser.add_argument("--output-cost-tokens", type=float, default=500.0,
                        help="a round's output, in uncached input tokens (an assumption)")
    parser.add_argument("--default-sibling-tokens", type=int, default=500,
                        help="size of a sibling no source knows (an assumption)")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    result = measure(args)
    print(json.dumps(result, indent=2) if args.json else render(result))
    return 0


if __name__ == "__main__":
    sys.exit(main())
