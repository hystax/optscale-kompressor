"""Describe the actual tool results in a Claude stream-json session."""
import json
import sys
import subprocess
import tempfile
from pathlib import Path

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
        print("\n  per-payload compression (local replay of actual tool output, not live savings)")
        print(f"  {'command / tool':<44}{'detected':<15}{'kind':<15}{'chars in':>10}{'out':>10}{'saved':>9}")
        total_in = total_out = 0
        with tempfile.TemporaryDirectory(prefix="claude-payloads-") as directory:
            payload = Path(directory) / "payload.txt"
            for label, size, _, content in results:
                payload.write_text(content)
                outputs = []
                for command in ("detect", "compress"):
                    args = [binary, command, "--config", config, "--json"]
                    if command == "compress":
                        args += ["--mode", "recoverable"]
                    run = subprocess.run([*args, str(payload)], capture_output=True, text=True)
                    if run.returncode:
                        raise RuntimeError(f"{command} failed: {run.stderr}")
                    outputs.append(json.loads(run.stdout))
                detected, compressed = outputs
                ci, co = compressed["chars_in"], compressed["chars_out"]
                total_in += ci
                total_out += co
                saved = 100 * (ci - co) / ci if ci else 0
                print(f"  {label[:43]:<44}{str(detected.get('content_type') or 'unknown'):<15}"
                      f"{str(compressed.get('kind') or 'unchanged'):<15}{ci:>10,}{co:>10,}{saved:>8.2f}%")
        print(f"\n  tool output chars {total_in:,} -> {total_out:,} "
              f"({100 * (total_in - total_out) / max(total_in, 1):.2f}% removed in local replay)")
