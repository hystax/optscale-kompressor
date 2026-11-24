"""Describe the actual tool results in a Claude stream-json session."""
import json
import sys

from replay import analyse

calls = {}
results = []
models = set()
for line in open(sys.argv[1], encoding="utf-8", errors="replace"):
    try:
        event = json.loads(line)
    except ValueError:
        continue
    if not isinstance(event, dict):
        continue
    message = event.get("message", {})
    if event.get("type") == "assistant":
        model = message.get("model")
        if model and model != "<synthetic>":
            models.add(model)
        for block in message.get("content", []):
            if block.get("type") == "tool_use":
                args = block.get("input", {})
                label = args.get("command") or args.get("file_path") or block.get("name", "tool")
                calls[block["id"]] = " ".join(str(label).split())
    elif event.get("type") == "user":
        for block in message.get("content", []):
            if block.get("type") != "tool_result":
                continue
            content = block.get("content", "")
            if isinstance(content, list):
                content = "\n".join(b.get("text", "") for b in content
                                    if isinstance(b, dict) and b.get("type") == "text")
            if not isinstance(content, str):
                content = ""
            results.append((calls.get(block.get("tool_use_id"), "tool"), len(content),
                            content.lstrip().startswith("<persisted-output>"), content))

if "--count" in sys.argv:
    print(len(results), sum(row[1] for row in results))
else:
    print(f"  model           {', '.join(sorted(models)) or 'not reported'}")
    print(f"  tool results    {len(results)} carrying {sum(row[1] for row in results):,} chars")
    print(f"  offloaded       {sum(row[2] for row in results)} result(s) sent as previews")
    if "--table" in sys.argv:
        print("\n  per-tool output (before proxy compression)")
        print(f"  {'command / tool':<64}{'chars':>10}  delivery")
        print("  " + "-" * 88)
        for label, size, offloaded, _ in results:
            print(f"  {label[:63]:<64}{size:>10,}  {'preview' if offloaded else 'inline'}")
    if "--analyse" in sys.argv:
        index = sys.argv.index("--analyse")
        binary, config = sys.argv[index + 1:index + 3]
        analyse([(label, content) for label, _, _, content in results], binary, config)
