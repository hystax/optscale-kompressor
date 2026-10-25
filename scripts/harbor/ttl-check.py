#!/usr/bin/env python3
"""Check a run's cache TTL behaviour from what the provider reported.

    scripts/harbor/ttl-check.py <run-dir>...

Reads every trial's agent trajectory, where Claude Code records each step's
usage including `cache_creation.ephemeral_5m_input_tokens` and
`ephemeral_1h_input_tokens` -- the split the proxy's own stats do not keep. For
each conversation it classifies the start-to-start gap before every request and
asks the questions `proxy.cache_ttl = "adaptive"` stands or falls on:

* **Retained after a pause**: of the cache the previous request held (read +
  written), how much this one read back. Expect ~0 past five minutes on the
  default TTL and ~1 under one-hour entries, below an hour.
* **The switch**: the first request writing one-hour entries. Did it read the
  five-minute entry the previous request left, or rewrite the prefix? The
  provider's documentation does not say, and the estimate assumed it reads.
* **Stray five-minute writes** after the switch: a marker the rewrite missed.

Prices are Claude Opus 5.5's ($4 input, $0.20 cache read, $5 / $8 for 5-minute /
one-hour writes, $20 output, per MTok) unless the step names another model.

Standard library only. Exit status is 0; this reports, it does not gate.
"""

from __future__ import annotations

import datetime as dt
import json
import statistics
import sys
from pathlib import Path

PRICE = {  # $/MTok: input, read, 5m write, 1h write, output
    "claude-opus-5-5": (4.0, 0.20, 5.0, 8.0, 20.0),
    "claude-opus-5": (5.0, 0.50, 6.25, 10.0, 25.0),
    "claude-opus-4-8": (5.0, 0.50, 6.25, 10.0, 25.0),
}
FIVE_MIN, HOUR = 300.0, 3600.0


def steps_with_usage(trajectory: Path):
    data = json.loads(trajectory.read_text())
    for step in data.get("steps", []):
        metrics = step.get("metrics") or {}
        extra = metrics.get("extra") or {}
        if "cache_read_input_tokens" not in extra or not step.get("timestamp"):
            continue
        split = extra.get("cache_creation") or {}
        yield {
            "at": dt.datetime.fromisoformat(step["timestamp"].replace("Z", "+00:00")),
            "model": step.get("model_name") or "claude-opus-5-5",
            "read": extra.get("cache_read_input_tokens") or 0,
            "write": extra.get("cache_creation_input_tokens") or 0,
            "w5": split.get("ephemeral_5m_input_tokens") or 0,
            "w1h": split.get("ephemeral_1h_input_tokens") or 0,
            "fresh": max((metrics.get("prompt_tokens") or 0)
                         - (extra.get("cache_read_input_tokens") or 0)
                         - (extra.get("cache_creation_input_tokens") or 0), 0),
            "out": metrics.get("completion_tokens") or 0,
        }


def cost(step) -> float:
    inp, read, w5, w1h, out = PRICE.get(step["model"], PRICE["claude-opus-5-5"])
    return (step["fresh"] * inp + step["read"] * read + step["w5"] * w5
            + step["w1h"] * w1h + step["out"] * out) / 1e6


def check_trial(trajectory: Path) -> dict:
    steps = list(steps_with_usage(trajectory))
    out = {"steps": len(steps), "cost": sum(map(cost, steps)), "w1h": 0, "w5": 0,
           "short": [], "medium": [], "long": [], "switch": None, "stray_5m": 0}
    # A switch is a move away from five-minute writes, so it needs one before
    # it. A trial that asked for an hour from its first request
    # (`cache_ttl = "1h"`) never switches, whatever its first step wrote.
    switched = False
    wrote_5m = bool(steps) and steps[0]["w5"] > 0
    for prev, step in zip(steps, steps[1:]):
        gap = (step["at"] - prev["at"]).total_seconds()
        held = prev["read"] + prev["write"]
        retained = step["read"] / held if held else None
        bucket = "short" if gap <= FIVE_MIN else "medium" if gap <= HOUR else "long"
        if retained is not None:
            out[bucket].append(retained)
        if step["w1h"] and not switched and wrote_5m:
            switched = True
            out["switch"] = {"gap_s": round(gap), "retained": retained,
                             "w1h": step["w1h"], "held": held}
        elif switched and step["w5"]:
            out["stray_5m"] += step["w5"]
        wrote_5m = wrote_5m or step["w5"] > 0
    for step in steps:
        out["w1h"] += step["w1h"]
        out["w5"] += step["w5"]
    return out


def mean(values):
    return f"{statistics.mean(values):.2f} (n={len(values)})" if values else "-"


def main() -> int:
    for run in map(Path, sys.argv[1:]):
        config = next(run.glob("kompressor-effective-config.toml"), None)
        ttl = "?"
        if config:
            for line in config.read_text().splitlines():
                if line.strip().startswith("cache_ttl"):
                    ttl = line.split("=", 1)[1].strip().strip('"')
        print(f"== {run}  cache_ttl={ttl}")
        for trajectory in sorted(run.glob("*/*/agent/trajectory.json")):
            r = check_trial(trajectory)
            task = trajectory.parent.parent.name.split("__")[0]
            print(f"  {task:32} steps {r['steps']:>3}  ${r['cost']:6.2f}  "
                  f"writes 5m {r['w5']:>9,} 1h {r['w1h']:>9,}")
            print(f"    retained after gap <=5m {mean(r['short'])}  5-60m {mean(r['medium'])}  "
                  f">60m {mean(r['long'])}")
            if r["switch"]:
                s = r["switch"]
                verdict = ("read the 5-minute entry" if s["retained"] and s["retained"] > 0.8
                           else "REWROTE the prefix on switching")
                print(f"    switch to 1h after a {s['gap_s']}s gap: retained {s['retained']:.2f} "
                      f"of {s['held']:,} held, wrote {s['w1h']:,} 1h -> {verdict}")
            if r["stray_5m"]:
                print(f"    !! {r['stray_5m']:,} tokens written at 5m after the switch: a marker was missed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
