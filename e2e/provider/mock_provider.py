#!/usr/bin/env python3
"""A provider that answers like OpenAI and Anthropic, and remembers what it was asked.

The point of this service is not to imitate a model. It is to be the *other
side* of the proxy, so a test can assert on the bytes kompressor actually put
on the wire rather than on what the engine says it did. Those are different
claims and only the second one is checked by the unit tests.

Three things it does that a real provider does not:

* **It records every request body.** `GET /_recorded` returns them, so a test
  can measure the compressed prompt, look for handles in it, and count how many
  times the proxy came back.
* **It scripts the retrieve loop.** A turn that carries no tool result yet is
  answered with a `optscale_kompressor_retrieve` call naming a handle taken out of the
  request; a turn that does carry one is answered normally. That is exactly the
  shape the proxy's loop exists to swallow, and it needs no coordination with
  the test.
* **It speaks SSE on demand**, because the streamed relay is a different code
  path from the buffered one and is the one that historically broke.

Standard library only, deliberately: the image needs no wheels, no index and no
network, which is most of what makes the suite hermetic.
"""

import hashlib
import hmac
import json
import os
import re
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = 8080

# A handle as the engine mints it: `k_` and twelve url-safe characters. The
# provider looks for one so the scripted tool call can ask for something that
# genuinely exists, rather than a constant the store would miss.
HANDLE = re.compile(rb"ref:(k_[A-Za-z0-9_-]{12})")

_lock = threading.Lock()
_recorded: list[dict] = []


def _record(path: str, headers, body: bytes) -> None:
    with _lock:
        _recorded.append(
            {
                "path": path,
                "bytes": len(body),
                "body": body.decode("utf-8", "replace"),
                "headers": {k.lower(): v for k, v in headers.items()},
            }
        )


# The example handle in the retrieve tool's own description. Every request
# carrying the tool carries it, so it is never a handle the proxy minted.
EXAMPLE_HANDLE = b"k_AbC123dEf456"


def _first_handle(body: bytes) -> str | None:
    for found in HANDLE.finditer(body):
        if found.group(1) != EXAMPLE_HANDLE:
            return found.group(1).decode()
    return None


def _already_answered(body: bytes) -> bool:
    """Whether this request already carries the result of a retrieve call.

    The three families spell it differently and the proxy appends whichever its
    walker produced, so all three spellings count.
    """
    return any(
        marker in body
        for marker in (b'"role":"tool"', b'"function_call_output"', b'"tool_result"')
    )


def _chat_tool_call(handle: str) -> dict:
    return {
        "id": "chatcmpl-tool",
        "object": "chat.completion",
        "choices": [
            {
                "index": 0,
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": None,
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "optscale_kompressor_retrieve",
                                "arguments": json.dumps({"ref": handle}),
                            },
                        }
                    ],
                },
            }
        ],
        "usage": {"prompt_tokens": 1000, "completion_tokens": 20},
    }


def _chat_answer(text: str = "done") -> dict:
    return {
        "id": "chatcmpl-answer",
        "object": "chat.completion",
        "choices": [
            {"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": text}}
        ],
        "usage": {"prompt_tokens": 1200, "completion_tokens": 5},
    }


def _anthropic_tool_call(handle: str, range_: dict | None = None) -> dict:
    # `range_` is what the client asked the mock to send, verbatim: the shapes
    # a real model sends are not always the schema's, and the proxy's answer
    # to a malformed one is what `a_flat_range_is_served_as_the_window_it_names`
    # checks.
    arguments = {"ref": handle} if range_ is None else {"ref": handle, "range": range_}
    return {
        "id": "msg_tool",
        "type": "message",
        "role": "assistant",
        "stop_reason": "tool_use",
        "content": [
            {"type": "tool_use", "id": "toolu_1", "name": "optscale_kompressor_retrieve", "input": arguments}
        ],
        "usage": {"input_tokens": 1000, "output_tokens": 20},
    }


def _anthropic_answer(text: str = "done") -> dict:
    return {
        "id": "msg_answer",
        "type": "message",
        "role": "assistant",
        "stop_reason": "end_turn",
        "content": [{"type": "text", "text": text}],
        "usage": {"input_tokens": 1200, "output_tokens": 5},
    }


# A request carrying this marker belongs to the keep-warm check, and is
# answered as a conversation with a cached prefix: `KEEPWARM:<stop_reason>`
# chooses how the turn ends, and the usage is large enough to arm a timer.
KEEPWARM = re.compile(rb"KEEPWARM:(tool_use|end_turn)")


def _keepwarm_answer(body: bytes) -> dict | None:
    found = KEEPWARM.search(body)
    if not found:
        return None
    compact = body.replace(b" ", b"")
    if b'"max_tokens":1,' in compact or b'"max_tokens":1}' in compact:
        # A ping: the prefix is read back, one token comes out, nothing is written.
        return {
            "id": "msg_ping", "type": "message", "role": "assistant",
            "stop_reason": "max_tokens", "content": [{"type": "text", "text": "o"}],
            "usage": {"input_tokens": 3, "cache_read_input_tokens": 4000,
                      "cache_creation_input_tokens": 0, "output_tokens": 1},
        }
    stop = found.group(1).decode()
    content = (
        [{"type": "tool_use", "id": "toolu_kw", "name": "bash", "input": {"command": "make"}}]
        if stop == "tool_use"
        else [{"type": "text", "text": "Waiting on the build."}]
    )
    return {
        "id": "msg_kw", "type": "message", "role": "assistant", "stop_reason": stop,
        "content": content,
        "usage": {"input_tokens": 3, "cache_read_input_tokens": 4000,
                  "cache_creation_input_tokens": 200, "output_tokens": 20},
    }


def _responses_tool_call(handle: str) -> dict:
    return {
        "id": "resp_tool",
        "object": "response",
        "status": "completed",
        "output": [
            {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "optscale_kompressor_retrieve",
                "arguments": json.dumps({"ref": handle}),
            }
        ],
        "usage": {"input_tokens": 1000, "output_tokens": 20},
    }


def _responses_answer(text: str = "done") -> dict:
    return {
        "id": "resp_answer",
        "object": "response",
        "status": "completed",
        "output": [
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}
        ],
        "usage": {"input_tokens": 1200, "output_tokens": 5},
    }


def _responses_stream(handle: str | None, narrate: bool = False) -> bytes:
    """A streamed Responses turn, in the event shape the real API sends.

    The reason this exists: the Responses endpoint used to answer only with a
    buffered body, so the relay's streamed path was never exercised for this
    family — and a bug that let a `optscale_kompressor_retrieve` call reach the client
    on exactly that path shipped undetected.
    """
    if handle:
        item = {
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_1",
            "name": "optscale_kompressor_retrieve",
            "arguments": "",
        }
        done = dict(item, arguments=json.dumps({"ref": handle}))
        frames = [
            ("response.created", '{"type":"response.created","response":{"id":"resp_s"}}'),
        ]
        if narrate:
            # One sentence before the call — what a reasoning model does, and
            # what used to make the relay give the whole turn to the client.
            frames.append(
                (
                    "response.output_text.delta",
                    json.dumps(
                        {
                            "type": "response.output_text.delta",
                            "output_index": 0,
                            "item_id": "msg_pre",
                            "delta": "Let me look at the omitted section.",
                        }
                    ),
                )
            )
        frames += [
            (
                "response.output_item.added",
                json.dumps(
                    {"type": "response.output_item.added", "output_index": 0, "item": item}
                ),
            ),
            (
                "response.function_call_arguments.delta",
                json.dumps(
                    {
                        "type": "response.function_call_arguments.delta",
                        "output_index": 0,
                        "item_id": "fc_1",
                        "delta": json.dumps({"ref": handle}),
                    }
                ),
            ),
            (
                "response.output_item.done",
                json.dumps(
                    {"type": "response.output_item.done", "output_index": 0, "item": done}
                ),
            ),
            (
                "response.completed",
                json.dumps(
                    {
                        "type": "response.completed",
                        "response": {
                            "id": "resp_s",
                            "usage": {"input_tokens": 1000, "output_tokens": 20},
                        },
                    }
                ),
            ),
        ]
    else:
        message = {
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "done"}],
        }
        frames = [
            ("response.created", '{"type":"response.created","response":{"id":"resp_s"}}'),
            (
                "response.output_text.delta",
                '{"type":"response.output_text.delta","output_index":0,"item_id":"msg_1","delta":"done"}',
            ),
            (
                "response.output_item.done",
                json.dumps(
                    {"type": "response.output_item.done", "output_index": 0, "item": message}
                ),
            ),
            (
                "response.completed",
                json.dumps(
                    {
                        "type": "response.completed",
                        "response": {
                            "id": "resp_s",
                            "usage": {"input_tokens": 1200, "output_tokens": 5},
                        },
                    }
                ),
            ),
        ]
    return "".join(f"event: {name}\ndata: {data}\n\n" for name, data in frames).encode()


def _chat_stream(handle: str | None) -> bytes:
    """A streamed Chat turn: a retrieve call, or an answer once one was served."""
    if handle:
        frames = [
            '{"id":"c1","choices":[{"index":0,"delta":{"role":"assistant"}}]}',
            '{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1",'
            '"type":"function","function":{"name":"optscale_kompressor_retrieve","arguments":""}}]}}]}',
            '{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":'
            '{"arguments":' + json.dumps(json.dumps({"ref": handle})) + "}}]}}]}",
            '{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}',
            '{"id":"c1","choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":20}}',
        ]
    else:
        frames = [
            '{"id":"c2","choices":[{"index":0,"delta":{"role":"assistant"}}]}',
            '{"id":"c2","choices":[{"index":0,"delta":{"content":"done"}}]}',
            '{"id":"c2","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}',
            '{"id":"c2","choices":[],"usage":{"prompt_tokens":1200,"completion_tokens":5}}',
        ]
    out = "".join(f"data: {f}\n\n" for f in frames) + "data: [DONE]\n\n"
    return out.encode()


def _anthropic_stream(handle: str | None) -> bytes:
    if handle:
        frames = [
            ("message_start", '{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":1000,"output_tokens":1}}}'),
            ("content_block_start", '{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"optscale_kompressor_retrieve","input":{}}}'),
            ("content_block_delta", '{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":' + json.dumps(json.dumps({"ref": handle})) + "}}"),
            ("content_block_stop", '{"type":"content_block_stop","index":0}'),
            ("message_delta", '{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":20}}'),
            ("message_stop", '{"type":"message_stop"}'),
        ]
    else:
        frames = [
            ("message_start", '{"type":"message_start","message":{"id":"msg_2","usage":{"input_tokens":1200,"output_tokens":1}}}'),
            ("content_block_start", '{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}'),
            ("content_block_delta", '{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}'),
            ("content_block_stop", '{"type":"content_block_stop","index":0}'),
            ("message_delta", '{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}'),
            ("message_stop", '{"type":"message_stop"}'),
        ]
    return "".join(f"event: {name}\ndata: {data}\n\n" for name, data in frames).encode()


# ---------------------------------------------------------------- cloud wires
#
# Gemini, Claude on Vertex, Claude on Bedrock and Bedrock Converse. The SigV4
# verifier below is written from the AWS specification independently of the
# proxy's Rust signer, so a signature both agree on is one AWS would accept.

AWS_ACCESS_KEY_ID = os.environ.get("E2E_AWS_ACCESS_KEY_ID", "")
AWS_SECRET_ACCESS_KEY = os.environ.get("E2E_AWS_SECRET_ACCESS_KEY", "")

# Bytes in the shape of an AWS event-stream body. Not decoded by anyone: the
# proxy must relay them untouched, which the runner checks byte for byte.
EVENTSTREAM_BYTES = bytes(range(256)) * 16


def _uri_encode(text: str, keep_slash: bool) -> str:
    out = []
    for byte in text.encode():
        char = chr(byte)
        if char.isascii() and (char.isalnum() or char in "-_.~"):
            out.append(char)
        elif char == "/" and keep_slash:
            out.append(char)
        else:
            out.append(f"%{byte:02X}")
    return "".join(out)


def _sigv4_problem(method: str, raw_path: str, headers, body: bytes) -> str | None:
    """Why this request's SigV4 signature is wrong, or None when it is right."""
    auth = headers.get("authorization") or ""
    match = re.match(
        r"AWS4-HMAC-SHA256 Credential=([^/]+)/(\d{8})/([^/]+)/([^/]+)/aws4_request, "
        r"SignedHeaders=([^,]+), Signature=([0-9a-f]{64})$",
        auth,
    )
    if not match:
        return f"not a SigV4 authorization: {auth[:40]!r}"
    key_id, date, region, service, signed, signature = match.groups()
    if key_id != AWS_ACCESS_KEY_ID:
        return f"unknown access key {key_id}"
    payload = hashlib.sha256(body).hexdigest()
    if headers.get("x-amz-content-sha256") != payload:
        return "x-amz-content-sha256 is not the hash of the body received"
    amz_date = headers.get("x-amz-date") or ""
    if not amz_date.startswith(date):
        return "x-amz-date disagrees with the credential scope"
    path, _, query = raw_path.partition("?")
    pairs = sorted(
        (_uri_encode(k, False), _uri_encode(v, False))
        for k, _, v in (p.partition("=") for p in query.split("&") if p)
    )
    names = signed.split(";")
    canonical_headers = "".join(
        f"{name}:{' '.join((headers.get(name) or '').split())}\n" for name in names
    )
    canonical = "\n".join([
        method,
        _uri_encode(path, True),
        "&".join(f"{k}={v}" for k, v in pairs),
        canonical_headers,
        signed,
        payload,
    ])
    scope = f"{date}/{region}/{service}/aws4_request"
    to_sign = "\n".join([
        "AWS4-HMAC-SHA256", amz_date, scope, hashlib.sha256(canonical.encode()).hexdigest()
    ])
    key = ("AWS4" + AWS_SECRET_ACCESS_KEY).encode()
    for part in (date, region, service, "aws4_request"):
        key = hmac.new(key, part.encode(), hashlib.sha256).digest()
    expected = hmac.new(key, to_sign.encode(), hashlib.sha256).hexdigest()
    if not hmac.compare_digest(expected, signature):
        return "signature mismatch"
    return None


def _gemini_answer(text: str = "done") -> dict:
    return {
        "candidates": [{"content": {"role": "model", "parts": [{"text": text}]},
                        "finishReason": "STOP", "index": 0}],
        "usageMetadata": {"promptTokenCount": 1500, "cachedContentTokenCount": 900,
                          "candidatesTokenCount": 7, "totalTokenCount": 1507},
    }


def _gemini_stream() -> bytes:
    chunks = [
        {"candidates": [{"content": {"role": "model", "parts": [{"text": "do"}]}, "index": 0}],
         "usageMetadata": {"promptTokenCount": 1500, "candidatesTokenCount": 1}},
        {"candidates": [{"content": {"role": "model", "parts": [{"text": "ne"}]},
                         "finishReason": "STOP", "index": 0}],
         "usageMetadata": {"promptTokenCount": 1500, "candidatesTokenCount": 2,
                           "totalTokenCount": 1502}},
    ]
    return "".join(f"data: {json.dumps(c)}\r\n\r\n" for c in chunks).encode()


def _converse_answer(text: str = "done") -> dict:
    return {
        "output": {"message": {"role": "assistant", "content": [{"text": text}]}},
        "stopReason": "end_turn",
        "usage": {"inputTokens": 1400, "outputTokens": 6, "totalTokens": 1406},
    }


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _send(self, code: int, body: bytes, content_type: str | None) -> None:
        """Answer. `content_type=None` sends no `Content-Type` header at all.

        Not a curiosity: chatgpt.com returns the Responses SSE stream without
        one, and a proxy that requires the header to recognise a stream routes
        the whole thing down its buffered path — where the retrieve loop cannot
        intercept anything. That shipped once; this is how it stays fixed.
        """
        self.send_response(code)
        if content_type is not None:
            self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802 - stdlib naming
        if self.path == "/_recorded":
            with _lock:
                payload = json.dumps(_recorded).encode()
            self._send(200, payload, "application/json")
        elif self.path == "/_health":
            self._send(200, b'{"ok":true}', "application/json")
        else:
            self._send(404, b'{"error":"no such path"}', "application/json")

    def do_POST(self) -> None:  # noqa: N802 - stdlib naming
        length = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(length)

        if self.path == "/_reset":
            with _lock:
                _recorded.clear()
            self._send(200, b'{"ok":true}', "application/json")
            return

        _record(self.path, self.headers, body)

        # A turn that already carries a tool result is the second round of the
        # retrieve loop, and gets a plain answer. Anything else gets the call —
        # but only if there is a handle to ask for, because a request with no
        # handle in it is one the proxy did not compress recoverably and a call
        # naming nothing would test nothing.
        handle = None if _already_answered(body) else _first_handle(body)
        streaming = b'"stream":true' in body.replace(b" ", b"")

        route = self.path.split("?", 1)[0]
        # A request signed with SigV4 is verified before anything is answered:
        # a wrong signature is AWS's 403, and the runner sees it as one.
        if (self.headers.get("authorization") or "").startswith("AWS4-HMAC-SHA256"):
            problem = _sigv4_problem("POST", self.path, self.headers, body)
            if problem:
                self._send(403, json.dumps({"message": problem}).encode(), "application/json")
                return

        if route.endswith(":generateContent"):
            self._send(200, json.dumps(_gemini_answer()).encode(), "application/json")
        elif route.endswith(":streamGenerateContent"):
            self._send(200, _gemini_stream(), "text/event-stream")
        elif route.endswith(":rawPredict") or route.endswith("/invoke"):
            # Claude on Vertex or Bedrock: an Anthropic body and answer, with the
            # retrieve loop scripted as for /v1/messages.
            payload = _anthropic_tool_call(handle) if handle else _anthropic_answer()
            self._send(200, json.dumps(payload).encode(), "application/json")
        elif route.endswith(":streamRawPredict"):
            self._send(200, _anthropic_stream(handle), "text/event-stream")
        elif route.endswith("/invoke-with-response-stream"):
            self._send(200, EVENTSTREAM_BYTES, "application/vnd.amazon.eventstream")
        elif route.endswith("/converse"):
            self._send(200, json.dumps(_converse_answer()).encode(), "application/json")
        elif self.path.endswith("/messages"):
            keepwarm = _keepwarm_answer(body)
            if keepwarm is not None:
                self._send(200, json.dumps(keepwarm).encode(), "application/json")
            elif streaming:
                self._send(200, _anthropic_stream(handle), "text/event-stream")
            else:
                asked = self.headers.get("x-e2e-range")
                range_ = json.loads(asked) if asked else None
                payload = _anthropic_tool_call(handle, range_) if handle else _anthropic_answer()
                self._send(200, json.dumps(payload).encode(), "application/json")
        elif self.path.endswith("/responses"):
            if streaming:
                narrate = self.headers.get("x-e2e-narrate") == "1"
                # No content type, exactly as chatgpt.com sends it.
                self._send(200, _responses_stream(handle, narrate), None)
            else:
                payload = _responses_tool_call(handle) if handle else _responses_answer()
                self._send(200, json.dumps(payload).encode(), "application/json")
        elif self.path.endswith("/chat/completions"):
            if streaming:
                self._send(200, _chat_stream(handle), "text/event-stream")
            else:
                payload = _chat_tool_call(handle) if handle else _chat_answer()
                self._send(200, json.dumps(payload).encode(), "application/json")
        else:
            self._send(404, b'{"error":"no such path"}', "application/json")

    def log_message(self, *args) -> None:  # keep the compose log readable
        pass


if __name__ == "__main__":
    print(f"mock provider on :{PORT}", file=sys.stderr, flush=True)
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
