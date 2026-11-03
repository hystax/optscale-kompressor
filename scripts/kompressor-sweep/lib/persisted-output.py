"""Exit successfully if a Claude tool result was offloaded to disk."""
import json
import sys

for line in open(sys.argv[1], encoding="utf-8", errors="replace"):
    try:
        event = json.loads(line)
    except ValueError:
        continue
    if not isinstance(event, dict) or event.get("type") != "user":
        continue
    for block in event.get("message", {}).get("content", []):
        if not isinstance(block, dict) or block.get("type") != "tool_result":
            continue
        content = block.get("content", "")
        if isinstance(content, list):
            content = "\n".join(
                item.get("text", "") for item in content
                if isinstance(item, dict) and item.get("type") == "text"
            )
        if isinstance(content, str) and content.lstrip().startswith("<persisted-output>"):
            sys.exit(0)
sys.exit(1)
