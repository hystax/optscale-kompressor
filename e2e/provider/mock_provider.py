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

import json
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


def _first_handle(body: bytes) -> str | None:
    found = HANDLE.search(body)
    return found.group(1).decode() if found else None


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


def _anthropic_tool_call(handle: str) -> dict:
    return {
        "id": "msg_tool",
        "type": "message",
        "role": "assistant",
        "stop_reason": "tool_use",
        "content": [
            {"type": "tool_use", "id": "toolu_1", "name": "optscale_kompressor_retrieve", "input": {"ref": handle}}
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

        if self.path.endswith("/messages"):
            keepwarm = _keepwarm_answer(body)
            if keepwarm is not None:
                self._send(200, json.dumps(keepwarm).encode(), "application/json")
            elif streaming:
                self._send(200, _anthropic_stream(handle), "text/event-stream")
            else:
                payload = _anthropic_tool_call(handle) if handle else _anthropic_answer()
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
