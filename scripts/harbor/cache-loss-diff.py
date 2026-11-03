#!/usr/bin/env python3
"""Say whether a run's prompt-cache losses were ours, and which way.

    scripts/harbor/cache-loss-diff.py <run-dir>...

Exits non-zero when any loss is ours, so it can gate a run.

# Why this is a tool and not a paragraph

The benchmark's own flag has told the reader to diff the captured bodies since
the day the check existed, and that instruction is right — every cause ever
found here was settled by diffing, and several confident readings taken from the
aggregates alone had to be retracted. But the procedure was only ever prose, so
it was retyped by hand each time, and a hand-retyped procedure is one that is
subtly different each time.

It also got the direction wrong. The obvious way to tell a promotion from a
demotion is to look for the marker text, and the obvious marker text is
`[... N body lines omitted from NAME; retrieve: ref:k_…]`. That is the *code*
compressor's shape (`crates/kompressor-proxy/src/tool.rs`). Every other
compressor leaves a bare `ref:k_…`, so a textbook promotion by any of them came
back unclassified — observed 2026-10-09, where a config payload went from one
marker to 10,663 characters of inline text and the direction read as "other".

So direction is decided by **counting `ref:k_` markers**, which every compressor
leaves regardless of how it phrases the rest:

    markers 1 -> 0   a PROMOTION: a fragment came back inline. Costs the prefix
                     once, and is a decision the engine is allowed to take.
    markers 0 -> 1   a DEMOTION: a message that shipped whole was elided again.
                     Always a bug — it means the decision was re-taken against a
                     bar that had moved, and the prefix was rewritten twice.

# What it compares

`tools`, `instructions`/`system` and the conversation separately, because they
fail for different reasons and the same total hides which. Unchanged captured prefixes suggest a provider/cache cause but do not prove it.
Ingress is compared before assigning changed content to the proxy; metadata-only
changes remain unresolved rather than being called compression damage.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import re
import sqlite3
import sys
from pathlib import Path

HANDLE = re.compile(r"ref:(k_[A-Za-z0-9_\-]+)")
SELF = Path(__file__).resolve().parent


def bench():
    """`cache_losses` and its three exclusions, rather than a second copy of them."""
    spec = importlib.util.spec_from_file_location("bench", SELF / "bench.py")
    module = importlib.util.module_from_spec(spec)
    argv, sys.argv = sys.argv, ["bench.py"]
    try:
        spec.loader.exec_module(module)
    finally:
        sys.argv = argv
    return module


# The one tool in that array that is ours. Everything else belongs to the agent.
OUR_TOOL = "optscale_kompressor_retrieve"


def split_tools(tools) -> tuple[str, str]:
    """The tools array, split into ours and the agent's.

    Compared together they answer the wrong question. A tools block is part of
    the cached prefix whoever wrote it, so any change to it breaks the cache --
    but only a change to *our* entry is a fault of this proxy, and the agent
    rewrites its own list for reasons of its own. Measured on the 89-task run of
    2026-10-12: one turn went from eleven tools to one as codex withdrew all ten
    of its own (it offers a `tool_search` and defers the schemas), while
    `optscale_kompressor_retrieve` stayed byte-identical. Comparing the whole array
    called that OURS, on a $98 run, and it was the client's doing.
    """
    tools = tools or []
    ours = [x for x in tools if (x.get("name") or x.get("type")) == OUR_TOOL]
    theirs = [x for x in tools if (x.get("name") or x.get("type")) != OUR_TOOL]
    return json.dumps(ours, sort_keys=True), json.dumps(theirs, sort_keys=True)


def parts(body: str) -> tuple[str, str, str, list]:
    """The things a provider caches, kept apart -- and the tools split by owner."""
    value = json.loads(body)
    our_tools, their_tools = split_tools(value.get("tools"))
    return (
        our_tools,
        their_tools,
        json.dumps(value.get("instructions") or value.get("system")),
        value.get("input") or value.get("messages") or [],
    )


def text_of(message) -> str:
    """Every string a message carries, so a marker anywhere in it is counted."""
    return json.dumps(message)


def direction(before: str, after: str) -> str:
    """Promotion or demotion, by marker count rather than by marker wording."""
    was, now = len(HANDLE.findall(before)), len(HANDLE.findall(after))
    if was > now:
        return f"PROMOTION (markers {was} -> {now}: a fragment came back inline)"
    if now > was:
        return f"DEMOTION (markers {was} -> {now}: a message shipped whole was elided again) -- ALWAYS A BUG"
    return f"neither (markers {was} -> {now}); the bytes changed some other way"


def normalize_content(value):
    """Content comparison only; raw differences remain visible in the report.

    Cache metadata and string/text-block representation can affect caching,
    but neither is evidence that the compressor changed historical content.
    """
    if isinstance(value, list):
        return [normalize_content(item) for item in value]
    if isinstance(value, dict):
        result = {key: (item if key in ("input", "arguments") else normalize_content(item))
                  for key, item in value.items() if key != "cache_control"}
        if isinstance(result.get("content"), str):
            result["content"] = [{"type": "text", "text": result["content"]}]
        return result
    return value


def classify_change(before, after, incoming_before=None, incoming_after=None):
    """Attribute changed content only when ingress/egress establishes ownership."""
    a, b = json.loads(before), json.loads(after)
    ia = json.loads(incoming_before) if incoming_before is not None else None
    ib = json.loads(incoming_after) if incoming_after is not None else None
    if split_tools(a.get("tools"))[0] != split_tools(b.get("tools"))[0]:
        return "PROXY", "retrieve tool changed"

    changes = []
    missing = object()
    def compare(old, new, old_in, new_in, label):
        if old == new:
            return
        if normalize_content(old) == normalize_content(new):
            changes.append(("METADATA", label))
        elif ia is None or ib is None or old_in is missing or new_in is missing:
            changes.append(("UNKNOWN", label + " (missing ingress)"))
        elif normalize_content(old_in) == normalize_content(new_in):
            changes.append(("PROXY", label))
        # Normalized, so a breakpoint the proxy moved does not hide that the
        # client is the one that changed the content.
        elif (normalize_content(old) == normalize_content(old_in)
              and normalize_content(new) == normalize_content(new_in)):
            changes.append(("CLIENT", label))
        else:
            changes.append(("UNKNOWN", label + " (client and proxy transformations)"))

    for key in ("system", "instructions"):
        compare(a.get(key), b.get(key), ia.get(key) if ia else None,
                ib.get(key) if ib else None, key)
    compare(split_tools(a.get("tools"))[1], split_tools(b.get("tools"))[1],
            split_tools(ia.get("tools"))[1] if ia else None,
            split_tools(ib.get("tools"))[1] if ib else None, "agent tools")
    am = a.get("input") or a.get("messages") or []
    bm = b.get("input") or b.get("messages") or []
    aim = (ia.get("input") or ia.get("messages") or []) if ia else []
    bim = (ib.get("input") or ib.get("messages") or []) if ib else []
    # Responses can carry string input rather than message arrays.
    if not isinstance(am, list) or not isinstance(bm, list):
        compare(am, bm, aim, bim, "input")
    else:
        if len(bm) < len(am):
            changes.append(("UNKNOWN", "history shortened"))
        for index, (old, new) in enumerate(zip(am, bm)):
            compare(old, new,
                    aim[index] if isinstance(aim, list) and index < len(aim) else missing,
                    bim[index] if isinstance(bim, list) and index < len(bim) else missing,
                    f"message {index}")
    # Do not let a client or metadata change hide a genuine proxy rewrite.
    for owner in ("PROXY", "UNKNOWN", "CLIENT", "METADATA"):
        if any(kind == owner for kind, _ in changes):
            return owner, "; ".join(f"{kind}: {label}" for kind, label in changes)
    return "PROVIDER_CANDIDATE", "captured prefix unchanged; provider/cache cause unproven"


def analyse(run: Path, module) -> int:
    stats, bodies = run / "kompressor-stats.db", run / "kompressor-bodies.jsonl"
    if not stats.exists() or not bodies.exists():
        print(f"{run.name}: no stats or bodies capture; nothing to diff")
        return 0

    con = sqlite3.connect(f"file:{stats}?mode=ro", uri=True)
    con.row_factory = sqlite3.Row
    # `messages_total` comes from the engine's row, not the proxy's, and
    # `cache_losses` needs it: its session-boundary exclusion is inert without
    # it, because a missing count reads as unknown rather than as a drop. This
    # tool read `proxy_requests` alone and so silently ran two exclusions where
    # the benchmark runs three -- the same shared function answering differently
    # for its two callers. Measured on the 89-task run of 2026-10-13, where that
    # cost one 27,008-token loss misattributed to us.
    rows = [dict(r) for r in con.execute(
        "SELECT p.*, o.messages_total FROM proxy_requests p "
        "LEFT JOIN optimization_events o USING (request_id) ORDER BY p.time_ms")]
    order = {r["request_id"]: n + 1 for n, r in enumerate(rows)}

    out = {}
    incoming = {}
    for line in open(bodies, encoding="utf-8", errors="replace"):
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        # The same file carries the relay's per-frame decisions, which have a
        # `dir` and no `body`.
        if "body" in record and record.get("dir") == "proxy->upstream":
            # First round matches the client request's compression decision;
            # hidden retrieval rounds must not overwrite this capture.
            out.setdefault(record["request_id"], record["body"])
        elif "body" in record and record.get("dir") == "client->proxy":
            incoming.setdefault(record["request_id"], record["body"])

    fetched = {}
    events = run / "retrieval_events.json"
    if events.exists():
        try:
            for event in json.load(open(events)):
                fetched.setdefault(event["ref"], []).append(order.get(event["request_id"]))
        except (OSError, json.JSONDecodeError, KeyError):
            pass

    losses = module.cache_losses(rows)
    if not losses:
        print(f"{run.name}: no cache loss")
        return 0

    ours = 0
    print(f"{run.name}: {len(losses)} loss(es)")
    for loss in losses:
        print(
            f"\n  {loss['was']:,} -> {loss['now']:,}  lost {loss['lost']:,}"
            f"  gap {loss['gap_ms'] / 1000:.1f}s"
        )
        by_id = {row["request_id"]: row for row in rows}
        before = by_id.get(loss.get("before_request_id"))
        after = by_id.get(loss.get("request_id"))
        if not (before and after and before["request_id"] in out and after["request_id"] in out):
            print("    no body pair captured for this turn; cannot say")
            continue

        a_ours, a_theirs, a_instr, a_msgs = parts(out[before["request_id"]])
        b_ours, b_theirs, b_instr, b_msgs = parts(out[after["request_id"]])
        print(f"    our tool identical {a_ours == b_ours}   "
              f"agent tools identical {a_theirs == b_theirs}   "
              f"instructions identical {a_instr == b_instr}")

        differing = None
        for index in range(min(len(a_msgs), len(b_msgs))):
            if json.dumps(a_msgs[index], sort_keys=True) != json.dumps(b_msgs[index], sort_keys=True):
                differing = index
                break
        print(f"    messages {len(a_msgs)} -> {len(b_msgs)}, first differing index: {differing}")

        owner, reason = classify_change(
            out[before["request_id"]], out[after["request_id"]],
            incoming.get(before["request_id"]), incoming.get(after["request_id"]))
        print(f"    -> {owner}: {reason}")
        if owner == "METADATA":
            print("       content unchanged after normalization; metadata/representation "
                  "and expiry remain candidates, not a proven proxy content rewrite")
        if owner != "PROXY":
            continue
        ours += 1
        # Show the content rewrite, not an earlier moving cache breakpoint.
        differing = next((i for i, (old, new) in enumerate(zip(a_msgs, b_msgs))
                          if normalize_content(old) != normalize_content(new)), None)
        if differing is None:
            continue
        was, now = text_of(a_msgs[differing]), text_of(b_msgs[differing])
        print(f"       message {differing}: {len(was):,} -> {len(now):,} chars")
        print(f"       {direction(was, now)}")
        for handle in set(HANDLE.findall(was)) - set(HANDLE.findall(now)):
            turns = [t for t in fetched.get(handle, []) if t]
            print(f"       {handle} was fetched {len(turns)}x" + (f" on turns {turns}" if turns else ""))
    return ours


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("runs", nargs="+", type=Path)
    args = parser.parse_args()
    module = bench()
    ours = sum(analyse(run, module) for run in args.runs)
    if ours:
        print(f"\n{ours} loss(es) contain proxy-owned changes. A demotion is always a bug; a promotion is a priced decision.")
    return 1 if ours else 0


if __name__ == "__main__":
    sys.exit(main())
