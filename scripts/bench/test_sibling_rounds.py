"""Offline checks for sibling-rounds.py on synthetic captures.

    python3 -m unittest discover -s scripts/bench -p 'test_*.py'
"""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import sqlite3
import tempfile
from types import SimpleNamespace
import unittest

spec = importlib.util.spec_from_file_location(
    "sibling_rounds", Path(__file__).with_name("sibling-rounds.py"))
sibling_rounds = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sibling_rounds)

H = [f"k_{c * 12}" for c in "ABCDEFGH"]  # eight well-formed handles
TOOL = "optscale_kompressor_retrieve"


def chat_base(request_id: str) -> dict:
    """A chat submission with two tool outputs: H0..H2 in one, H3 in the other."""
    return {
        "model": "gpt-5",
        "messages": [
            {"role": "user", "content": f"question {request_id}"},
            {"role": "tool", "tool_call_id": "c0",
             "content": "\n".join(f"[... 9 matches omitted [ref:{h}] in a{i}.rs]"
                                  for i, h in enumerate(H[:3]))},
            {"role": "tool", "tool_call_id": "c1",
             "content": f"[... 40 log lines omitted [ref:{H[3]}]]"},
        ],
        "tools": [{"type": "function", "function": {
            "name": TOOL, "description": "e.g. ref:k_AbC123dEf456"}}],
    }


def chat_round(asked: list[str], n: int) -> list[dict]:
    calls = [{"id": f"r{n}_{i}", "type": "function", "function": {
        "name": TOOL, "arguments": json.dumps({"ref": h})}} for i, h in enumerate(asked)]
    out = [{"role": "assistant", "content": None, "tool_calls": calls}]
    out += [{"role": "tool", "tool_call_id": c["id"], "content": "{\"found\":true}"} for c in calls]
    return out


def capture_lines(request_id: str, rounds: list[list[str]]) -> list[dict]:
    base = chat_base(request_id)
    lines = [
        {"request_id": request_id, "dir": "client->proxy", "body": json.dumps(base)},
        {"request_id": request_id, "dir": "proxy->upstream", "body": json.dumps(base)},
    ]
    body = json.loads(json.dumps(base))
    for n, asked in enumerate(rounds, 1):
        body["messages"] += chat_round(asked, n)
        lines.append({"request_id": request_id, "dir": "retrieve-round", "round": n,
                      "body": json.dumps(body)})
    return lines


def args(capture: Path, **extra) -> SimpleNamespace:
    defaults = dict(capture=str(capture), stats=None, refs=None, gap_ms=1500, budget=16384,
                    output_cost_tokens=500.0, default_sibling_tokens=500, json=True)
    defaults.update(extra)
    return SimpleNamespace(**defaults)


class SiblingRoundTests(unittest.TestCase):
    def write(self, directory: Path, lines: list[dict]) -> Path:
        path = directory / "kompressor-bodies.jsonl"
        path.write_text("\n".join(json.dumps(line) for line in lines) + "\n")
        return path

    def test_a_later_round_asking_a_sibling_is_covered_and_one_asking_elsewhere_is_not(self):
        with tempfile.TemporaryDirectory() as directory:
            capture = self.write(Path(directory), [
                # Round two asks H1, which shares H0's output: covered.
                *capture_lines("sib", [[H[0]], [H[1]]]),
                # Round two asks H3, from the other output: not covered.
                *capture_lines("far", [[H[0]], [H[3]]]),
                # One round only: nothing to measure.
                *capture_lines("one", [[H[2]]]),
                # A repeat ask is not a sibling case.
                *capture_lines("rep", [[H[0]], [H[0]]]),
            ])
            result = sibling_rounds.measure(args(capture))
        self.assertEqual(result["round_source"], {"capture": 4})
        self.assertEqual(result["later_rounds"], 3)
        self.assertEqual(result["covered_all"], 1)
        self.assertAlmostEqual(result["share"], 1 / 3)
        self.assertEqual(result["verdict"], "do not build")
        openai = result["providers"]["openai"]
        self.assertEqual(openai["repeat_only"], 1)
        self.assertEqual(openai["multi_round_requests"], 3)
        # The covered round saved one round at r * prompt + output.
        prompt = len(json.dumps(chat_base("sib"))) / 4
        self.assertAlmostEqual(openai["saved"], round(0.1 * prompt + 500, 1), places=0)
        # Every answered round attaches the other two handles of its output
        # once per request: H0's siblings in "sib", "far", "rep"; H2's in "one".
        self.assertEqual(openai["attached"], 8)
        self.assertGreater(openai["cost"], 0)

    def test_the_budget_bounds_what_would_be_attached(self):
        with tempfile.TemporaryDirectory() as directory:
            capture = self.write(Path(directory), capture_lines("sib", [[H[0]], [H[1]]]))
            tight = sibling_rounds.measure(args(capture, budget=600))
        self.assertEqual(tight["providers"]["openai"]["attached"], 1)

    def test_rounds_are_read_from_the_stats_database_when_the_capture_has_none(self):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            base = chat_base("req")
            capture = self.write(directory, [
                {"request_id": "req", "dir": "proxy->upstream", "body": json.dumps(base)},
            ])
            stats = directory / "stats.db"
            con = sqlite3.connect(stats)
            con.execute("CREATE TABLE retrieval_events (id INTEGER PRIMARY KEY, time_ms INTEGER, "
                        "request_id TEXT, surface TEXT, ref TEXT, fragment_tokens INTEGER)")
            con.execute("CREATE TABLE proxy_requests (request_id TEXT, prompt_tokens INTEGER, "
                        "cache_read_tokens INTEGER, cache_write_tokens INTEGER, "
                        "retrieve_rounds INTEGER, api_family TEXT)")
            # Two asks 10 ms apart are one round; 4 s later is the next round.
            for i, (at, ref) in enumerate([(1000, H[0]), (1010, H[3]), (5000, H[1])]):
                con.execute("INSERT INTO retrieval_events VALUES (?, ?, 'req', 'proxy', ?, 700)",
                            (i, at, ref))
            con.execute("INSERT INTO proxy_requests VALUES ('req', 90000, 0, 0, 2, 'openai_chat')")
            con.commit()
            con.close()
            result = sibling_rounds.measure(args(capture, stats=str(stats)))
        self.assertEqual(result["round_source"], {"stats": 1})
        self.assertEqual(result["later_rounds"], 1)
        self.assertEqual(result["covered_all"], 1)
        openai = result["providers"]["openai"]
        self.assertAlmostEqual(openai["saved"], 0.1 * 30000 + 500)
        # H0's siblings H1, H2 at 700 tokens each from retrieval_events for H1
        # and the default for H2, written once and re-read by one later round.
        self.assertEqual(openai["attached"], 2)
        self.assertAlmostEqual(openai["cost"], (700 + 500) * (1.0 + 0.1 * 1))

    def test_responses_and_anthropic_rounds_are_read_too(self):
        anthropic_appended = [
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": TOOL, "input": {"ref": H[0]}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1"}]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "t2", "name": TOOL, "input": {"ref": H[1]}},
                {"type": "tool_use", "id": "t3", "name": TOOL, "input": {"ref": H[2]}}]},
        ]
        self.assertEqual(sibling_rounds.rounds_in(anthropic_appended), [[H[0]], [H[1], H[2]]])
        responses_appended = [
            {"type": "function_call", "name": TOOL, "arguments": json.dumps({"ref": H[0]})},
            {"type": "function_call_output", "call_id": "x", "output": "{}"},
            {"type": "function_call", "name": TOOL, "arguments": json.dumps({"ref": H[1]})},
            {"type": "function_call", "name": TOOL, "arguments": json.dumps({"ref": H[2]})},
        ]
        self.assertEqual(sibling_rounds.rounds_in(responses_appended), [[H[0]], [H[1], H[2]]])

    def test_the_text_report_states_the_verdict(self):
        with tempfile.TemporaryDirectory() as directory:
            capture = self.write(Path(directory), capture_lines("sib", [[H[0]], [H[1]]]))
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                sibling_rounds.main(["--capture", str(capture)])
        self.assertIn("100.0%", out.getvalue())
        self.assertIn("build", out.getvalue())


if __name__ == "__main__":
    unittest.main()
