"""Check Claude stream-json completion before scoring an answer."""
import json
import sys

result = None
api_error = False
with open(sys.argv[1], encoding="utf-8", errors="replace") as log:
    for line in log:
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if not isinstance(event, dict):
            continue
        api_error |= bool(event.get("is_api_error_message"))
        if event.get("type") == "result":
            result = event

status = int(sys.argv[2])
if (
    status
    or result is None
    or api_error
    or result.get("is_error")
    or result.get("terminal_reason") == "api_error"
):
    if status:
        print(f"  Claude process exit status: {status}")
    if result is None:
        print("  Claude produced no terminal result.")
    else:
        detail = result.get("result", result.get("subtype", "unknown"))
        print(f"  Claude session error: {str(detail)[:1000]}")
    sys.exit(1)
