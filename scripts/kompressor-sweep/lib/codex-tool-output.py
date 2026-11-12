"""Did codex hand the model a tool result containing NEEDLE?

    codex-tool-output.py <session.jsonl> <needle>

Exits 0 when some tool result in the session contains the needle, 1 when none
does. Prints one line saying which, and how codex had cut the read if it did.

The session file is codex's own record, written before the request leaves for
the proxy, so a needle missing here was dropped by codex and never reached the
engine. That happens when the model asks for less output than the file holds:
in code mode it calls `exec_command({..., max_output_tokens: 1000})` and codex
keeps the head and tail of a 9,122-token read, measured 2026-10-30.
"""
import json
import re
import sys

path, needle = sys.argv[1], sys.argv[2]
outputs = []
for line in open(path, encoding="utf-8", errors="replace"):
    try:
        payload = json.loads(line).get("payload") or {}
    except ValueError:
        continue
    if payload.get("type") not in ("function_call_output", "custom_tool_call_output"):
        continue
    out = payload.get("output")
    if isinstance(out, list):
        out = "".join(item.get("text", "") for item in out if isinstance(item, dict))
    elif isinstance(out, dict):
        out = out.get("content") or json.dumps(out)
    outputs.append(out if isinstance(out, str) else json.dumps(out))

if any(needle in out for out in outputs):
    print("in codex's tool output")
    sys.exit(0)
cut = next((m for out in outputs
            for m in [re.search(r"truncated output \(original token count: (\d+)\)", out)] if m), None)
if cut:
    print(f"missing: codex truncated the read (original {int(cut.group(1)):,} tokens)")
else:
    print(f"missing from all {len(outputs)} tool output(s)")
sys.exit(1)
