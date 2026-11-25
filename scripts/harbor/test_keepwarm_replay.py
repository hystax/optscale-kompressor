#!/usr/bin/env python3
"""Offline regressions: python3 -m unittest discover -s scripts/harbor -p 'test_*.py'."""
import contextlib
import importlib.util
import io
import json
import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "keepwarm_replay", Path(__file__).with_name("keepwarm-replay.py"))
replay = importlib.util.module_from_spec(spec)
sys.modules["keepwarm_replay"] = replay
spec.loader.exec_module(replay)

T0 = 1_790_000_000_000
TOKENS = 100_000


class Log:
    """A synthetic `proxy_requests`, in the columns the replay reads."""

    def __init__(self):
        self.rows = []

    def request(self, conversation, request_id, at_s, messages, one_hour=False,
                write=1_000):
        self.rows.append(dict(
            request_id=request_id, conversation=conversation, time_ms=T0 + int(at_s * 1000),
            keep_warm=0, read=TOKENS - write, write=write,
            write_1h=write if one_hour else 0, messages=messages))

    def pings(self, conversation, request_id, at_s, count, interval=270):
        for n in range(1, count + 1):
            self.rows.append(dict(
                request_id=f"{request_id}.keepwarm{n}", conversation=conversation,
                time_ms=T0 + int((at_s + n * interval) * 1000), keep_warm=1,
                read=TOKENS, write=0, write_1h=0, messages=None))

    def db(self, path=":memory:"):
        db = sqlite3.connect(path)
        db.execute("""CREATE TABLE proxy_requests (
            id INTEGER PRIMARY KEY, time_ms INTEGER, tenant TEXT, request_id TEXT,
            api_family TEXT, model TEXT, status INTEGER, keep_warm INTEGER,
            prompt_tokens INTEGER, completion_tokens INTEGER, cache_read_tokens INTEGER,
            cache_write_tokens INTEGER, cache_write_1h_tokens INTEGER, conversation TEXT,
            prefix_json TEXT)""")
        for row in self.rows:
            prefix = (json.dumps(dict(messages_count=row["messages"]))
                      if row["messages"] is not None else None)
            db.execute(
                "INSERT INTO proxy_requests (time_ms, tenant, request_id, api_family, model,"
                " status, keep_warm, prompt_tokens, completion_tokens, cache_read_tokens,"
                " cache_write_tokens, cache_write_1h_tokens, conversation, prefix_json)"
                " VALUES (?, '', ?, 'anthropic_messages', 'claude-opus-5-5', 200, ?, 2, 1,"
                " ?, ?, ?, ?, ?)",
                (row["time_ms"], row["request_id"], row["keep_warm"], row["read"],
                 row["write"], row["write_1h"], row["conversation"], prefix))
        db.commit()
        return db


def run(log, **settings):
    settings = replay.Settings(**settings)
    waits = replay.load(log.db(), None, None, None, settings)
    return waits, replay.replay(waits, settings)


class Classify(unittest.TestCase):
    def test_a_wait_is_named_by_how_its_pings_ended(self):
        log = Log()
        # More pings than the background cap: only a tool_use gets those.
        log.request("a", "a1", 0, 3)
        log.pings("a", "a1", 0, 6)
        log.request("a", "a2", 6 * 270 + 100, 5)
        # Four pings, then nothing for long after the fifth was due.
        log.request("b", "b1", 0, 3)
        log.pings("b", "b1", 0, 4)
        log.request("b", "b2", 4000, 5)
        # One ping, and the client was back before the second.
        log.request("c", "c1", 0, 3)
        log.pings("c", "c1", 0, 1)
        log.request("c", "c2", 400, 5)
        # A quick turn sends no ping and says nothing.
        log.request("d", "d1", 0, 3)
        log.request("d", "d2", 10, 5)
        waits, _ = run(log)
        kinds = {wait.request_id: wait.kind for wait in waits}
        self.assertEqual(kinds["a1"], "tool_use")
        self.assertEqual(kinds["b1"], "background")
        self.assertEqual(kinds["c1"], "unknown")
        self.assertIsNone(kinds["d1"])

    def test_a_shorter_request_is_another_conversation_not_this_one_coming_back(self):
        log = Log()
        log.request("a", "a1", 0, 9)
        log.pings("a", "a1", 0, 2)
        log.request("a", "other", 300, 2)
        log.request("a", "a2", 700, 11)
        waits, _ = run(log)
        self.assertEqual(waits[0].gap_s, 700)


class Rules(unittest.TestCase):
    def test_current_leaves_every_one_hour_wait_unpinged(self):
        log = Log()
        log.request("h", "h1", 0, 3, one_hour=True)
        log.pings("h", "h1", 0, 7)
        log.request("h", "h2", 2000, 5, one_hour=True, write=0)
        waits, results = run(log)
        self.assertTrue(waits[0].one_hour)
        self.assertEqual(results["recorded"].pings, 7)
        self.assertEqual(results["current"].pings, 0)
        self.assertEqual(results["current"].lost, 0, "under an hour: the entry lived")

    def test_the_prior_stops_pinging_where_background_waits_never_come_back(self):
        log = Log()
        # Dead ends: each background wait runs out its four pings and the
        # client is next seen long after the cache lapsed.
        for n, at in enumerate((0, 5000, 10000)):
            log.request("dead", f"d{n}", at, 3 + 2 * n)
            log.pings("dead", f"d{n}", at, 4)
        # Waits that come back while the last ping's entry still lives.
        for n in range(4):
            at = n * 1300
            log.request("live", f"l{n}", at, 3 + 2 * n)
            log.pings("live", f"l{n}", at, 4)
        log.request("live", "l-end", 4 * 1300, 20)
        waits, results = run(log, prior_min=2, prior_threshold=0.3)
        self.assertEqual([w.kind for w in waits if w.conversation == "dead"],
                         ["background"] * 3)
        current, prior = results["current"], results["prior/ignored"]
        self.assertEqual(prior.suppressed, 1, "the third dead end, after two observed")
        self.assertEqual(current.pings - prior.pings, 4)
        self.assertEqual(current.trailing - prior.trailing, 4)
        self.assertEqual(prior.newly_lost, 0)
        self.assertEqual(prior.bridged, current.bridged)
        self.assertGreater(current.bridged, 0, "the live conversation's waits are bridged")
        self.assertEqual(results["prior/returns"].suppressed, 1)

    def test_the_first_background_wait_is_never_suppressed(self):
        log = Log()
        log.request("dead", "d0", 0, 3)
        log.pings("dead", "d0", 0, 4)
        # Whatever the settings, a conversation's first background wait is
        # pinged as it is today: there is nothing yet to judge it by.
        for prior_min in (0, 1):
            _, results = run(log, prior_min=prior_min, prior_threshold=1.0)
            self.assertEqual(results["prior/ignored"].suppressed, 0)
            self.assertEqual(results["prior/ignored"].pings, 4)

    def test_one_ping_near_the_hour_bridges_a_long_one_hour_wait(self):
        log = Log()
        # A tool_use wait on one-hour entries, back after 80 minutes.
        log.request("h", "h1", 0, 3, one_hour=True)
        log.pings("h", "h1", 0, 11)
        log.request("h", "h2", 4800, 5, one_hour=True, write=0)
        # Another, back after 40 minutes: before the ping is due.
        log.request("h", "h3", 10000, 7, one_hour=True, write=0)
        log.pings("h", "h3", 10000, 8)
        log.request("h", "h4", 12400, 9, one_hour=True, write=0)
        _, results = run(log)
        current, one_hour = results["current"], results["one-hour"]
        self.assertEqual((current.pings, current.bridged, current.lost), (0, 0, 1))
        self.assertEqual((one_hour.pings, one_hour.bridged, one_hour.lost), (1, 1, 0))
        self.assertAlmostEqual(one_hour.ping_cost, (TOKENS + 2) * 0.05, msg="the prompt the request sent, at the read price")
        self.assertAlmostEqual(one_hour.avoided, (TOKENS + 2) * (2.0 - 0.05))

    def test_a_background_one_hour_wait_gets_no_hour_ping(self):
        log = Log()
        log.request("h", "h1", 0, 3, one_hour=True)
        log.pings("h", "h1", 0, 4)
        log.request("h", "h2", 4800, 5, one_hour=True, write=0)
        _, results = run(log)
        self.assertEqual(results["one-hour"].pings, 0)

    def test_a_tool_use_wait_nobody_came_back_to_sends_trailing_pings(self):
        log = Log()
        log.request("t", "t1", 0, 3)
        log.pings("t", "t1", 0, 11)
        log.request("h", "h1", 0, 3, one_hour=True)
        log.pings("h", "h1", 0, 11)
        _, results = run(log)
        self.assertEqual(results["recorded"].trailing, 22)
        self.assertEqual(results["current"].trailing, 11)
        self.assertEqual(results["current"].trailing_by_kind["tool_use"], 11)
        self.assertEqual(results["one-hour"].trailing, 12, "one more for the hour")


class Prior(unittest.TestCase):
    def test_laplace_rate_against_the_threshold(self):
        settings = replay.Settings(prior_min=2, prior_threshold=0.25)
        prior = replay.Prior(back=0, gone=1)
        self.assertFalse(prior.suppresses(settings), "one observation is not enough")
        prior = replay.Prior(back=0, gone=3)  # 1/5
        self.assertTrue(prior.suppresses(settings))
        prior = replay.Prior(back=1, gone=3)  # 2/6
        self.assertFalse(prior.suppresses(settings))

    def test_a_wait_resolves_at_the_next_request(self):
        prior = replay.Prior(pending=100.0)
        prior.resolve(90.0)
        self.assertEqual((prior.back, prior.gone, prior.pending), (1, 0, None))
        prior.pending = 100.0
        prior.resolve(101.0)
        self.assertEqual((prior.back, prior.gone), (1, 1))


class Cli(unittest.TestCase):
    def test_it_reads_a_database_file_without_writing_it(self):
        log = Log()
        log.request("t", "t1", 0, 3)
        log.pings("t", "t1", 0, 2)
        log.request("t", "t2", 700, 5)
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "stats.db"
            log.db(str(path)).close()
            before = path.read_bytes()
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(replay.main([str(path), "--json"]), 0)
            self.assertEqual(path.read_bytes(), before)
        report = json.loads(out.getvalue())
        self.assertEqual(report["current"]["pings"], 2)
        self.assertEqual(report["current"]["bridged"], 1)


if __name__ == "__main__":
    unittest.main()
