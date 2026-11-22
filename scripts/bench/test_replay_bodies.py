"""Offline checks for the replay: python3 -m unittest discover -s scripts/bench -p 'test_*.py'.

Nothing here reaches a provider. The last test starts the binary against the
replay's own local upstream, and is skipped where no binary has been built.
"""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import tomllib
from types import SimpleNamespace
import unittest

spec = importlib.util.spec_from_file_location("replay", Path(__file__).with_name("replay-bodies.py"))
replay = importlib.util.module_from_spec(spec)
spec.loader.exec_module(replay)


class ReplayConfigTests(unittest.TestCase):
    def test_effective_config_is_redirected_without_duplicate_keys(self):
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory)
            source = out / "source.toml"
            source.write_text('''[server]
listen = "172.17.0.1:8817"
proxy_listen = "127.0.0.1:9000"
queue_timeout = "2s"
[refs]
path = "/original/refs.db"
[stats]
path = "/original/stats.db"
[proxy]
compress_timeout = "2s"
cache_keep_warm = true
[[proxy.upstreams]]
name = "anthropic"
base_url = "https://example.com"
[proxy.intercept]
listen = "127.0.0.1:9999"
[proxy.providers.anthropic]
cache_keep_warm = true
frozen_prefix = "first_seen"
[proxy.providers.anthropic.engine]
enable_code_aware = false
''')
            config = tomllib.loads(replay.write_config(out, 12345, 12346, source).read_text())
            self.assertEqual(config["server"]["listen"], "127.0.0.1:12346")
            self.assertEqual(config["server"]["proxy_listen"], "")
            self.assertEqual(config["refs"]["path"], str(out / "refs.db"))
            self.assertEqual(config["stats"]["path"], str(out / "stats.db"))
            proxy = config["proxy"]
            self.assertEqual(proxy["upstreams"][0]["base_url"], "http://127.0.0.1:12345")
            self.assertNotIn("intercept", proxy)
            self.assertFalse(proxy["cache_keep_warm"])
            self.assertFalse(proxy["providers"]["anthropic"]["cache_keep_warm"])
            self.assertFalse(proxy["providers"]["anthropic"]["engine"]["enable_code_aware"])
            self.assertEqual(proxy["providers"]["anthropic"]["frozen_prefix"], "first_seen")


    def rewritten(self, source_text):
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory)
            (out / "source.toml").write_text(source_text)
            path = replay.write_config(out, 12345, 12346, out / "source.toml")
            return out.resolve(), tomllib.loads(path.read_text())

    def test_the_packaged_configuration_is_redirected(self):
        out, config = self.rewritten(replay.PACKAGED.read_text(encoding="utf-8"))
        self.assertEqual(config["server"]["listen"], "127.0.0.1:12346")
        self.assertEqual(Path(config["stats"]["path"]).parent, out)
        self.assertNotIn("intercept", config["proxy"])
        self.assertFalse(config["proxy"]["cache_keep_warm"])
        self.assertTrue(config["proxy"]["upstreams"])
        for upstream in config["proxy"]["upstreams"]:
            self.assertEqual(upstream["base_url"], "http://127.0.0.1:12345")
            self.assertEqual(upstream.get("subscription", {}).get(
                "base_url", "http://127.0.0.1:12345"), "http://127.0.0.1:12345")

    def test_a_section_left_to_its_defaults_is_still_redirected(self):
        # Without a `[server]` the service would listen where a live one does,
        # and without `[stats]` it would write wherever the default points.
        out, config = self.rewritten('''[[proxy.upstreams]]
name = "anthropic"
path_prefix = "/v1/messages"
  base_url = "https://example.com"
[proxy.providers.anthropic]
cold_recompact = "predicted"''')
        self.assertEqual(config["server"]["listen"], "127.0.0.1:12346")
        self.assertEqual(Path(config["refs"]["path"]), out / "refs.db")
        self.assertEqual(Path(config["stats"]["path"]), out / "stats.db")
        self.assertEqual(config["maintenance"]["backup_dir"], "")
        proxy = config["proxy"]
        self.assertFalse(proxy["cache_keep_warm"])
        self.assertFalse(proxy["providers"]["anthropic"]["cache_keep_warm"])
        self.assertEqual(proxy["providers"]["anthropic"]["cold_recompact"], "predicted")
        self.assertEqual(proxy["upstreams"][0]["base_url"], "http://127.0.0.1:12345")
        self.assertEqual(proxy["upstreams"][0]["path_prefix"], "/v1/messages")

    def test_a_spelling_the_rewrite_does_not_read_is_refused_not_replayed(self):
        # Each of these says, in valid TOML the line-by-line rewrite does not
        # recognise, something that would leave the replay.
        for escaping in (
            "[proxy]\nproviders.anthropic.cache_keep_warm = true\n",
            "[proxy]\nproviders = { anthropic = { cache_keep_warm = true } }\n",
            "[proxy]\nintercept = { listen = \"127.0.0.1:9999\" }\n",
            "[proxy]\nupstreams = [{ name = \"a\", base_url = \"https://example.com\" }]\n",
            "server.stats_listen = \"127.0.0.1:8818\"\n",
        ):
            with self.subTest(escaping), contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit) as refused:
                    self.rewritten(escaping)
                # Refused by name, or because the redirected key now appears
                # twice: either way nothing was started.
                self.assertRegex(str(refused.exception.code),
                                 "outside this replay|does not parse")


def built_binary():
    name = "kompressor.exe" if os.name == "nt" else "kompressor"
    built = [path for path in (replay.REPO / "target" / profile / name
                               for profile in ("release", "debug")) if path.exists()]
    return max(built, key=lambda path: path.stat().st_mtime) if built else None


class ReplayEndToEndTests(unittest.TestCase):
    def test_a_capture_replays_through_the_binary_and_the_limit_keeps_its_last_pair(self):
        binary = built_binary()
        if binary is None:
            self.skipTest("no built kompressor binary under target/")
        request = lambda text: {
            "model": "claude-opus-5-5", "max_tokens": 64,
            "messages": [{"role": "user", "content": text}]}
        records = []
        for index in range(3):
            raw = json.dumps(request(f"request {index}"))
            records.append(dict(request_id=f"r{index}", dir="client->proxy", body=raw))
            # A ping's record sits between a request's two bodies in a real
            # capture; it has no body and is neither replayed nor counted.
            records.append(dict(request_id=f"r{index}.keepwarm1", dir="keep-warm", read=1))
            records.append(dict(request_id=f"r{index}", dir="proxy->upstream", body=raw + " "))
        with tempfile.TemporaryDirectory() as directory:
            capture = Path(directory) / "bodies.jsonl"
            capture.write_text("\n".join(map(json.dumps, records)))
            # An override in the environment must not reach the child: this
            # one would send it looking for a configuration that is not there.
            previous = os.environ.get("KOMPRESSOR_CONFIG")
            os.environ["KOMPRESSOR_CONFIG"] = str(Path(directory) / "absent.toml")
            try:
                summary = replay.replay(SimpleNamespace(
                    bin=str(binary), capture=str(capture), config=None, limit=2,
                    out=str(Path(directory) / "out")))
            finally:
                if previous is None:
                    del os.environ["KOMPRESSOR_CONFIG"]
                else:
                    os.environ["KOMPRESSOR_CONFIG"] = previous
            self.assertEqual((summary["requests"], summary["errors"]), (2, 0), summary)
            self.assertEqual(summary["by_path"], {"/v1/messages": 2})
            self.assertGreaterEqual(summary["upstream_calls"], 2)
            self.assertEqual(summary["panics"], 0)
            # Both selected requests' original pairs, the second included: the
            # loop used to stop before reading the last one's outbound body.
            self.assertAlmostEqual(
                summary["original_run_reduction_pct"],
                round(100 * (1 - (len(json.dumps(request("request 0"))) + 1)
                             / len(json.dumps(request("request 0")))), 3))


if __name__ == "__main__":
    unittest.main()
