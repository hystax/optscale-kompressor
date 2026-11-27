# kompressor-client (Python)

A thin client for a running Kompressor (`kompressor serve`): one call per
endpoint, standard library only, Python 3.9+.

```python
from kompressor_client import Kompressor

k = Kompressor("http://127.0.0.1:8817")          # or $KOMPRESSOR_URL; api_key= or $KOMPRESSOR_API_KEY
out = k.compress(messages, model="claude-opus-5-5")
messages, stats = out["messages"], out["stats"]

messages = k.compress_or_passthrough(messages)  # never raises: the input on any error
k.compress_raw(tool_output, content_type="log")
k.retrieve("k_3f9a…", range={"lines": {"start": 1, "end": 80}})
```

Errors raise `KompressorError` with `status`, `code` and `request_id`. Field
names are the server's own (`frozen_message_count`, `content_type`, …): see
README "Surfaces" and DESIGN §10.1 for what each one does.

Tests: `python3 -m unittest discover -s tests` (an in-process fake server; no
network).
