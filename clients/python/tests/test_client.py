"""The Python client against an in-process stand-in for the server.

Run from clients/python: ``python3 -m unittest discover -s tests``.
"""

from __future__ import annotations

import json
import io
import http.client
import urllib.error
from unittest.mock import patch, MagicMock
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from kompressor_client import Kompressor, KompressorError  # noqa: E402

# The fields the server's request structs accept. Both refuse unknown
# fields, so a client that drifts from these gets a 400 in production.
COMPRESS_FIELDS = {"messages", "model", "provider", "request_id", "options", "frozen_message_count", "context"}
RAW_FIELDS = {"content", "content_type", "options", "request_id", "context", "source_tool", "file_path"}
RETRIEVE_FIELDS = {"ref", "range", "request_id"}
DETECT_FIELDS = {"content", "role"}


class Fake(BaseHTTPRequestHandler):
    seen: list = []

    def log_message(self, *args):  # quiet
        pass

    def _reply(self, status: int, body: dict) -> None:
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path == "/healthz":
            self._reply(200, {"status": "ok"})
        else:
            self._reply(404, {"error": {"code": "not_found", "message": "no", "request_id": "r0"}})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        Fake.seen.append((self.path, {k.lower(): v for k, v in self.headers.items()}, body))
        if self.headers.get("authorization") == "Bearer wrong":
            return self._reply(401, {"error": {"code": "unauthorized", "message": "bad key", "request_id": "r1"}})
        allowed = {
            "/v1/compress": COMPRESS_FIELDS,
            "/v1/compress/raw": RAW_FIELDS,
            "/v1/retrieve": RETRIEVE_FIELDS,
            "/v1/detect": DETECT_FIELDS,
        }[self.path]
        unknown = set(body) - allowed
        if unknown:
            return self._reply(400, {"error": {"code": "bad_json", "message": f"unknown field {unknown}", "request_id": "r2"}})
        if self.path == "/v1/compress":
            self._reply(200, {"messages": [{"role": "user", "content": "short"}], "stats": {"ratio": 0.2}})
        elif self.path == "/v1/compress/raw":
            self._reply(200, {"content": "short", "stats": {"ratio": 0.3}})
        elif self.path == "/v1/retrieve":
            self._reply(200, {"found": False, "hint": "regenerate it"})
        else:
            self._reply(200, {"content_type": "log", "strategy": "log"})


class ClientTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), Fake)
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()
        cls.url = f"http://127.0.0.1:{cls.server.server_address[1]}"

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()

    def setUp(self):
        Fake.seen.clear()

    def test_compress_sends_only_the_fields_given_and_the_key(self):
        k = Kompressor(self.url, api_key="sk-test", tenant="acme")
        out = k.compress([{"role": "user", "content": "x" * 100}], model="m", frozen_message_count=0)
        self.assertEqual(out["stats"]["ratio"], 0.2)
        path, headers, body = Fake.seen[0]
        self.assertEqual(path, "/v1/compress")
        self.assertEqual(headers["authorization"], "Bearer sk-test")
        self.assertEqual(headers["x-kompressor-tenant"], "acme")
        self.assertEqual(set(body), {"messages", "model", "frozen_message_count"})

    def test_every_field_each_method_can_send_is_one_the_server_accepts(self):
        k = Kompressor(self.url)
        k.compress([], model="m", provider="p", frozen_message_count=1, context="c", options={}, request_id="r")
        k.compress_raw("x", content_type="log", options={}, request_id="r", context="c", source_tool="t", file_path="f")
        k.retrieve("k_abc", range={"lines": {"start": 1, "end": 2}}, request_id="r")
        k.detect("x", role="tool")
        for path, _, body in Fake.seen:
            allowed = {"/v1/compress": COMPRESS_FIELDS, "/v1/compress/raw": RAW_FIELDS,
                       "/v1/retrieve": RETRIEVE_FIELDS, "/v1/detect": DETECT_FIELDS}[path]
            self.assertTrue(set(body) <= allowed, (path, body))
            self.assertEqual(set(body), allowed, f"{path} sends every field it documents")

    def test_a_server_error_carries_status_code_and_request_id(self):
        k = Kompressor(self.url, api_key="wrong")
        with self.assertRaises(KompressorError) as caught:
            k.compress([])
        self.assertEqual(caught.exception.status, 401)
        self.assertEqual(caught.exception.code, "unauthorized")
        self.assertEqual(caught.exception.request_id, "r1")

    def test_an_unreachable_server_is_an_error_and_fail_open_returns_the_input(self):
        k = Kompressor("http://127.0.0.1:9", timeout=0.5)
        with self.assertRaises(KompressorError) as caught:
            k.compress([])
        self.assertEqual(caught.exception.status, 0)
        messages = [{"role": "user", "content": "keep me"}]
        self.assertIs(k.compress_or_passthrough(messages), messages)
        self.assertFalse(k.health())

    def test_malformed_and_truncated_responses_fail_open(self):
        messages = [{"role": "user", "content": "keep me"}]
        for body in [b"null", b"[]", b"{}", b'{"messages":null}', b"not json"]:
            response = MagicMock()
            response.__enter__.return_value.read.return_value = body
            with patch("urllib.request.urlopen", return_value=response):
                self.assertIs(Kompressor(self.url).compress_or_passthrough(messages), messages)
        response = MagicMock()
        response.__enter__.return_value.read.side_effect = http.client.IncompleteRead(b"partial", 50)
        with patch("urllib.request.urlopen", return_value=response):
            self.assertIs(Kompressor(self.url).compress_or_passthrough(messages), messages)
        for body in [b"null", b"[]", b'{"error":"bad gateway"}']:
            error = urllib.error.HTTPError(self.url, 502, "gateway", {}, io.BytesIO(body))
            with patch("urllib.request.urlopen", side_effect=error):
                self.assertIs(Kompressor(self.url).compress_or_passthrough(messages), messages)

    def test_other_calls_and_health(self):
        k = Kompressor(self.url)
        self.assertEqual(k.compress_raw("x" * 50)["content"], "short")
        self.assertFalse(k.retrieve("k_abc")["found"])
        self.assertEqual(k.detect("x")["strategy"], "log")
        self.assertTrue(k.health())

    def test_the_key_never_appears_in_repr(self):
        self.assertNotIn("sk-secret", repr(Kompressor(self.url, api_key="sk-secret")))


if __name__ == "__main__":
    unittest.main()
