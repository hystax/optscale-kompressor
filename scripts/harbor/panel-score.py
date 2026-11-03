#!/usr/bin/env python3
"""Score a paired panel of Harbor runs: cost per step and per solved task.

    scripts/harbor/panel-score.py --arm off='runs/harbor_tasks_off_anthropic_panel-off-*/*/' \
                                  --arm rec='runs/harbor_tasks_recoverable_anthropic_panel-rec-*/*/'

Each `--arm NAME=GLOB` names the run directories of one arm; the first arm is
the baseline. Costs are the **proxy's bill**: every upstream request priced at
its model's rates from the usage the provider reported. Harbor's own cost is
printed beside it and not used, because it only sees the requests in the
agent's trajectory: on the two 89-task Anthropic runs it under-counted the
uncompressed arm by $5.11 and the compressed arm by $2.66, which moved the
comparison by nine points on its own.

Requests are assigned to tasks by the tool-call ids that appear in each
trial's trajectory, and from there to every request opening with the same first
user message -- the task's instruction -- which names a trial in both arms. (The
proxy's conversation key cannot: a passthrough run never computes one, and
scoring by it left each OFF conversation's opening request unassigned, which
made OFF look 30% cheaper per step than it was.)
Whatever cannot be assigned is reported as unassigned rather than guessed at.

Arms are compared task by task (the mean over a task's trials in each arm),
on three ratios against the baseline:

* cost per agent step -- divides out how long the agent took, which is the
  largest noise term: per-task log cost per step varied with sd 0.27-0.37
  between two runs of the same configuration;
* cost per trial;
* cost per solved task, pooled over the panel.

Intervals are 95% bootstrap over tasks (2,000 resamples, fixed seed), the unit
the noise lives in. Standard library only.
"""

from __future__ import annotations

import argparse
import collections
import glob
import json
import math
import os
import random
import re
import statistics
import sys

PRICE = {  # $/MTok: input, cache read, 5m write, 1h write, output
    "claude-opus-5-5": (4.0, 0.20, 5.0, 8.0, 20.0),
    "claude-opus-5": (5.0, 0.50, 6.25, 10.0, 25.0),
    "claude-opus-4-8": (5.0, 0.50, 6.25, 10.0, 25.0),
    "claude-sonnet-5-5": (2.0, 0.20, 2.5, 4.0, 10.0),
    "claude-haiku-5-5": (0.10, 0.01, 0.125, 0.20, 0.50),
}
TOOL_ID = re.compile(r"toolu_[A-Za-z0-9]{10,}")


def load_json(path, default=None):
    try:
        with open(path, encoding="utf-8") as fh:
            return json.load(fh)
    except (OSError, ValueError):
        return default


def trials(run: str) -> dict:
    """task -> {solved, steps, harbor_cost, errored, tool_ids} for one run dir."""
    out = {}
    for d in glob.glob(os.path.join(run, "*", "*__*", "")):
        task = os.path.basename(d.rstrip("/")).split("__")[0]
        result = load_json(os.path.join(d, "result.json"), {}) or {}
        traj_path = os.path.join(d, "agent", "trajectory.json")
        text = open(traj_path, encoding="utf-8", errors="replace").read() if os.path.exists(traj_path) else ""
        steps = len((json.loads(text) if text else {}).get("steps", []))
        reward = ((result.get("verifier_result") or {}).get("rewards") or {}).get("reward")
        out[task] = {
            "solved": 1.0 if reward == 1.0 else 0.0,
            "steps": steps,
            "harbor_cost": (result.get("agent_result") or {}).get("cost_usd") or 0.0,
            "errored": bool((result.get("exception_info") or {}).get("exception_type")),
            "tool_ids": set(TOOL_ID.findall(text)),
        }
    return out


def proxy_costs(run: str, tasks: dict) -> tuple[dict, float]:
    """Proxy-billed dollars per task, and the unassigned remainder."""
    owner = {i: task for task, t in tasks.items() for i in t["tool_ids"]}
    model, task_of, opening_of = {}, {}, {}
    opening_votes = collections.defaultdict(collections.Counter)
    bodies = os.path.join(run, "kompressor-bodies.jsonl")
    if os.path.exists(bodies):
        for line in open(bodies, encoding="utf-8", errors="replace"):
            if '"client->proxy"' not in line[:400]:
                continue
            record = json.loads(line)
            raw = record["body"]
            body = raw if isinstance(raw, str) else json.dumps(raw)
            found = re.search(r'"model"\s*:\s*"([^"]+)"', body)
            if found:
                model.setdefault(record["request_id"], found.group(1))
            # The first user message carries the task's instruction, so it names
            # the trial in both arms. The proxy's own conversation key does not:
            # a passthrough run never computes one.
            try:
                messages = (json.loads(raw) if isinstance(raw, str) else raw).get("messages") or []
            except (ValueError, AttributeError):
                messages = []
            first = next((m for m in messages if m.get("role") == "user"), None)
            if first is not None:
                opening_of[record["request_id"]] = hash(json.dumps(first.get("content"), sort_keys=True))
            votes = collections.Counter(owner[i] for i in set(TOOL_ID.findall(body)) if i in owner)
            if votes:
                task = votes.most_common(1)[0][0]
                task_of[record["request_id"]] = task
                if record["request_id"] in opening_of:
                    opening_votes[opening_of[record["request_id"]]][task] += 1
    by_opening = {key: votes.most_common(1)[0][0] for key, votes in opening_votes.items()}
    rows = load_json(os.path.join(run, "proxy_requests.json"), []) or []
    rows = rows if isinstance(rows, list) else rows.get("requests", [])
    by_conversation = {}
    for row in rows:
        if row["request_id"] in task_of and row.get("conversation") is not None:
            by_conversation.setdefault(row["conversation"], task_of[row["request_id"]])
    cost, unassigned = collections.Counter(), 0.0
    unpriced = collections.Counter()
    for row in rows:
        name = row.get("model") or model.get(row["request_id"])
        price = PRICE.get(name or "")
        if not price:
            # Visible, not skipped: a model missing from the table is cost the
            # comparison would otherwise silently leave out of one arm.
            if any(row.get(k) for k in ("prompt_tokens", "cache_read_tokens", "cache_write_tokens", "completion_tokens")):
                unpriced[name or "unknown"] += 1
            continue
        inp, read, w5, w1, out = price
        # One-hour writes cost 2x input, five-minute ones 1.25x; the split is
        # recorded from schema v11. Without it every write is priced five-minute,
        # which under-prices an arm that asked for one-hour entries.
        written = row.get("cache_write_tokens") or 0
        hour = min(row.get("cache_write_1h_tokens") or 0, written)
        dollars = ((row.get("prompt_tokens") or 0) * inp + (row.get("cache_read_tokens") or 0) * read
                   + (written - hour) * w5 + hour * w1 + (row.get("completion_tokens") or 0) * out) / 1e6
        task = (task_of.get(row["request_id"])
                or by_opening.get(opening_of.get(row["request_id"]))
                or by_conversation.get(row.get("conversation")))
        if task:
            cost[task] += dollars
        else:
            unassigned += dollars
    return cost, unassigned, unpriced


def load_arm(pattern: str) -> tuple[dict, dict]:
    """task -> list of trial dicts with a proxy cost, plus arm totals."""
    per_task = collections.defaultdict(list)
    totals = collections.Counter()
    for run in sorted(glob.glob(pattern)):
        tasks = trials(run)
        if not tasks:
            continue
        cost, unassigned, unpriced = proxy_costs(run, tasks)
        totals["runs"] += 1
        totals["unassigned"] += unassigned
        for name, count in unpriced.items():
            totals[f"unpriced:{name}"] += count
        totals["split_known"] += any(r.get("cache_write_1h_tokens") is not None
                                     for r in (load_json(os.path.join(run, "proxy_requests.json"), []) or []))
        for task, t in tasks.items():
            if t["errored"] or t["steps"] == 0:
                totals["errored"] += 1
                continue
            t = dict(t, cost=cost.get(task, 0.0))
            del t["tool_ids"]
            per_task[task].append(t)
            totals["trials"] += 1
            totals["cost"] += t["cost"]
            totals["harbor"] += t["harbor_cost"]
            totals["solved"] += t["solved"]
            totals["steps"] += t["steps"]
    return per_task, totals


def mean_of(trials_: list, key: str) -> float:
    return statistics.mean(t[key] for t in trials_)


def bootstrap(tasks: list, stat, seed: int = 7, n: int = 2000) -> tuple[float, float, float]:
    rng = random.Random(seed)
    point = stat(tasks)
    draws = sorted(stat([rng.choice(tasks) for _ in tasks]) for _ in range(n))
    return point, draws[int(0.025 * n)], draws[int(0.975 * n) - 1]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--arm", action="append", required=True, metavar="NAME=GLOB")
    args = ap.parse_args()
    arms = [tuple(a.split("=", 1)) for a in args.arm]
    loaded = {name: load_arm(pattern) for name, pattern in arms}

    print(f"{'arm':10} {'runs':>4} {'trials':>6} {'errored':>7} {'solved':>7} {'proxy $':>9} "
          f"{'harbor $':>9} {'unassigned $':>12} {'$/step':>8} {'$/solved':>9}")
    for name, (_, tot) in loaded.items():
        per_step = tot["cost"] / tot["steps"] if tot["steps"] else 0
        per_solved = tot["cost"] / tot["solved"] if tot["solved"] else float("nan")
        print(f"{name:10} {tot['runs']:>4} {tot['trials']:>6} {tot['errored']:>7} "
              f"{tot['solved']:>7.0f} {tot['cost']:>9.2f} {tot['harbor']:>9.2f} {tot['unassigned']:>12.2f} "
              f"{per_step:>8.4f} {per_solved:>9.3f}")

    for name, (_, tot) in loaded.items():
        unpriced = {k.split(":", 1)[1]: v for k, v in tot.items() if k.startswith("unpriced:")}
        if unpriced:
            print(f"  !! {name}: requests with usage but no price, left out: {unpriced}")
        if tot["runs"] and tot["split_known"] < tot["runs"]:
            print(f"  note {name}: {tot['runs'] - tot['split_known']} run(s) recorded no 5m/1h write split "
                  "(stats before schema v11); their writes are priced at the five-minute rate")

    base_name, (base, _) = arms[0][0], loaded[arms[0][0]]
    for name, _ in arms[1:]:
        other = loaded[name][0]
        shared = sorted(t for t in base if t in other)
        if not shared:
            print(f"\n{name}: no tasks in common with {base_name}")
            continue

        def log_ratio(key_fn):
            return lambda ts: statistics.mean(math.log(key_fn(other[t]) / key_fn(base[t])) for t in ts)

        per_step = lambda trials_: mean_of(trials_, "cost") / mean_of(trials_, "steps")
        per_trial = lambda trials_: mean_of(trials_, "cost")
        usable = [t for t in shared if per_step(base[t]) > 0 and per_step(other[t]) > 0]

        def per_solved(ts):
            a = sum(mean_of(base[t], "cost") for t in ts) / max(sum(mean_of(base[t], "solved") for t in ts), 1e-9)
            b = sum(mean_of(other[t], "cost") for t in ts) / max(sum(mean_of(other[t], "solved") for t in ts), 1e-9)
            return math.log(b / a)

        print(f"\n{name} vs {base_name}: {len(usable)} tasks in common "
              f"({sum(len(base[t]) for t in usable)} vs {sum(len(other[t]) for t in usable)} trials)")
        for label, stat in (("cost per step", log_ratio(per_step)),
                            ("cost per trial", log_ratio(per_trial)),
                            ("cost per solved task", per_solved)):
            point, lo, hi = bootstrap(usable, stat)
            print(f"  {label:22} {math.exp(point) - 1:+7.1%}   95% CI {math.exp(lo) - 1:+7.1%} .. {math.exp(hi) - 1:+7.1%}")
        solve = lambda ts, arm: statistics.mean(mean_of(arm[t], "solved") for t in ts)
        print(f"  solve rate             {solve(usable, base):.1%} -> {solve(usable, other):.1%}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
