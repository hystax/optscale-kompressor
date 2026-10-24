"""Compress every fixture once, and use that one measurement for both jobs.

The premise check (is the faulty line gone, is there a handle to fetch it back)
and the efficiency table are the same compression. Running them separately
would let the table describe a different compression from the one the premise
was checked against, and a table that does not describe what the model saw is
worse than no table.

Writes a JSON array for the formatter and prints the count of usable languages.
A file that fails the premise is deleted, so the run goes ahead without it.
"""
import json
import os
import subprocess
import sys

# Extension to the language the fixture generator wrote, which is authoritative
# here in a way the detector's answer is not: bughunt.py` emits one file
# per language and we are reading its output.
FIXTURE_LANGUAGE = {
    ".py": "python",
    ".js": "javascript",
    ".ts": "typescript",
    ".go": "go",
    ".rs": "rust",
    ".java": "java",
    ".c": "c",
    ".cpp": "cpp",
}


kompressor, config, fixtures, out_json = sys.argv[1:5]

rows = []
for name in sorted(os.listdir(fixtures)):
    path = os.path.join(fixtures, name)
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
        rows.append({"file": name, "kind": "?", "ok": False, "why": "would not compress"})
        os.remove(path)
        continue

    body = j.get("content", "")
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
    leaked = "bound = size" in body or "bound := size" in body
    ok = not leaked and bool(refs)
    why = "bug still visible" if leaked else ("" if refs else "no handle minted")

    with open(path, encoding="utf-8", errors="replace") as fh:
        lines = sum(1 for _ in fh)

    rows.append({
        "file": name,
        "kind": j.get("kind", ""),
        # Two different questions, and conflating them made the table lie.
        #
        # `language` is what the detector routed the payload to, and it is
        # content-based on purpose: there is no JavaScript signature, because
        # JS matches the TypeScript one and the TypeScript grammar parses it
        # correctly; C++ without a template or `using namespace` lands on C
        # for the same reason. Good routing, useless label — eight fixtures
        # came out as six languages, with typescript n=2 and c n=2, in the one
        # table whose whole point is per-language coverage.
        #
        # `fixture_language` is what the file actually is. We generated it, so
        # it is not a guess, and grouping on it makes the table agree with the
        # "8 language(s) ready" line above it.
        "fixture_language": FIXTURE_LANGUAGE.get(
            os.path.splitext(name)[1].lower(), ""),
        "language": d.get("language") or "",
        "strategy": d.get("strategy") or "",
        "lines": lines,
        "chars_in": j.get("chars_in", 0),
        "chars_out": j.get("chars_out", 0),
        "saved": j.get("saved_pct", 0.0),
        "handles": len(refs),
        "kept": sum(r.get("bytes", 0) for r in refs),
        "ok": ok,
        "why": why,
    })
    if not ok:
        os.remove(path)

with open(out_json, "w") as fh:
    json.dump(rows, fh)
print(sum(1 for r in rows if r.get("ok")))
