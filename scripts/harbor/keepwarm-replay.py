#!/usr/bin/env python3
"""Replay keep-warm rules over the request log in a `stats.db`.

    scripts/harbor/keepwarm-replay.py <stats.db> [--days N] [--tenant T] [--json]

Every keep-warm change has been replayed over recorded requests before it
shipped (DESIGN §12.10). This is that replay as a tool, for the two candidates
of HEADROOM-PLAN Tier 4, against the rule the proxy runs now:

* `recorded`   -- the pings the log holds, priced from their own usage.
* `current`    -- today's rule: a wait armed on `tool_use` gets
  `--max-pings`, a background wait `--background-pings`, and a request whose
  every marker asks for an hour gets none (Tier 1).
* `prior`      -- `current`, plus `proxy.cache_keep_warm_background_prior`: a
  background wait is not pinged when the conversation's earlier background
  waits mostly did not come back (`--prior-min`, `--prior-threshold`; the rule
  is stated in `crates/kompressor-proxy/src/keepwarm.rs`). Run twice, because
  the log cannot say what some waits were (below).
* `one-hour`   -- `current`, plus `proxy.cache_keep_warm_one_hour`: one ping
  `--one-hour-delay` after an all-one-hour request that stopped on `tool_use`.
* `both`       -- the two together.

For each it reports waits pinged, pings sent, their cost, pauses past the cache
lifetime bridged and lost, pings sent after the conversation's last turn
(`trailing`), and what bridged pauses avoided net of the pings (`net`), in
input-equivalent tokens at the model's rates (`kompressor_stats::cache::rates`).

# What the log can and cannot say

`proxy_requests` keeps no stop reason. A wait is known to have been armed only
when it lasted long enough to send a ping (`<request>.keepwarm<N>` rows), and
what armed it is read from how its pings ended:

* more pings than the background cap: `tool_use`;
* the pings ran out at or below the background cap with the client still
  away, every one reading the cache: a background wait (a watch or wake bounded
  below the cap included);
* anything else -- the client came back first, or a ping found the cache
  gone -- is `unknown`. Each is either a background wait that came back or a
  `tool_use` wait. `prior/returns` takes every one for a background wait: it
  counts as a return and is itself left unpinged when the prior says so, which
  bounds the pauses the prior can lose from above. `prior/ignored` takes none:
  the prior then sees only the waits that ran out, which bounds how often it
  suppresses from above. The live rule also sees the background waits that
  came back before their first ping, which leave no row here, so it suppresses
  less often than either.

A request with no ping whose pause outlasted one interval was not armed, under
any rule that only removes pings or adds them to armed one-hour waits; but an
all-one-hour request recorded after Tier 1 shipped sends no ping, so its
`tool_use` cannot be seen and `one-hour` is only measured on rows from before
it. A request is all-one-hour when the latest cache write in its conversation,
up to and including it, was written entirely at the one-hour TTL. Each ping is
assumed to read the cache (a cold ping ends a series in the proxy; the replay
keeps the series going), and a ping is priced as a read of the prompt the
request sent.

Standard library only.
"""

from __future__ import annotations

import argparse
import json
import math
import sqlite3
import sys
import time
from collections import defaultdict
from dataclasses import dataclass, field

SHORT_TTL_S = 300.0
LONG_TTL_S = 3600.0


def read_rate(model: str | None) -> float:
    """`kompressor_stats::cache::rates` for Anthropic."""
    model = (model or "").removeprefix("anthropic/")
    if model.startswith(("claude-opus-5-5", "claude-sonnet-5-5")):
        return 0.05
    if model.startswith(("claude-fable-5-1", "claude-mythos-5-1")):
        return 0.025
    return 0.1


WRITE_5M, WRITE_1H = 1.25, 2.0


@dataclass
class Wait:
    """One client request and the pause after it."""

    request_id: str
    conversation: str
    start_s: float
    model: str | None
    tokens: int
    one_hour: bool
    # Seconds to the client's next request in the conversation, or None when
    # none came in the log.
    gap_s: float | None
    # What the log says armed it: None (no pings), "tool_use", "background",
    # "unknown".
    kind: str | None
    recorded_pings: int = 0
    recorded_ping_cost: float = 0.0
    recorded_trailing: int = 0

    @property
    def read(self) -> float:
        return read_rate(self.model)

    @property
    def lifetime_s(self) -> float:
        return LONG_TTL_S if self.one_hour else SHORT_TTL_S

    def rewrite_cost(self) -> float:
        write = WRITE_1H if self.one_hour else WRITE_5M
        return self.tokens * (write - self.read)


@dataclass
class Totals:
    waits_pinged: int = 0
    pings: int = 0
    ping_cost: float = 0.0
    bridged: int = 0
    lost: int = 0
    trailing: int = 0
    avoided: float = 0.0
    suppressed: int = 0
    # Pauses `current` bridges that this rule loses.
    newly_lost: int = 0
    trailing_by_kind: dict[str, int] = field(default_factory=lambda: defaultdict(int))

    @property
    def net(self) -> float:
        return self.avoided - self.ping_cost


@dataclass
class Settings:
    interval_s: float = 270.0
    max_pings: int = 11
    background_pings: int = 4
    prior_min: int = 2
    prior_threshold: float = 1 / 3
    one_hour_delay_s: float = 3300.0


def load(db: sqlite3.Connection, tenant: str | None, from_ms: int | None,
         to_ms: int | None, settings: Settings) -> list[Wait]:
    """Every Anthropic client request with a conversation, as a `Wait`."""
    where = ["api_family = 'anthropic_messages'", "conversation IS NOT NULL"]
    args: list = []
    if tenant is not None:
        where.append("tenant = ?")
        args.append(tenant)
    if from_ms is not None:
        where.append("time_ms >= ?")
        args.append(from_ms)
    if to_ms is not None:
        where.append("time_ms < ?")
        args.append(to_ms)
    columns = {row[1] for row in db.execute("PRAGMA table_info(proxy_requests)")}
    messages = ("json_extract(prefix_json, '$.messages_count')"
                if "prefix_json" in columns else "NULL")
    rows = db.execute(
        f"""SELECT request_id, conversation, time_ms, model, status, keep_warm,
                   COALESCE(prompt_tokens, 0), COALESCE(cache_read_tokens, 0),
                   COALESCE(cache_write_tokens, 0), COALESCE(cache_write_1h_tokens, 0),
                   COALESCE(completion_tokens, 0), {messages}
            FROM proxy_requests WHERE {' AND '.join(where)}
            ORDER BY conversation, time_ms, id""",
        args,
    ).fetchall()

    pings: dict[str, list[tuple]] = defaultdict(list)
    clients: dict[str, list[tuple]] = defaultdict(list)
    for row in rows:
        (request_id, conversation, *_rest) = row
        if row[5]:
            parent, _, _ = request_id.rpartition(".keepwarm")
            pings[parent].append(row)
        else:
            clients[conversation].append(row)

    waits: list[Wait] = []
    for conversation, turns in clients.items():
        last_write_one_hour = False
        last_start = turns[-1][2] / 1000.0
        for index, row in enumerate(turns):
            (request_id, _, time_ms, model, status, _, prompt, read, write,
             write_1h, _, count) = row
            if write > 0:
                last_write_one_hour = write_1h >= write
            if status is None or not 200 <= status < 300:
                continue
            start_s = time_ms / 1000.0
            # The client's next request in this conversation: the first later
            # one that does not have fewer messages (`KeepWarm::cancel`).
            gap = None
            for later in turns[index + 1:]:
                if count is None or later[11] is None or later[11] >= count:
                    gap = later[2] / 1000.0 - start_s
                    break
            own = pings.get(request_id, [])
            wait = Wait(
                request_id=request_id,
                conversation=conversation,
                start_s=start_s,
                model=model,
                tokens=prompt + read + write,
                one_hour=last_write_one_hour,
                gap_s=gap,
                kind=classify(own, gap, start_s, settings),
            )
            for ping in own:
                rate = read_rate(ping[3])
                wait.recorded_pings += 1
                wait.recorded_ping_cost += (ping[6] + ping[7] * rate + ping[8] * WRITE_5M
                                            + ping[9] * (WRITE_1H - WRITE_5M))
                if ping[2] / 1000.0 > last_start:
                    wait.recorded_trailing += 1
            waits.append(wait)
    return waits


def classify(pings: list[tuple], gap: float | None, start_s: float,
             settings: Settings) -> str | None:
    """What armed a wait, read from how its pings ended (module docs)."""
    if not pings:
        return None
    count = len(pings)
    if count > settings.background_pings:
        return "tool_use"
    every_read = all(ping[7] > 0 for ping in pings)
    last = pings[-1][2] / 1000.0
    ran_out = gap is None or start_s + gap > last + settings.interval_s
    if every_read and ran_out:
        return "background"
    return "unknown"


def schedule(wait: Wait, pings_allowed: int, interval_s: float) -> tuple[int, float]:
    """Pings sent and the time the cache then lives to, from the wait's start."""
    if wait.gap_s is None:
        sent = pings_allowed
    else:
        sent = min(pings_allowed, max(0, math.ceil(wait.gap_s / interval_s) - 1))
    return sent, sent * interval_s + wait.lifetime_s


def score(totals: Totals, wait: Wait, sent: int, alive_to: float, trailing: bool,
          current_bridged: bool | None) -> bool:
    """Add one wait's outcome; returns whether its pause was bridged."""
    if sent:
        totals.waits_pinged += 1
    totals.pings += sent
    totals.ping_cost += sent * wait.tokens * wait.read
    if trailing and sent:
        totals.trailing += sent
        totals.trailing_by_kind[wait.kind or "none"] += sent
    bridged = False
    # Only waits the log shows armed: an `end_turn` with nothing running is
    # not kept warm under any of these rules, and its pauses are not theirs.
    if wait.kind is not None and wait.gap_s is not None and wait.gap_s > wait.lifetime_s:
        if sent and wait.gap_s <= alive_to:
            bridged = True
            totals.bridged += 1
            totals.avoided += wait.rewrite_cost()
        else:
            totals.lost += 1
            if current_bridged:
                totals.newly_lost += 1
    return bridged


def last_turns(waits: list[Wait]) -> dict[str, float]:
    last: dict[str, float] = {}
    for wait in waits:
        last[wait.conversation] = max(last.get(wait.conversation, 0.0), wait.start_s)
    return last


def replay(waits: list[Wait], settings: Settings) -> dict[str, Totals]:
    """Every rule over the same waits."""
    last = last_turns(waits)
    results = {name: Totals() for name in
               ("recorded", "current", "prior/returns", "prior/ignored", "one-hour", "both")}
    current_bridged: dict[str, bool] = {}

    def is_trailing(wait: Wait) -> bool:
        return wait.start_s >= last[wait.conversation]

    # `current` first: the others report what they lose against it.
    for wait in waits:
        recorded = results["recorded"]
        recorded.pings += wait.recorded_pings
        recorded.waits_pinged += bool(wait.recorded_pings)
        recorded.ping_cost += wait.recorded_ping_cost
        recorded.trailing += wait.recorded_trailing
        if wait.recorded_trailing:
            recorded.trailing_by_kind[wait.kind or "none"] += wait.recorded_trailing
        sent, alive = current_pings(wait, settings)
        current_bridged[wait.request_id] = score(
            results["current"], wait, sent, alive, is_trailing(wait), None)

    for name, prior_mode, one_hour in (
        ("prior/returns", "returns", False),
        ("prior/ignored", "ignored", False),
        ("one-hour", None, True),
        ("both", "returns", True),
    ):
        totals = results[name]
        priors: dict[str, Prior] = defaultdict(Prior)
        for wait in waits:
            prior = priors[wait.conversation]
            prior.resolve(wait.start_s)
            sent, alive = current_pings(wait, settings)
            if wait.one_hour and one_hour and wait.kind == "tool_use":
                sent, alive = one_hour_ping(wait, settings)
            background = wait.kind == "background" or (
                wait.kind == "unknown" and prior_mode == "returns")
            if prior_mode and background and not wait.one_hour:
                if prior.suppresses(settings):
                    totals.suppressed += 1
                    sent, alive = 0, wait.lifetime_s
                cap = settings.background_pings
                prior.pending = wait.start_s + (cap + 1) * settings.interval_s
            score(totals, wait, sent, alive, is_trailing(wait),
                  current_bridged[wait.request_id])
    return results


def current_pings(wait: Wait, settings: Settings) -> tuple[int, float]:
    """Today's rule, Tier 1 included."""
    if wait.kind is None or wait.one_hour:
        return 0, wait.lifetime_s
    cap = settings.max_pings if wait.kind == "tool_use" else settings.background_pings
    if wait.kind == "unknown":
        # Came back before either cap mattered, or was stopped by a cold ping.
        cap = min(settings.max_pings, max(wait.recorded_pings, settings.background_pings))
    return schedule(wait, cap, settings.interval_s)


def one_hour_ping(wait: Wait, settings: Settings) -> tuple[int, float]:
    delay = settings.one_hour_delay_s
    sent = 1 if wait.gap_s is None or wait.gap_s > delay else 0
    return sent, (delay if sent else 0.0) + LONG_TTL_S


@dataclass
class Prior:
    """`keepwarm::Prior`, replayed: background waits whose client came back
    before their pings ran out, or within one interval of the last, and waits
    whose client came later."""

    back: int = 0
    gone: int = 0
    pending: float | None = field(default=None)

    def resolve(self, now: float) -> None:
        if self.pending is None:
            return
        if now <= self.pending:
            self.back += 1
        else:
            self.gone += 1
        self.pending = None

    def suppresses(self, settings: Settings) -> bool:
        seen = self.back + self.gone
        # Never on a conversation's first background wait.
        return (seen >= max(settings.prior_min, 1)
                and (self.back + 1) / (seen + 2) < settings.prior_threshold)


def render(results: dict[str, Totals], waits: list[Wait]) -> str:
    kinds = defaultdict(int)
    for wait in waits:
        kinds[wait.kind or "none"] += 1
    head = (f"{len(waits)} client requests; armed per the log: "
            + ", ".join(f"{k} {v}" for k, v in sorted(kinds.items()) if k != "none"))
    lines = [head, "",
             f"{'rule':<14}{'waits':>7}{'pings':>7}{'ping cost':>13}{'bridged':>9}"
             f"{'lost':>6}{'newly lost':>11}{'trailing':>10}{'suppressed':>11}{'net':>13}"]
    for name, t in results.items():
        bridged = "" if name == "recorded" else str(t.bridged)
        lost = "" if name == "recorded" else str(t.lost)
        net = "" if name == "recorded" else f"{t.net:,.0f}"
        lines.append(f"{name:<14}{t.waits_pinged:>7}{t.pings:>7}{t.ping_cost:>13,.0f}"
                     f"{bridged:>9}{lost:>6}{t.newly_lost:>11}{t.trailing:>10}"
                     f"{t.suppressed:>11}{net:>13}")
    lines.append("")
    for name in ("recorded", "current"):
        by_kind = results[name].trailing_by_kind
        lines.append(f"trailing pings by what armed them, {name}: "
                     + (", ".join(f"{k} {v}" for k, v in sorted(by_kind.items())) or "none"))
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("db")
    parser.add_argument("--tenant")
    parser.add_argument("--days", type=float, help="only the last N days of the log")
    parser.add_argument("--from-ms", type=int)
    parser.add_argument("--to-ms", type=int)
    defaults = Settings()
    parser.add_argument("--interval", type=float, default=defaults.interval_s)
    parser.add_argument("--max-pings", type=int, default=defaults.max_pings)
    parser.add_argument("--background-pings", type=int, default=defaults.background_pings)
    parser.add_argument("--prior-min", type=int, default=defaults.prior_min)
    parser.add_argument("--prior-threshold", type=float, default=defaults.prior_threshold)
    parser.add_argument("--one-hour-delay", type=float, default=defaults.one_hour_delay_s)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    settings = Settings(args.interval, args.max_pings, args.background_pings,
                        args.prior_min, args.prior_threshold, args.one_hour_delay)
    from_ms = args.from_ms
    if args.days is not None:
        db_end = None
        with sqlite3.connect(f"file:{args.db}?mode=ro", uri=True) as db:
            db_end = db.execute("SELECT MAX(time_ms) FROM proxy_requests").fetchone()[0]
        from_ms = int((db_end or time.time() * 1000) - args.days * 86_400_000)
    db = sqlite3.connect(f"file:{args.db}?mode=ro", uri=True)
    waits = load(db, args.tenant, from_ms, args.to_ms, settings)
    results = replay(waits, settings)
    if args.json:
        print(json.dumps({name: {**vars(t), "net": t.net} for name, t in results.items()},
                         indent=2, default=dict))
    else:
        print(render(results, waits))
    return 0


if __name__ == "__main__":
    sys.exit(main())
