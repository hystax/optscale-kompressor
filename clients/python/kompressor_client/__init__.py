"""A thin client for the OptScale Kompressor HTTP API.

Standard library only (``urllib``): Python 3.9 or newer, no dependencies.
Every method is one POST to ``/v1/...`` and returns the decoded JSON the
server sent; the request field names are the server's own
(``CompressRequest`` / ``CompressRawRequest``, which refuse unknown fields).

    from kompressor_client import Kompressor

    k = Kompressor("http://127.0.0.1:8817", api_key=None)
    out = k.compress(messages, model="claude-opus-5-5")
    messages, stats = out["messages"], out["stats"]

See docs/design/HEADROOM-PORT.md, Tier 6.
"""

from __future__ import annotations

import json
import http.client
import os
import urllib.error
import urllib.request
from typing import Any, Dict, List, Optional

__all__ = ["Kompressor", "KompressorError", "DEFAULT_BASE_URL"]
__version__ = "0.1.0"

DEFAULT_BASE_URL = "http://127.0.0.1:8817"
Json = Dict[str, Any]


class KompressorError(Exception):
    """The server answered with an error, or could not be reached.

    ``status`` is the HTTP status (0 when no answer arrived), ``code`` the
    server's error code (``"unauthorized"``, ``"invalid"``, ...) and
    ``request_id`` the id to look the request up by in the server's log.
    """

    def __init__(
        self,
        message: str,
        status: int = 0,
        code: Optional[str] = None,
        request_id: Optional[str] = None,
    ) -> None:
        super().__init__(message)
        self.status = status
        self.code = code
        self.request_id = request_id


def _drop_none(fields: Json) -> Json:
    return {key: value for key, value in fields.items() if value is not None}


class Kompressor:
    """One Kompressor server.

    ``base_url`` defaults to ``$KOMPRESSOR_URL`` or ``http://127.0.0.1:8817``;
    ``api_key`` to ``$KOMPRESSOR_API_KEY``, sent as ``Authorization: Bearer``.
    """

    def __init__(
        self,
        base_url: Optional[str] = None,
        api_key: Optional[str] = None,
        timeout: float = 30.0,
        tenant: Optional[str] = None,
    ) -> None:
        self.base_url = (base_url or os.environ.get("KOMPRESSOR_URL") or DEFAULT_BASE_URL).rstrip("/")
        self.api_key = api_key if api_key is not None else os.environ.get("KOMPRESSOR_API_KEY")
        self.timeout = timeout
        self.tenant = tenant

    def __repr__(self) -> str:  # the key never appears in a repr or a log line
        return f"Kompressor(base_url={self.base_url!r}, api_key={'***' if self.api_key else None})"

    # ------------------------------------------------------------------ calls
    def compress(
        self,
        messages: List[Json],
        *,
        model: Optional[str] = None,
        provider: Optional[str] = None,
        frozen_message_count: Optional[int] = None,
        context: Optional[str] = None,
        options: Optional[Json] = None,
        request_id: Optional[str] = None,
    ) -> Json:
        """Compress a conversation in the caller's own message shape.

        Returns ``{"messages": [...], "stats": {...}}``. Messages
        ``[0, frozen_message_count)`` come back byte-identical.
        """
        return self._post(
            "/v1/compress",
            _drop_none(
                {
                    "messages": messages,
                    "model": model,
                    "provider": provider,
                    "frozen_message_count": frozen_message_count,
                    "context": context,
                    "options": options,
                    "request_id": request_id,
                }
            ),
        )

    def compress_or_passthrough(self, messages: List[Json], **kwargs: Any) -> List[Json]:
        """``compress`` that never fails: on any error the messages come back
        as they went in. The library form of the proxy's fail-open rule."""
        try:
            result = self.compress(messages, **kwargs).get("messages")
            return result if isinstance(result, list) else messages
        except KompressorError:
            return messages

    def compress_raw(
        self,
        content: str,
        *,
        content_type: Optional[str] = None,
        options: Optional[Json] = None,
        request_id: Optional[str] = None,
        context: Optional[str] = None,
        source_tool: Optional[str] = None,
        file_path: Optional[str] = None,
    ) -> Json:
        """Compress one payload. Returns ``{"content": str, "stats": {...}}``."""
        return self._post(
            "/v1/compress/raw",
            _drop_none(
                {
                    "content": content,
                    "content_type": content_type,
                    "options": options,
                    "request_id": request_id,
                    "context": context,
                    "source_tool": source_tool,
                    "file_path": file_path,
                }
            ),
        )

    def retrieve(self, ref: str, *, range: Optional[Json] = None, request_id: Optional[str] = None) -> Json:
        """Fetch back what a marker's handle elided. ``range`` is one of
        ``{"lines": {...}}``, ``{"chars": {...}}``, ``{"json_pointer": {...}}``,
        ``{"grep": {...}}``. A miss is ``{"found": false, "hint": ...}``, not
        an exception."""
        return self._post("/v1/retrieve", _drop_none({"ref": ref, "range": range, "request_id": request_id}))

    def detect(self, content: str, *, role: Optional[str] = None) -> Json:
        """What the engine would decide about a payload, without compressing it."""
        return self._post("/v1/detect", _drop_none({"content": content, "role": role}))

    def health(self) -> bool:
        """Whether the server answers ``/healthz``."""
        try:
            return self._request("GET", "/healthz", None).get("status") == "ok"
        except KompressorError:
            return False

    # -------------------------------------------------------------- transport
    def _post(self, path: str, body: Json) -> Json:
        return self._request("POST", path, body)

    def _request(self, method: str, path: str, body: Optional[Json]) -> Json:
        data = None if body is None else json.dumps(body).encode("utf-8")
        request = urllib.request.Request(self.base_url + path, data=data, method=method)
        request.add_header("accept", "application/json")
        if data is not None:
            request.add_header("content-type", "application/json")
        if self.api_key:
            request.add_header("authorization", f"Bearer {self.api_key}")
        if self.tenant:
            request.add_header("x-kompressor-tenant", self.tenant)
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                result = json.loads(response.read())
                if not isinstance(result, dict):
                    raise ValueError("expected a JSON object")
                return result
        except urllib.error.HTTPError as error:
            raise _from_http_error(error) from None
        except (urllib.error.URLError, OSError, ValueError, http.client.HTTPException) as error:
            raise KompressorError(f"kompressor at {self.base_url} could not be reached: {error}") from None


def _from_http_error(error: urllib.error.HTTPError) -> KompressorError:
    try:
        body = json.loads(error.read() or b"{}")
        envelope = body.get("error") if isinstance(body, dict) else None
        if not isinstance(envelope, dict):
            envelope = {}
    except (ValueError, OSError, http.client.HTTPException):
        envelope = {}
    return KompressorError(
        envelope.get("message") or f"HTTP {error.code}",
        status=error.code,
        code=envelope.get("code"),
        request_id=envelope.get("request_id"),
    )
