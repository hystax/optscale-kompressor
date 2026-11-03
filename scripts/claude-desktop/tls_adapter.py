"""Route Claude Messages through an existing Kompressor reverse proxy.

Run mitmdump with allow_hosts=^api\\.anthropic\\.com:443$, connection_strategy=lazy
and this addon. Other hosts are tunneled; non-Messages API calls go directly
upstream. No request/response bodies or credentials are logged by this addon.
"""

from mitmproxy import http


def request(flow: http.HTTPFlow) -> None:
    request = flow.request
    if (
        request.scheme == "https"
        and request.host == "api.anthropic.com"
        and request.port == 443
        and request.method == "POST"
        and request.path.split("?", 1)[0] == "/v1/messages"
    ):
        # Kompressor expects JSON, including when the client uploads gzip.
        request.decode()
        request.scheme = "http"
        request.host = "127.0.0.1"
        request.port = 8817
        request.path = "/anthropic" + request.path
        request.host_header = "127.0.0.1:8817"


def responseheaders(flow: http.HTTPFlow) -> None:
    # Deliver SSE events as they arrive, without buffering the whole answer.
    flow.response.stream = True
