"""Save Claude stage statistics and render the shared sweep report formats."""
import html
import json
from pathlib import Path
import re
import sqlite3
import subprocess
import sys

mode, work = sys.argv[1], Path(sys.argv[2])
lib = Path(__file__).resolve().parent
reports = work / "reports"
reports.mkdir(parents=True, exist_ok=True)
manifest = reports / "manifest.json"
stages = json.loads(manifest.read_text()) if manifest.exists() else {}


def readonly(path):
    return sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True)


def helper(name, *args):
    return subprocess.check_output(
        [sys.executable, str(lib / name), *map(str, args)], text=True
    )


if mode == "record":
    stage, since, status = sys.argv[3], int(sys.argv[4]), int(sys.argv[5])
    text = (reports / f"{stage}.log").read_text()
    verdicts = re.findall(r"^\s+(PASS|FAIL|VOID|INCONCLUSIVE|ERROR|PREMISE BROKEN) — (.*)", text, re.M)
    if verdicts:
        verdict, reason = verdicts[-1]
    elif stage == "sweep" and "session finished all 16 commands" in text:
        verdict, reason = "PASS", "completed all 16 commands"
    else:
        verdict, reason = "ERROR", "stage did not complete"
    if status and verdict == "PASS":
        verdict, reason = "ERROR", f"stage exited {status}"
    snapshot = reports / f"{stage}.db"
    snapshot.unlink(missing_ok=True)
    if (work / "stats.db").exists():
        with readonly(work / "stats.db") as source, sqlite3.connect(snapshot) as dest:
            source.backup(dest)
    stages[stage] = dict(since=since, verdict=verdict, reason=reason)
    manifest.write_text(json.dumps(stages, indent=2) + "\n")
    sys.exit(0)

binary = sys.argv[3]
if not stages and (work / "stats.db").exists():
    # Older runs did not save stage snapshots. Label their scope explicitly.
    stages = {"latest": dict(since=0, verdict="not recorded", reason="latest database only")}

sections = []
for stage, entry in stages.items():
    db = work / "stats.db" if stage == "latest" else reports / f"{stage}.db"
    if not db.exists():
        continue
    since = entry["since"]
    summary = helper("runstats.py", db, since, "--proxy-only")
    with readonly(db) as conn:
        rows = conn.execute(
            """select model, prompt_tokens, cache_read_tokens, cache_write_tokens,
                      completion_tokens from proxy_requests where time_ms >= ?""", (since,)
        ).fetchall()
    usage = ["", "  provider usage (Anthropic)"]
    for i, label in enumerate(("fresh input", "cache reads", "cache writes", "output"), 1):
        values = [row[i] for row in rows if row[i] is not None]
        value = f"{sum(values):,}" if values else "not reported"
        usage.append(f"    {label:<14}{value}")
    models = sorted({row[0] for row in rows if row[0]})
    usage.append(f"    models        {', '.join(models) or 'not reported'}")
    usage.append("  Claude-plan quota was used; token counts are not a dollar bill.")
    summary += "\n".join(usage) + "\n"
    print(f"\n== {stage} — live proxy traffic")
    print(summary)
    page = reports / f"{stage}.html"
    result = subprocess.run(
        [binary, "stats", "report", "--stats", str(db), "--from", str(since)],
        capture_output=True, text=True,
    )
    link = ""
    if result.returncode == 0:
        page.write_text(result.stdout)
        link = f'<p><a href="reports/{stage}.html">Detailed statistics</a></p>'
    else:
        print(f"  HTML statistics unavailable: {result.stderr.strip()}")
    sections.append(f"<section><h2>{html.escape(stage)} — live proxy traffic</h2>"
                    f"<pre>{html.escape(summary)}</pre>{link}</section>")

print("\n== paid test results")
print(f"  {'test':<12}{'verdict':<16}detail")
print("  " + "-" * 74)
table = ["<section><h2>Paid test results</h2><table>",
         "<tr><th>test</th><th>verdict</th><th>detail</th></tr>"]
for stage in ("sweep", "retrieval", "bughunt"):
    entry = stages.get(stage, dict(verdict="not run", reason="—"))
    print(f"  {stage:<12}{entry['verdict']:<16}{entry['reason']}")
    table.append("<tr>" + "".join(f"<td>{html.escape(str(value))}</td>"
                                 for value in (stage, entry["verdict"], entry["reason"])) + "</tr>")
table.append("</table></section>")
sections.insert(0, "\n".join(table))

for name, preset in (("fixture-languages.json", "fixtures"), ("bughunt-languages.json", "bughunt")):
    path = work / name
    if path.exists():
        print(f"\n== {preset} compression — local premise measurement")
        print(helper("bughunt-table.py", path))
        print(helper("bughunt-table.py", path, "--by-type"))
        sections.append("<section><h2>Local premise measurement (not live traffic)</h2>" +
                        helper("bughunt-table.py", path, f"--preset={preset}", "--html") + "</section>")

page = work / "kompressor-claude-report.html"
page.write_text(
    '<!doctype html><html lang="en"><meta charset="utf-8">'
    '<title>Claude compression sweep</title><style>'
    'body{max-width:1100px;margin:40px auto;padding:0 24px;font:16px system-ui;color:#172033}'
    'section{margin:32px 0}pre{overflow:auto;background:#f3f5f8;padding:20px}'
    'table{border-collapse:collapse;width:100%}th,td{text-align:left;padding:10px;border-bottom:1px solid #ddd}'
    '</style><body><h1>Claude compression sweep</h1>'
    '<p>Live statistics are saved separately for each stage. Local fixture measurements '
    'describe what can compress, not what the model received.</p>'
    + "\n".join(sections) + "</body></html>"
)
print(f"\n== html report\n  {page}")
