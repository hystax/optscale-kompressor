"""Render the per-language compression table, for the terminal and for the report.

`python3 bughunt-table.py lang.json`            -> plain text
`python3 bughunt-table.py lang.json --html`     -> an HTML fragment
"""
import json
import sys

rows = json.load(open(sys.argv[1]))
html = "--html" in sys.argv
ok = [r for r in rows if r.get("ok")]


def kib(n):
    return f"{n / 1024:.1f} KiB"


def totals():
    ci = sum(r["chars_in"] for r in ok)
    co = sum(r["chars_out"] for r in ok)
    return ci, co, (100.0 * (ci - co) / ci if ci else 0.0)


def rollup(rows, key, title, label):
    """Group measured payloads and report the ratio of the sums.

    Averaging per-file percentages would weight a 12 KB file the same as a
    350 KB one; these are ratios of summed characters, which is the number that
    describes the run.
    """
    groups = {}
    for r in rows:
        k = r.get(key) or ""
        if not k:
            continue
        g = groups.setdefault(k, {"n": 0, "ci": 0, "co": 0, "h": 0})
        g["n"] += 1
        g["ci"] += r["chars_in"]
        g["co"] += r["chars_out"]
        g["h"] += r["handles"]
    if not groups:
        return []
    out = []
    w = max(len(label), max(len(k) for k in groups)) + 2
    head = f"  {label:<{w}}{'payloads':>9}{'chars in':>11}{'chars out':>11}{'saved':>9}{'handles':>9}"
    out.append("")
    out.append(f"  {title}")
    out.append(head)
    out.append("  " + "-" * (len(head) - 2))
    order = sorted(groups.items(), key=lambda kv: -(100.0 * (kv[1]["ci"] - kv[1]["co"]) / kv[1]["ci"]))
    for k, g in order:
        pct = 100.0 * (g["ci"] - g["co"]) / g["ci"]
        out.append(f"  {k:<{w}}{g['n']:>9}{g['ci']:>11,}{g['co']:>11,}{pct:>8.2f}%{g['h']:>9}")
    ci = sum(g["ci"] for g in groups.values())
    co = sum(g["co"] for g in groups.values())
    out.append("  " + "-" * (len(head) - 2))
    out.append(f"  {str(len(groups)) + ' ' + label + 's':<{w}}"
               f"{sum(g['n'] for g in groups.values()):>9}{ci:>11,}{co:>11,}"
               f"{100.0 * (ci - co) / ci:>8.2f}%{sum(g['h'] for g in groups.values()):>9}")
    return out


if "--by-type" in sys.argv:
    if not ok:
        print("  nothing measured")
        sys.exit(0)
    for line in rollup(ok, "kind", "by content type", "type"):
        print(line)
    # Grouped on what the fixture *is*, not on what the detector routed it to.
    # The two differ by design — JavaScript has no signature of its own because
    # the TypeScript grammar parses it, and plain C++ lands on C — so grouping
    # on the routed name collapsed eight fixtures into six rows and quietly
    # contradicted the "8 language(s) ready" line printed just above it.
    # Falls back to the routed name for a record written before this existed.
    code = [r for r in ok if r.get("fixture_language") or r.get("language")]
    for r in code:
        r.setdefault("fixture_language", "")
        if not r["fixture_language"]:
            r["fixture_language"] = r.get("language", "")
    if code:
        for line in rollup(code, "fixture_language",
                           "by language (code payloads)", "language"):
            print(line)
        # Routing is worth showing rather than hiding: a fixture that reaches
        # the wrong grammar compresses badly, and this is where that shows up.
        routed = sorted({(r["fixture_language"], r.get("language", "")) for r in code})
        differing = [(f, l) for f, l in routed if l and f != l]
        if differing:
            print()
            print("  routed to another grammar (expected; the grammar parses both)")
            for f, l in differing:
                print(f"    {f:12} -> {l}")
    sys.exit(0)

if not html:
    if not ok:
        print("  no language compressed; nothing to tabulate")
        sys.exit(0)
    # Width follows the longest name so the fixture tree (paths) and the bug hunt
# (bare filenames) both line up without two copies of this renderer.
    w = max(13, max((len(r["file"]) for r in ok), default=13) + 1)
    head = f"  {'file':<{w}}{'type':<13}{'lines':>7}{'chars in':>10}{'chars out':>10}{'saved':>9}{'handles':>9}{'kept':>11}"
    print(head)
    print("  " + "-" * (len(head) - 2))
    for r in sorted(ok, key=lambda r: -r["saved"]):
        print(f"  {r['file']:<{w}}{r['kind'] or 'untouched':<13}{r['lines']:>7}{r['chars_in']:>10}"
              f"{r['chars_out']:>10}{r['saved']:>8.2f}%{r['handles']:>9}{kib(r['kept']):>11}")
    ci, co, pct = totals()
    print("  " + "-" * (len(head) - 2))
    print(f"  {str(len(ok)) + ' payloads':<{w + 13}}{sum(r['lines'] for r in ok):>7}{ci:>10}{co:>10}"
          f"{pct:>8.2f}%{sum(r['handles'] for r in ok):>9}"
          f"{kib(sum(r['kept'] for r in ok)):>11}")
    # The aggregate is computed from the summed characters, not by averaging the
    # per-file percentages: files differ in size and the mean of ratios is not
    # the ratio of sums.
    for r in rows:
        if not r.get("ok"):
            print(f"  dropped: {r['file']} — {r.get('why', 'premise failed')}")
    sys.exit(0)

esc = lambda s: str(s).replace("&", "&amp;").replace("<", "&lt;")
# Markup matches the generator's house style — `class="n"` for numeric columns,
# `p.note` for prose, no thead — so the section reads as part of the report
# rather than something stapled to the end of it.
n = lambda v: f'<td class="n">{v}</td>'
nh = lambda v: f'<th class="n">{v}</th>'

# The same renderer serves two different tables, so it has to be told which one
# it is drawing. Without this both sections carried the bug hunt's heading and
# its `id`, which made the fixture tree read as eight languages of ledger.
def arg(flag, fallback):
    for a in sys.argv:
        if a.startswith(flag + "="):
            return a[len(flag) + 1:]
    return fallback


# Presets rather than free-text flags: the caller passes these through a shell
# that splits on whitespace, and a title with spaces in it does not survive
# that. A name does.
PRESETS = {
    "bughunt": (
        "bughunt-languages",
        "Bug hunt: compression by language",
        "One source file per language in the detector's code table: twelve "
        "near-identical functions each, one out-of-bounds read buried in a body. "
        "Measured by the engine directly, on the same compression the premise "
        "check ran against — these are the files the model was given.",
    ),
    "fixtures": (
        "sweep-fixtures",
        "Sweep fixtures: compression by payload",
        "The payloads the sweep sends, measured by the engine directly. This is "
        "the ceiling, not the result: through codex the same files arrive wrapped "
        "in a harness frame and truncated at about 40 KB, so what the proxy "
        "actually sees is smaller and harder.",
    ),
}
section_id, title, note = PRESETS[arg("--preset", "bughunt")]

out = [f'<section id="{esc(section_id)}">',
       f"<h2>{esc(title)}</h2>",
       f'<p class="note">{esc(note)}</p>',
       "<table>",
       "<tr><th>file</th><th>type</th>" + nh("lines") + nh("chars in")
       + nh("chars out") + nh("saved") + nh("handles") + nh("kept") + "</tr>"]
for r in sorted(ok, key=lambda r: -r["saved"]):
    out.append(f"<tr><td>{esc(r['file'])}</td><td>{esc(r['kind'] or 'untouched')}</td>"
               + n(f"{r['lines']:,}") + n(f"{r['chars_in']:,}")
               + n(f"{r['chars_out']:,}") + n(f"{r['saved']:.2f}%")
               + n(r["handles"]) + n(kib(r["kept"])) + "</tr>")
ci, co, pct = totals()
out.append(f"<tr><th>{len(ok)} languages</th><th></th>"
           + nh(f"{sum(r['lines'] for r in ok):,}") + nh(f"{ci:,}") + nh(f"{co:,}")
           + nh(f"{pct:.2f}%") + nh(sum(r["handles"] for r in ok))
           + nh(kib(sum(r["kept"] for r in ok))) + "</tr>")
out.append("</table>")
dropped = [r for r in rows if not r.get("ok")]
if dropped:
    out.append('<p class="note">Dropped before the run: ' + ", ".join(
        f"{esc(r['file'])} ({esc(r.get('why', 'premise failed'))})"
        for r in dropped) + "</p>")
out.append('<p class="note">The total is computed from the summed characters, '
           "not by averaging the per-file percentages: the files differ in size "
           "and the mean of ratios is not the ratio of sums.</p></section>")
print("\n".join(out))
