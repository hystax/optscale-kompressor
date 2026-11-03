#!/usr/bin/env python3
"""Check the documentation against the code it describes.

Prose drifts quietly. Every check here is one that caught a real defect:

* a configuration reference that could not be parsed, because it documented a
  nested `match = { … }` table the schema refuses;
* a `[engine]` block whose values had drifted from the defaults it claimed to
  show;
* `§` cross-references left pointing at sections that had been deleted;
* commands and HTTP endpoints described in the docs that the binary does not
  serve — and, the other way, `/v1/stats/*` being mounted only when statistics
  are enabled, which nothing said;
* a comparison table whose *values* were updated and whose *headers* were not.

Run it with `make docs-check`. It needs the release binary.
"""

from __future__ import annotations

import json
import os
import re
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
BIN = ROOT / "target" / "release" / "kompressor"
DOCS = ["README.md", "docs/QUICKSTART.md", "docs/DESIGN.md"]


def free_port() -> int:
    """A port nothing is listening on. A fixed default collided with whatever
    else the machine runs, and the probe then talked to that instead."""
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


PORT = int(os.environ.get("KOMPRESSOR_CHECK_PORT") or free_port())

problems: list[str] = []


def fail(check: str, detail: str) -> None:
    problems.append(f"{check}: {detail}")


def kompressor(*args: str, **kw) -> subprocess.CompletedProcess:
    return subprocess.run([str(BIN), *args], capture_output=True, text=True, **kw)


# --------------------------------------------------------------------------
def check_toml_blocks() -> None:
    """Every TOML block loads, and the full reference *is* the defaults."""
    defaults = kompressor("check-config", "--config", "/dev/null").stdout
    for name in DOCS:
        text = (ROOT / name).read_text()
        for i, m in enumerate(re.finditer(r"```toml\n(.*?)```", text, re.S), 1):
            block = m.group(1)
            if "[" not in block:
                continue
            with tempfile.NamedTemporaryFile("w", suffix=".toml", delete=False) as f:
                f.write(block)
                path = f.name
            got = kompressor("check-config", "--config", path)
            os.unlink(path)
            if got.returncode != 0:
                first = (got.stderr or got.stdout).strip().splitlines()[0]
                fail("toml", f"{name} block {i} does not load: {first}")
            elif block.lstrip().startswith("# kompressor.toml") and got.stdout != defaults:
                fail("toml", f"{name} block {i} claims to be the defaults and is not")


def check_schema() -> None:
    """The documented DDL has to build the schema the code builds.

    Appendix B used to be checked only for *parsing*, and Appendix E only
    against Appendix B — so a migration that added a column left both stale and
    both self-consistent. The source of truth is the SQL const in the Rust, so
    apply that and the documented DDL to two scratch databases and compare what
    each one actually created.

    The first version of that fix applied a hardcoded `["V1", "V2"]` while the
    ladder grew to eight, which is the same bug one level up: everything after
    v2 was unchecked and the appendix drifted freely behind a passing check.
    Extending it found eight real omissions at once — two tables, three views
    and seven columns, the oldest dating from v4. The ladder is now read from
    `MIGRATIONS` itself, so a new migration is covered the moment it is added
    there rather than when someone remembers this function exists.
    """
    import re
    import sqlite3

    def consts(path: Path, names: list[str]) -> list[str]:
        text = path.read_text()
        out = []
        for name in names:
            # both spellings occur: a raw string where the SQL holds quotes,
            # a plain one where it does not
            match = re.search(
                rf'const {name}: &str = r#"(.*?)"#;', text, re.S
            ) or re.search(rf'const {name}: &str = "(.*?)";', text, re.S)
            if not match:
                fail("schema", f"{path.name} no longer defines {name}")
                return []
            out.append(match.group(1))
        return out

    def ladder(path: Path) -> list[str]:
        """The migration consts, in order, read from MIGRATIONS itself.

        Derived rather than listed, because a hardcoded list is the same bug
        this check exists to catch: it read ["V1", "V2"] while the ladder had
        grown to eight, so every migration after the second was unchecked and
        the appendix could drift freely. Reading the ladder means a new
        migration is covered the moment it is added to it.
        """
        text = path.read_text()
        match = re.search(r"pub const MIGRATIONS: &\[\(u32, &str\)\] = &\[(.*?)\];", text, re.S)
        if not match:
            fail("schema", "schema.rs no longer defines MIGRATIONS")
            return []
        pairs = re.findall(r"\(\s*(\d+)\s*,\s*(V\d+)\s*\)", match.group(1))
        versions = [int(n) for n, _ in pairs]
        if versions != list(range(1, len(versions) + 1)):
            fail("schema", f"the migration ladder is not 1..n in order: {versions}")
        return [name for _, name in pairs]

    schema_rs = ROOT / "crates/kompressor-stats/src/schema.rs"
    code = consts(schema_rs, ladder(schema_rs)) + consts(
        ROOT / "crates/kompressor-store/src/sqlite.rs", ["SCHEMA_V1"]
    )
    documented = re.findall(r"```sql\n(.*?)```", (ROOT / "docs/DESIGN.md").read_text(), re.S)
    # CREATE VIEW too: migrations add views, and a block holding only views
    # would otherwise be dropped here and then reported as undocumented.
    documented = [
        b
        for b in documented
        if "CREATE TABLE" in b or "ALTER TABLE" in b or "CREATE VIEW" in b
    ]

    def normalise(sql: str) -> str:
        """A view body, stripped of everything that is not meaning.

        Comments go, runs of whitespace become one space, and
        `CREATE VIEW IF NOT EXISTS` reads as `CREATE VIEW` — a view created by
        a migration that first drops it spells that clause differently from one
        created once, and the difference says nothing about what the view
        returns. Case is left alone: `tokens_in` and `Tokens_In` are the same
        column to SQLite but not to a reader, and a rename that only changed
        case is exactly the sort of thing worth being told about.
        """
        sql = re.sub(r"--[^\n]*", " ", sql)
        sql = re.sub(r"(?i)\bCREATE\s+VIEW\s+IF\s+NOT\s+EXISTS\b", "CREATE VIEW", sql)
        return re.sub(r"\s+", " ", sql).strip().rstrip(";")

    def schema_of(scripts: list[str]) -> tuple[dict[str, list[str]], dict[str, str]]:
        conn = sqlite3.connect(":memory:")
        for script in scripts:
            conn.executescript(script)
        objects = list(
            conn.execute(
                "SELECT name, type, sql FROM sqlite_master"
                " WHERE type IN ('table','view') ORDER BY 1"
            )
        )
        columns = {
            name: [c[1] for c in conn.execute(f"PRAGMA table_info({name})")]
            for name, _, _ in objects
        }
        views = {
            name: normalise(sql) for name, kind, sql in objects if kind == "view" and sql
        }
        return columns, views

    try:
        from_code, views_from_code = schema_of(code)
        from_docs, views_from_docs = schema_of(documented)
    except sqlite3.Error as error:
        fail("schema", f"a schema block does not apply: {error}")
        return

    for table, columns in from_code.items():
        if table not in from_docs:
            fail("schema", f"the code creates {table}; the documented DDL does not")
            continue
        missing = [c for c in columns if c not in from_docs[table]]
        extra = [c for c in from_docs[table] if c not in columns]
        if missing:
            fail("schema", f"{table}: documented DDL is missing {', '.join(missing)}")
        if extra:
            fail("schema", f"{table}: documented DDL invents {', '.join(extra)}")
    for table in from_docs:
        if table not in from_code:
            fail("schema", f"the documented DDL creates {table}; the code does not")

    # Matching column names are not a matching view. A documented `v_daily_savings`
    # that forgot the `outcome <> 'discarded'` filter has the same shape and a
    # different answer — which is the shape of every accounting bug this project
    # has had.
    for view, body in views_from_code.items():
        if view not in views_from_docs:
            continue  # already reported above as a missing object
        if views_from_docs[view] != body:
            fail(
                "schema",
                f"{view}: the documented body differs from the code's\n"
                f"       code: {body}\n"
                f"       docs: {views_from_docs[view]}",
            )



def diagram_in(png):
    """The drawio XML a `.drawio.png` carries, or None if it carries none.

    The charts are single files: a rendered PNG with the diagram embedded in a
    text chunk, so editing one in drawio updates the picture and the source
    together and neither can go stale against the other. That only works while
    the chunk is there, which is what this reads — in pure Python, because this
    check has to run in a tarball on a machine with no drawio and no display.

    drawio writes `zTXt` keyed `mxGraphModel`, holding URL-encoded XML under
    **raw deflate** — no zlib header, so `zlib.decompress` fails on it and the
    window size has to be given as -15. Older files use `tEXt` keyed `mxfile`
    with no compression at all; both shapes are read.
    """
    import struct
    import urllib.parse
    import zlib

    data = png.read_bytes()
    i = 8
    while i + 12 <= len(data):
        length = struct.unpack(">I", data[i:i + 4])[0]
        kind = data[i + 4:i + 8]
        body = data[i + 8:i + 8 + length]
        if kind in (b"tEXt", b"zTXt"):
            key, _, rest = body.partition(b"\x00")
            if key in (b"mxfile", b"mxGraphModel"):
                if kind == b"zTXt":
                    try:
                        rest = zlib.decompressobj(-15).decompress(rest[1:])
                    except zlib.error:
                        return None
                return urllib.parse.unquote(rest.decode("utf-8", "replace"))
        i += 12 + length
    return None


def check_crate_diagram() -> None:
    """Every edge in the crates diagram is a real Cargo dependency, and none is missing.

    The diagram claims to be the dependency graph, so it is checkable — and a
    diagram nobody checks is a diagram that drifts the first time a crate grows
    a dependency. Edges the picture draws plus the "also directly:" lines in its
    boxes have to be exactly what the manifests say.
    """
    import re
    import xml.etree.ElementTree as ET
    from html import unescape

    source = ROOT / "docs/charts/crates.drawio.png"
    if not source.exists():
        fail("crates", "docs/charts/crates.drawio.png is missing")
        return
    xml = diagram_in(source)
    if xml is None:
        fail("crates", "docs/charts/crates.drawio.png carries no embedded diagram; "
                       "it was exported without drawio's `-e` flag and the source is gone")
        return

    real: set[tuple[str, str]] = set()
    for manifest in sorted((ROOT / "crates").glob("*/Cargo.toml")):
        body = manifest.read_text()
        name = re.search(r'^name = "kompressor-([a-z]+)"', body, re.M)
        if not name:
            continue
        for dep in set(re.findall(r"^kompressor-([a-z]+)\s*=", body, re.M)):
            real.add((name.group(1), dep))

    root = ET.fromstring(xml)
    drawn = {
        (cell.get("source"), cell.get("target"))
        for cell in root.iter("mxCell")
        if cell.get("edge") == "1"
    }
    stated: set[tuple[str, str]] = set()
    for cell in root.iter("mxCell"):
        listed = re.search(r"also directly: ([a-z\u00b7 ]+)</font>", unescape(cell.get("value") or ""))
        if listed:
            stated |= {
                (cell.get("id"), part.strip())
                for part in listed.group(1).split("\u00b7")
                if part.strip()
            }

    covered = drawn | stated
    for edge in sorted(real - covered):
        fail("crates", f"the diagram omits the dependency {edge[0]} -> {edge[1]}")
    for edge in sorted(covered - real):
        fail("crates", f"the diagram draws {edge[0]} -> {edge[1]}, which no manifest declares")


def check_toc() -> None:
    """The table of contents names the sections that exist, in order."""
    doc = (ROOT / "docs/DESIGN.md").read_text()
    toc = re.findall(r"^\d+\. (.+)$", doc, re.M)
    heads = re.findall(r"^## \d+\. (.+)$", doc, re.M)
    for i, (listed, real) in enumerate(zip(toc, heads), 1):
        if listed != real:
            fail("toc", f"entry {i} says {listed!r}, the heading says {real!r}")
    if len(heads) > len(toc):
        fail("toc", f"{len(heads) - len(toc)} section(s) missing from the contents")


def check_section_refs() -> None:
    """Every §N.M points at a section that exists."""
    doc = (ROOT / "docs/DESIGN.md").read_text()
    sections: set[str] = set()
    for m in re.finditer(r"^#{2,3} (\d+)(?:\.(\d+))?\.?\s", doc, re.M):
        sections.add(m.group(1))
        if m.group(2):
            sections.add(f"{m.group(1)}.{m.group(2)}")
    refs: dict[str, set[str]] = {}
    sources = [(n, (ROOT / n).read_text()) for n in DOCS]
    rust = subprocess.run(
        ["grep", "-rho", "§[0-9.]*", "crates/"], cwd=ROOT, capture_output=True, text=True
    ).stdout
    sources.append(("crates/", rust))
    for name, text in sources:
        for ref in re.findall(r"§(\d+(?:\.\d+)?)", text):
            refs.setdefault(ref, set()).add(name)
    for ref, where in sorted(refs.items()):
        if ref not in sections:
            fail("refs", f"§{ref} does not exist, referenced from {', '.join(sorted(where))}")


def check_commands() -> None:
    """Every command the docs name is one the binary has, or is marked absent.

    A command the docs *say* is unbuilt is not a drift; the whole point of
    writing it down is that someone looked for it and found it missing. So a
    line naming one is only a problem when nothing near it admits as much.
    """
    absent = re.compile(r"not built|do(?:es)? not exist|unimplemented|designed, not", re.I)
    listed: dict[str, list[str]] = {}
    for name in DOCS:
        lines = (ROOT / name).read_text().split("\n")
        for i, line in enumerate(lines):
            # Only a code context counts: a backtick span, or a line that
            # begins with the command as a usage block does. Plain prose runs
            # the word `kompressor` into whatever follows it.
            found = re.findall(r"`kompressor ([a-z][a-z-]+)", line)
            found += re.findall(r"^\s*(?:\$ )?kompressor ([a-z][a-z-]+)", line)
            for command in found:
                context = "\n".join(lines[max(0, i - 3) : i + 4])
                listed.setdefault(command, []).append(context)
    for command, contexts in sorted(listed.items()):
        if command in {"wrap", "unwrap"}:
            continue  # clap exits non-zero on a missing required argument
        if kompressor(command, "--help").returncode == 0:
            continue
        if any(absent.search(c) for c in contexts):
            continue
        fail("cli", f"`kompressor {command}` is documented and does not exist")


def check_test_count_agrees() -> None:
    """README and DESIGN must claim the same number of tests.

    Not that the number is *right* -- running the suite here would double every
    docs check -- but that the two places asserting it have not drifted apart.
    They had: the README said 940 while DESIGN said 951 and the suite reported
    989, and nothing was watching. Cross-consistency is cheap and catches the
    case where one is updated and the other is not.
    """
    claims = {}
    for name in ("README.md", "docs/DESIGN.md"):
        found = re.findall(r"\*\*([\d,]+) tests\*\*", (ROOT / name).read_text())
        if found:
            claims[name] = {int(n.replace(",", "")) for n in found}
    if len(claims) < 2:
        fail("test-count", f"expected a test count in both files, found {sorted(claims)}")
        return
    if len(set().union(*claims.values())) != 1:
        detail = "; ".join(f"{n} says {sorted(v)}" for n, v in sorted(claims.items()))
        fail("test-count", f"the docs disagree about the test count: {detail}")


def check_counts() -> None:
    """The counts the docs assert still hold."""
    import gzip

    corpus = ROOT / "fixtures/corpus.jsonl.gz"
    payloads = sum(1 for _ in gzip.open(corpus, "rt")) - 1  # minus the header row
    readme = (ROOT / "README.md").read_text()
    for claimed in re.findall(r"([\d,]+)-payload corpus", readme):
        claimed = claimed.replace(",", "")
        # the large tuning corpus lives outside the repo and is named here too
        if int(claimed) > 1000:
            continue
        if int(claimed) != payloads:
            fail("counts", f"README says a {claimed}-payload corpus; it holds {payloads}")
    for claimed in re.findall(r"checked on all (\d+)\n?corpus payloads", readme):
        if int(claimed) != payloads:
            fail("counts", f"README says all {claimed} corpus payloads; it holds {payloads}")
    vectors = json.load(gzip.open(ROOT / "fixtures/golden.json.gz", "rt"))["records"]
    modes = {m for r in vectors for m in r["modes"]}
    comparisons = len(vectors) * len(modes)
    for name in ("README.md", "docs/DESIGN.md"):
        text = (ROOT / name).read_text()
        for claimed in re.findall(r"all (\d+) of them|(\d+) comparisons", text):
            n = int(claimed[0] or claimed[1])
            if n != comparisons:
                fail("counts", f"{name} claims {n} vector comparisons; there are {comparisons}")


def check_endpoints() -> None:
    """Every documented HTTP path is served, with statistics on."""
    paths = set()
    for name in DOCS:
        text = (ROOT / name).read_text()
        paths |= set(re.findall(r"`(/v1/[a-z_/]+|/healthz|/readyz|/metrics|/mcp)`", text))
    # `/v1/stats` names the family in prose; the endpoints are below it.
    paths = {p for p in paths if p != "/v1/stats"}
    # Proxy paths 404 without an upstream, which is correct; they are the
    # provider's routes, not ours.
    paths -= {"/v1/chat/completions", "/v1/messages", "/v1/responses", "/v1/models", "/mcp"}
    if not paths:
        return

    with tempfile.TemporaryDirectory() as tmp:
        config = Path(tmp) / "c.toml"
        config.write_text(
            '[refs]\nbackend = "memory"\n'
            f'[stats]\nenabled = true\npath = "{tmp}/s.db"\n'
            '[log]\nlevel = "error"\n'
        )
        proc = subprocess.Popen(
            [str(BIN), "serve", "--api", "--mcp", "--config", str(config),
             "--listen", f"127.0.0.1:{PORT}"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        try:
            for _ in range(60):
                if proc.poll() is not None:
                    fail("http", f"the server exited {proc.returncode} before answering on "
                         f"127.0.0.1:{PORT} (is the port taken?); skipping the endpoint check")
                    return
                try:
                    urllib.request.urlopen(f"http://127.0.0.1:{PORT}/healthz", timeout=1)
                    break
                except Exception:
                    time.sleep(0.25)
            else:
                fail("http", f"the server did not come up on 127.0.0.1:{PORT}; "
                     "skipping the endpoint check")
                return
            for path in sorted(paths):
                post = any(k in path for k in ("compress", "retrieve", "detect"))
                req = urllib.request.Request(
                    f"http://127.0.0.1:{PORT}{path}",
                    data=b'{"messages":[]}' if post else None,
                    method="POST" if post else "GET",
                    headers={"content-type": "application/json"},
                )
                try:
                    code = urllib.request.urlopen(req, timeout=5).getcode()
                except urllib.error.HTTPError as e:
                    code = e.code
                except Exception as e:  # noqa: BLE001
                    code = str(e)
                if code in (404, 405):
                    fail("http", f"{path} is documented and answers {code}")
        finally:
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                proc.kill()


def check_wrap_listing() -> None:
    """The agent list in QUICKSTART is the one the binary prints."""
    quick = (ROOT / "docs/QUICKSTART.md").read_text()
    m = re.search(r"^\$ kompressor wrap --list\n(.*?)^```", quick, re.S | re.M)
    if not m:
        return
    actual = kompressor("wrap", "--list").stdout.strip()
    if m.group(1).strip() != actual:
        fail("wrap", "QUICKSTART's `wrap --list` output is not what the binary prints")


def check_links() -> None:
    """Relative links point at files that exist."""
    for name in DOCS:
        path = ROOT / name
        for target in re.findall(r"\]\(([^)#][^)]*)\)", path.read_text()):
            if target.startswith(("http://", "https://", "mailto:")):
                continue
            if not (path.parent / target.split("#")[0]).exists():
                fail("links", f"{name} links to {target}, which does not exist")


# --------------------------------------------------------------------------
def main() -> int:
    if not BIN.exists():
        print(f"{BIN} is missing; run `make release` first", file=sys.stderr)
        return 2
    for check in (
        check_toml_blocks,
        check_schema,
        check_crate_diagram,
        check_toc,
        check_section_refs,
        check_commands,
        check_counts,
        check_test_count_agrees,
        check_endpoints,
        check_wrap_listing,
        check_links,
    ):
        try:
            check()
        except Exception as e:  # noqa: BLE001
            fail(check.__name__, f"the check itself failed: {e}")
    if problems:
        print(f"{len(problems)} documentation problem(s):\n", file=sys.stderr)
        for p in problems:
            print(f"  {p}", file=sys.stderr)
        return 1
    print("docs check: the documentation matches the code")
    return 0


if __name__ == "__main__":
    sys.exit(main())
