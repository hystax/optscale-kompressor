"""Record and render the paid tests' verdicts.

    record:  verdict.py <file> <test> <verdict> <k=v>...
    render:  verdict.py <file> --text | --html

A test that never ran has no entry, which is not the same as one that failed.
The report says so rather than showing a blank row, because "we did not look"
and "we looked and it was fine" are different findings.
"""
import json
import os
import sys

path = sys.argv[1]
mode = sys.argv[2] if len(sys.argv) > 2 else "--text"

try:
    data = json.load(open(path))
except (OSError, ValueError):
    data = {}

if mode not in ("--text", "--html"):
    name, verdict = sys.argv[2], sys.argv[3]
    facts = {}
    for kv in sys.argv[4:]:
        k, _, v = kv.partition("=")
        facts[k] = v
    data[name] = {"verdict": verdict, "facts": facts}
    with open(path, "w") as fh:
        json.dump(data, fh, indent=1)
    sys.exit(0)

TESTS = [("retrieval", "Retrieval: can the model fetch back what was elided?"),
         ("bughunt", "Bug hunt: the code compressor and the retrieve loop, eight languages")]

if mode == "--text":
    print("  test        verdict        detail")
    print("  " + "-" * 74)
    for key, title in TESTS:
        e = data.get(key)
        if not e:
            print(f"  {key:<11} {'not run':<14} —")
            continue
        detail = ", ".join(f"{k}={v}" for k, v in e["facts"].items())
        print(f"  {key:<11} {e['verdict']:<14} {detail}")
    sys.exit(0)

esc = lambda s: str(s).replace("&", "&amp;").replace("<", "&lt;")
out = ['<section id="sweep-tests">', "<h2>Paid tests</h2>",
       '<p class="note">Each of these spends plan quota and answers one question '
       "the aggregate numbers cannot: whether a model handed a compressed prompt "
       "can get back what was taken out of it. A test that did not run says so; "
       "that is not the same as one that passed.</p>",
       "<table>", "<tr><th>test</th><th>verdict</th><th>detail</th></tr>"]
for key, title in TESTS:
    e = data.get(key)
    if not e:
        out.append(f'<tr><td title="{esc(title)}">{key}</td><td>not run</td><td>—</td></tr>')
        continue
    detail = ", ".join(f"{esc(k)}={esc(v)}" for k, v in e["facts"].items())
    out.append(f'<tr><td title="{esc(title)}">{key}</td>'
               f"<td>{esc(e['verdict'])}</td><td>{detail}</td></tr>")
out.append("</table></section>")
print("\n".join(out))
