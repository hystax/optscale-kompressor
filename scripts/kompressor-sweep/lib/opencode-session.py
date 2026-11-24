"""Read an `opencode run --format json` transcript.

    opencode-session.py LOG                  summary: tool results, truncation, errors
    opencode-session.py LOG --count          `<results> <chars>`
    opencode-session.py LOG --table [--analyse BIN CONFIG]
    opencode-session.py LOG --status EXIT    exit 1, saying why, unless the session completed
    opencode-session.py LOG --answer         the model's last words
    opencode-session.py LOG --reads          `<fixture_reads> <max_in_one_turn> <searches>`
    opencode-session.py LOG --truncated      exit 0 if opencode cut a tool result short
    opencode-session.py LOG --error          the first error opencode reported, if any

One event per line. The ones that matter here:

    {"type":"tool_use","part":{"messageID":…,"tool":"shell",
        "state":{"input":{"command":…},"output":"…","metadata":{"metadata":{"truncated":…}}}}}
    {"type":"text","part":{"messageID":…,"text":"…"}}
    {"type":"step_finish","part":{"reason":"stop"|"tool-calls",…}}
    {"type":"error","error":{"type":"provider.auth","message":"…"}}

A tool event carries its input and its output together, so there is no
id-matching to do; one `messageID` is one assistant turn, which is what the
bughunt's one-file-per-turn rule is counted against.
"""
import json
import re
import sys

from replay import analyse

SEARCH = re.compile(r"\b(grep|rg|egrep|awk|perl|sqlite3)\b|sed -n .*/p|readFileSync")
FIXTURE = re.compile(r"\b[Ll]edger\.(c|cpp|go|java|js|py|rs|ts)\b")


def events(path):
    for line in open(path, encoding="utf-8", errors="replace"):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if isinstance(event, dict):
            yield event


def label_of(state, tool):
    args = state.get("input") or {}
    label = args.get("command") or args.get("filePath") or args.get("file_path") or tool
    return " ".join(str(label).split())


def truncated(state):
    """opencode's own cut: a result over `tool_output` limits is saved to disk
    and the model is sent the head. The proxy then compresses a stub."""
    meta = state.get("metadata") or {}
    inner = meta.get("metadata") if isinstance(meta.get("metadata"), dict) else {}
    if meta.get("truncated") or inner.get("truncated"):
        return True
    output = state.get("output")
    return isinstance(output, str) and output.rstrip().endswith("[output truncated]")


results, errors, texts, finishes = [], [], [], []
turns = {}
for event in events(sys.argv[1]):
    kind = event.get("type")
    part = event.get("part") or {}
    if kind == "tool_use":
        state = part.get("state") or {}
        output = state.get("output")
        if not isinstance(output, str):
            output = ""
        results.append((label_of(state, part.get("tool", "tool")), output, truncated(state)))
        turns.setdefault(part.get("messageID"), []).append(json.dumps(state.get("input") or {}))
    elif kind == "text":
        texts.append(part.get("text") or "")
    elif kind == "step_finish":
        finishes.append(part.get("reason"))
    elif kind == "error":
        error = event.get("error") or {}
        errors.append(error.get("message") or error.get("type") or json.dumps(error)[:300])

if "--count" in sys.argv:
    print(len(results), sum(len(content) for _, content, _ in results))
elif "--answer" in sys.argv:
    print((texts[-1] if texts else "")[-4000:])
elif "--error" in sys.argv:
    print(errors[0][:1000] if errors else "")
elif "--truncated" in sys.argv:
    sys.exit(0 if any(cut for _, _, cut in results) else 1)
elif "--reads" in sys.argv:
    reads = searches = widest = 0
    for inputs in turns.values():
        in_turn = 0
        for payload in inputs:
            if SEARCH.search(payload):
                searches += 1
            hits = len({m.group(0) for m in FIXTURE.finditer(payload)})
            reads += hits
            in_turn += hits
        widest = max(widest, in_turn)
    print(reads, widest, searches)
elif "--status" in sys.argv:
    status = int(sys.argv[sys.argv.index("--status") + 1])
    # `stop` is the model ending its turn; a run that ended any other way —
    # killed, out of steps, an upstream error — did not finish what it was
    # asked, whatever its exit status says.
    if status or errors or not finishes or finishes[-1] != "stop":
        if status:
            print(f"  opencode exit status: {status}")
        if errors:
            print(f"  opencode reported: {errors[0][:1000]}")
        elif not finishes:
            print("  opencode produced no completed step.")
        elif finishes[-1] != "stop":
            print(f"  the last step ended with {finishes[-1]!r}, not 'stop'.")
        sys.exit(1)
else:
    print(f"  tool results    {len(results)} carrying {sum(len(c) for _, c, _ in results):,} chars")
    print(f"  truncated       {sum(cut for _, _, cut in results)} result(s) cut short by opencode")
    if errors:
        print(f"  errors          {len(errors)}; first: {errors[0][:300]}")
    if "--table" in sys.argv:
        print("\n  per-tool output (before proxy compression)")
        print(f"  {'command / tool':<64}{'chars':>10}  delivery")
        print("  " + "-" * 88)
        for label, content, cut in results:
            print(f"  {label[:63]:<64}{len(content):>10,}  {'truncated' if cut else 'inline'}")
    if "--analyse" in sys.argv:
        index = sys.argv.index("--analyse")
        binary, config = sys.argv[index + 1:index + 3]
        analyse([(label, content) for label, content, _ in results], binary, config)
