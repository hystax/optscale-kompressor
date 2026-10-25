"""Measure a set of payloads through the engine directly, without changing them.

The sibling `bughunt-measure.py` also enforces a premise and deletes what
fails it; this one only looks. Output is the row shape `bughunt-table.py`
renders, so the fixture tree and the bug-hunt fixtures print the same table.

    python3 measure.py <kompressor> <config> <out.json> <file>...
"""
import json
import os
import subprocess
import sys

kompressor, config, out_json = sys.argv[1:4]
paths = sys.argv[4:]

rows = []
for path in paths:
    if not os.path.isfile(path):
        continue
    proc = subprocess.run(
        [kompressor, "compress", "--config", config, "--mode", "recoverable",
         "--json", path],
        capture_output=True, text=True,
    )
    try:
        j = json.loads(proc.stdout)
    except ValueError:
        rows.append({"file": os.path.basename(path), "kind": "?", "ok": False,
                     "why": "would not compress"})
        continue
    refs = j.get("refs") or []
    # The language is only in `detect`, not `compress`. One extra call per
    # file, which is nothing next to the compression itself, and it is what
    # lets the roll-up say "rust" rather than a second row of "source_code".
    det = subprocess.run(
        [kompressor, "detect", "--config", config, "--json", path],
        capture_output=True, text=True)
    try:
        d = json.loads(det.stdout)
    except ValueError:
        d = {}
    with open(path, encoding="utf-8", errors="replace") as fh:
        lines = sum(1 for _ in fh)
    rows.append({
        "file": os.path.relpath(path, os.path.commonpath([os.path.dirname(p) for p in paths]))
                if len(paths) > 1 else os.path.basename(path),
        "kind": j.get("kind", ""),
        "language": d.get("language") or "",
        "strategy": d.get("strategy") or "",
        "lines": lines,
        "chars_in": j.get("chars_in", 0),
        "chars_out": j.get("chars_out", 0),
        "saved": j.get("saved_pct", 0.0),
        "handles": len(refs),
        "kept": sum(r.get("bytes", 0) for r in refs),
        "ok": True,
        "why": "",
    })

with open(out_json, "w") as fh:
    json.dump(rows, fh)
print(len(rows))
