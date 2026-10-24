"""How many tool results the agent actually received, and how much text they carried."""
import json
import sys

total = calls = 0
for line in open(sys.argv[1]):
    payload = json.loads(line).get("payload") or {}
    if payload.get("type") in ("function_call_output", "custom_tool_call_output"):
        body = payload.get("output")
        if isinstance(body, dict):
            body = body.get("content") or json.dumps(body)
        total += len(str(body))
        calls += 1
print(f"{calls} {total}")
