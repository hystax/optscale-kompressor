#!/usr/bin/env python3
"""Offline regressions: python3 -m unittest discover -s scripts/harbor -p 'test_*.py'."""
import copy
from collections import defaultdict
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("cache_diff", Path(__file__).with_name("cache-loss-diff.py"))
cache = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cache)
bench = cache.bench()


def body(text="original", system="stable", **extra):
    return {"messages": [{"role": "user", "content": text}], "system": system, **extra}


def classify(a, b, ia=None, ib=None):
    return cache.classify_change(json.dumps(a), json.dumps(b),
                                 json.dumps(ia) if ia is not None else None,
                                 json.dumps(ib) if ib is not None else None)[0]


class AttributionTests(unittest.TestCase):
    def test_moving_cache_marker_is_not_content_damage(self):
        a, b = body(), body()
        a["messages"][0]["content"] = [{"type": "text", "text": "original",
                                         "cache_control": {"type": "ephemeral"}}]
        b["messages"].append({"role": "assistant", "content": "next"})
        self.assertEqual(classify(a, b, a, b), "METADATA")

    def test_client_system_change_is_not_proxy_owned(self):
        a, b = body(), body(system="new client instruction")
        self.assertEqual(classify(a, b, a, b), "CLIENT")
        self.assertEqual(classify(a, b), "UNKNOWN")

    def test_client_message_edit_and_proxy_promotion_are_distinct(self):
        a, b = body("ref:k_example"), body("restored fragment")
        original = body("restored fragment")
        self.assertEqual(classify(a, b, original, original), "PROXY")
        self.assertEqual(classify(a, b, a, b), "CLIENT")
        self.assertEqual(classify(b, a, original, original), "PROXY")

    def test_client_change_does_not_hide_proxy_change(self):
        a, b = body("ref:k_example"), body("restored", system="client changed")
        self.assertEqual(classify(a, b, body("restored"), b), "PROXY")

    def test_missing_or_mixed_ingress_does_not_prove_ownership(self):
        a, b = body("old summary"), body("new summary")
        self.assertEqual(classify(a, b, body("old raw"), body("new raw")), "UNKNOWN")
        self.assertEqual(classify(a, b, {"messages": []}, {"messages": []}), "UNKNOWN")

    def test_own_tool_change_and_history_truncation(self):
        a = body(tools=[{"name": cache.OUR_TOOL, "description": "a"}])
        b = copy.deepcopy(a)
        b["tools"][0]["description"] = "b"
        self.assertEqual(classify(a, b), "PROXY")
        self.assertEqual(classify(body(), {"messages": [], "system": "stable"}), "UNKNOWN")

    def test_tool_arguments_are_not_normalized_as_cache_metadata(self):
        a = body()
        a["messages"][0]["content"] = [{"type": "tool_use", "name": "write",
                                         "input": {"cache_control": "old"}}]
        b = copy.deepcopy(a)
        b["messages"][0]["content"][0]["input"]["cache_control"] = "new"
        self.assertEqual(classify(a, b, a, b), "CLIENT")

    def test_agent_tools_and_string_input(self):
        a, b = body(tools=[]), body(tools=[{"name": "new_client_tool"}])
        self.assertEqual(classify(a, b, a, b), "CLIENT")
        a, b = {"input": "old"}, {"input": "new"}
        self.assertEqual(classify(a, b, a, b), "CLIENT")
        self.assertEqual(classify(a, a), "PROVIDER_CANDIDATE")


class AccountingTests(unittest.TestCase):
    def test_anthropic_cache_warming_does_not_start_new_session(self):
        rows = [dict(conversation="a", time_ms=1, prompt_tokens=1000,
                     cache_read_tokens=0, cache_write_tokens=0, tokens_in=100),
                dict(conversation="a", time_ms=2, prompt_tokens=2,
                     cache_read_tokens=1000, cache_write_tokens=100, tokens_in=200)]
        self.assertEqual(bench.billed_tokens(rows, "tokens_in", "anthropic"), (210, 300, 200))
        # The same remainder drop with reliable message counts is also continuous.
        rows[0]["messages_total"], rows[1]["messages_total"] = 5, 7
        self.assertEqual(bench.billed_tokens(rows, "tokens_in"), (210, 300, 200))
        rows[1]["messages_total"] = 2
        self.assertEqual(bench.billed_tokens(rows, "tokens_in", "anthropic"), (300, 300, 300))

    def test_discarded_rewrite_has_no_weighted_or_unique_savings(self):
        rows = [dict(conversation="a", time_ms=1, prompt_tokens=1000,
                     tokens_in=1000, tokens_out=500, outcome="discarded")]
        self.assertEqual(bench.billed_tokens(rows, "tokens_in"),
                         bench.billed_tokens(rows, "tokens_out"))

    def test_loss_retains_exact_pair_when_token_counts_repeat(self):
        rows = [dict(conversation="a", request_id=str(i), time_ms=i * 1000,
                     cache_read_tokens=n, retrieve_rounds=0)
                for i, n in enumerate([100, 50, 100, 50])]
        self.assertEqual([(x['before_request_id'], x['request_id'])
                         for x in bench.cache_losses(rows)], [("0", "1"), ("2", "3")])

    def test_injection_overhead_is_not_reported_as_saved(self):
        row = defaultdict(lambda: None, tokens_in=1000, tokens_out=400,
                          prompt_tokens=100, completion_tokens=10, status=200,
                          retrieve_rounds=1, retrieve_tokens_injected=353)
        events = [dict(found=1, expired=0, ranged=0, withheld=0,
                       returned_tokens=300)]
        result = bench.efficiency([row], events, provider="anthropic")
        self.assertEqual(result["saved_net"], 300)  # legacy field preserved
        self.assertEqual(result["saved_after_injection"], 247)

    def test_truncated_stream_is_not_reported_as_an_unexplained_refusal(self):
        row = defaultdict(lambda: None, status=200, stream=1, model="test-model",
                          error="upstream stream ended early")
        result = bench.efficiency([row], [], provider="anthropic")
        self.assertEqual(result["requests_failed"], 1)
        self.assertEqual(result["requests_answered_nothing"], 0)
        row["error"] = None
        self.assertEqual(bench.efficiency([row], [], provider="anthropic")
                         ["requests_answered_nothing"], 1)

    def test_binary_identity_tracks_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "binary"
            path.write_bytes(b"abc")
            self.assertEqual(bench.file_sha256(path),
                             "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
            original = bench.file_sha256(path)
            path.write_bytes(b"abcd")
            self.assertNotEqual(bench.file_sha256(path), original)
            self.assertIsNone(bench.file_sha256(Path(directory) / "missing"))


if __name__ == "__main__":
    unittest.main()
