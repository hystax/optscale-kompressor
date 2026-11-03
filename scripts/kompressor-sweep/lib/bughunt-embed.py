"""Append sections to the generated HTML report.

    bughunt-embed.py <report.html> <script> <arg>...   (one section)
    bughunt-embed.py <report.html> --pairs <script> <arg> ...  (several)

Appended rather than spliced into the generator's own markup: `kompressor stats
report` owns the rest of that document, and a section that vanished because the
generator changed its layout would be worse than one sitting at the end.
"""
import subprocess
import sys

page = sys.argv[1]
rest = sys.argv[2:]

jobs = []
if rest and rest[0] == "--pairs":
    for pair in " ".join(rest[1:]).split("|"):
        parts = pair.split()
        if parts:
            jobs.append(parts)
else:
    jobs.append(rest)

frags = []
for job in jobs:
    out = subprocess.run([sys.executable] + job + ["--html"],
                         capture_output=True, text=True)
    if out.returncode == 0 and out.stdout.strip():
        frags.append(out.stdout)

if frags:
    html = open(page, encoding="utf-8").read()
    marker = "</body>"
    blob = "\n".join(frags)
    html = html.replace(marker, blob + "\n" + marker, 1) if marker in html else html + blob
    open(page, "w", encoding="utf-8").write(html)
print(len(frags))
