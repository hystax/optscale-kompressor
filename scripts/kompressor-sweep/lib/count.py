"""How many tool results the agent actually received, and how much text they carried.

Prints four numbers: results, chars, code-mode cells, results codex truncated.

The last two say why a run came back light. Codex 0.166 runs every command
through its code-mode `exec` tool, and it cuts each cell's output to about 40 KB
("Warning: truncated output (original token count: N)") before the request
leaves for the proxy. A run that did read every file can then land under the
byte threshold, and it needs a different warning from an agent that read
nothing.
"""
import json
import re
import sys

TRUNCATED = re.compile(r"truncated output \(original token count: \d+\)")

total = calls = cells = truncated = 0
for line in open(sys.argv[1]):
    payload = json.loads(line).get("payload") or {}
    kind = payload.get("type")
    if kind == "custom_tool_call" and payload.get("name") == "exec":
        cells += 1
    if kind in ("function_call_output", "custom_tool_call_output"):
        body = payload.get("output")
        if isinstance(body, dict):
            body = body.get("content") or json.dumps(body)
        body = str(body)
        total += len(body)
        calls += 1
        if TRUNCATED.search(body):
            truncated += 1
print(f"{calls} {total} {cells} {truncated}")
