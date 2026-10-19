"""How a Claude Code session read the bughunt fixtures, from a stream-json log.

Prints `<file_reads> <max_files_in_one_turn> <searches>`.

Rules 1 and 2 of the bughunt prompt are not decoration. A model that searches
inside the files never needs the markers, and one that batches eight files into
a single turn hands the engine one mixed-language blob instead of eight
payloads — the code compressor then has almost nothing to work with, and
counting retrievals afterwards measures the batching rather than the loop.
Both have to be checked rather than trusted, and until the session was captured
as stream-json neither could be: `claude -p` prints only the final answer.
"""
import json
import re
import sys

SEARCH = re.compile(r"\b(grep|rg|egrep|awk|perl)\b|sed -n .*/p|readFileSync")
FIXTURE = re.compile(r"\b[Ll]edger\.(c|cpp|go|java|js|py|rs|ts)\b")

reads = searches = widest = 0
for line in open(sys.argv[1], encoding="utf-8", errors="replace"):
    line = line.strip()
    if not line.startswith("{"):
        continue
    try:
        event = json.loads(line)
    except ValueError:
        continue
    if event.get("type") != "assistant":
        continue
    # One assistant message is one turn; several tool_use blocks in it are
    # several files read together, which is what rule 2 forbids.
    in_turn = 0
    for block in event.get("message", {}).get("content", []):
        if block.get("type") != "tool_use":
            continue
        payload = json.dumps(block.get("input", {}))
        if SEARCH.search(payload):
            searches += 1
        hits = len(set(FIXTURE.findall(payload)))
        # `findall` returns the extension group, so count distinct spellings of
        # the whole name instead.
        hits = len({m.group(0) for m in FIXTURE.finditer(payload)})
        if hits:
            reads += hits
            in_turn += hits
    widest = max(widest, in_turn)

print(reads, widest, searches)
