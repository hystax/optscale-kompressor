"""Write one source file per language, each with a bug buried inside a long function body.

Two things make this a retrieval test rather than a reading test.

The first is determinism: compression always produces the same elided view, so
re-reading a file gets you nowhere. The only way to see a body is to fetch it.

The second is that the faulty line is *byte-identical* to its eleven siblings.
Every function ends `return rows[bound]`, and `bound` is assigned in the middle
of the body — the part that gets elided. A scanner grepping for a known-bad
shape (`rows[len(rows)]`, `rows[rows.length]`) finds nothing, because no such
line exists anywhere. An earlier draft of this fixture used those shapes and a
model solved it in one `grep` without retrieving anything, which is exactly the
result this arrangement is built to prevent.

It is still not grep-proof. A model that reads every file and *compares* the
twelve `bound =` lines will spot the odd one out without retrieving. The prompt
forbids content search for that reason, and the caller checks whether the ban
was honoured before believing the verdict.
"""
import sys

OUT = sys.argv[1]
FILLER = 26   # comfortably over code_max_body_lines (24)
PIVOT = 13    # where the three bound lines go: mid-body, safely inside the elision


def assemble(head, filler, pivot_lines, tail, close):
    lines = filler[:PIVOT] + pivot_lines + filler[PIVOT:]
    return head + "".join(lines) + tail + close


def python(name, n, buggy):
    filler = [f"    total_{k} = accumulate(rows, {k}, offset={n * k})\n" for k in range(FILLER)]
    pivot = [
        "    last = len(rows) - 1\n",
        "    size = len(rows)\n",
        f"    bound = {'size' if buggy else 'last'}\n",
    ]
    return assemble(
        f'def {name}(rows, options=None):\n    """Pass {n}."""\n',
        filler, pivot, "    return rows[bound]\n", "\n\n")


def go(name, n, buggy):
    filler = [f"\ttotal{k} := accumulate(rows, {k}, {n * k})\n" for k in range(FILLER)]
    pivot = [
        "\tlast := len(rows) - 1\n",
        "\tsize := len(rows)\n",
        f"\tbound := {'size' if buggy else 'last'}\n",
    ]
    return assemble(f"func {name}(rows []int) int {{\n",
                    filler, pivot, "\treturn rows[bound]\n", "}\n\n")


def rust(name, n, buggy):
    filler = [f"    let total_{k} = accumulate(rows, {k}, {n * k});\n" for k in range(FILLER)]
    pivot = [
        "    let last = rows.len() - 1;\n",
        "    let size = rows.len();\n",
        f"    let bound = {'size' if buggy else 'last'};\n",
    ]
    return assemble(f"pub fn {name}(rows: &[i64]) -> i64 {{\n",
                    filler, pivot, "    rows[bound]\n", "}\n\n")


def java(name, n, buggy):
    filler = [f"        long total{k} = accumulate(rows, {k}, {n * k});\n" for k in range(FILLER)]
    pivot = [
        "        int last = rows.length - 1;\n",
        "        int size = rows.length;\n",
        f"        int bound = {'size' if buggy else 'last'};\n",
    ]
    return assemble(f"    static long {name}(long[] rows) {{\n",
                    filler, pivot, "        return rows[bound];\n", "    }\n\n")


def cfam(name, n, buggy):
    filler = [f"    long total_{k} = accumulate(rows, {k}, {n * k});\n" for k in range(FILLER)]
    pivot = [
        "    int last = count - 1;\n",
        "    int size = count;\n",
        f"    int bound = {'size' if buggy else 'last'};\n",
    ]
    return assemble(f"long {name}(const long *rows, int count) {{\n",
                    filler, pivot, "    return rows[bound];\n", "}\n\n")


def js(name, n, buggy):
    filler = [f"  const total{k} = accumulate(rows, {k}, {n * k});\n" for k in range(FILLER)]
    pivot = [
        "  const last = rows.length - 1;\n",
        "  const size = rows.length;\n",
        f"  const bound = {'size' if buggy else 'last'};\n",
    ]
    return assemble(f"function {name}(rows) {{\n",
                    filler, pivot, "  return rows[bound];\n", "}\n\n")


def ts(name, n, buggy):
    filler = [f"  const total{k} = accumulate(rows, {k}, {n * k});\n" for k in range(FILLER)]
    pivot = [
        "  const last = rows.length - 1;\n",
        "  const size = rows.length;\n",
        f"  const bound = {'size' if buggy else 'last'};\n",
    ]
    return assemble(f"export function {name}(rows: number[]): number {{\n",
                    filler, pivot, "  return rows[bound];\n", "}\n\n")


BUGGY_INDEX = 7

PY_PRE = "def accumulate(rows, k, offset=0):\n    return sum(rows[: k + 1]) + offset\n\n\n"
GO_PRE = ("package ledger\n\nfunc accumulate(rows []int, k, off int) int {\n"
          "\ts := off\n\tfor i := 0; i <= k && i < len(rows); i++ {\n\t\ts += rows[i]\n\t}\n\treturn s\n}\n\n")
RS_PRE = ("pub fn accumulate(rows: &[i64], k: usize, off: i64) -> i64 {\n"
          "    off + rows.iter().take(k + 1).sum::<i64>()\n}\n\n")
JS_PRE = ("function accumulate(rows, k, off) {\n"
          "  return off + rows.slice(0, k + 1).reduce((a, b) => a + b, 0);\n}\n\n")
TS_PRE = ("function accumulate(rows: number[], k: number, off: number): number {\n"
          "  return off + rows.slice(0, k + 1).reduce((a, b) => a + b, 0);\n}\n\n")
C_PRE = ("#include <stddef.h>\n\nstatic long accumulate(const long *rows, int k, long off) {\n"
         "    long s = off;\n    for (int i = 0; i <= k; i++) s += rows[i];\n    return s;\n}\n\n")
CPP_PRE = ("#include <cstddef>\n\nstatic long accumulate(const long *rows, int k, long off) {\n"
           "    long s = off;\n    for (int i = 0; i <= k; i++) s += rows[i];\n    return s;\n}\n\n")
JAVA_PRE = ("public final class Ledger {\n    static long accumulate(long[] rows, int k, long off) {\n"
            "        long s = off;\n        for (int i = 0; i <= k && i < rows.length; i++) s += rows[i];\n"
            "        return s;\n    }\n\n")

FILES = [
    ("ledger.py",  PY_PRE,   python, "",             "reconcile{:02d}"),
    ("ledger.go",  GO_PRE,   go,     "",             "Reconcile{:02d}"),
    ("ledger.rs",  RS_PRE,   rust,   "",             "reconcile{:02d}"),
    ("ledger.js",  JS_PRE,   js,     "",             "reconcile{:02d}"),
    ("ledger.ts",  TS_PRE,   ts,     "",             "reconcile{:02d}"),
    ("ledger.c",   C_PRE,    cfam,   "",             "reconcile{:02d}"),
    ("ledger.cpp", CPP_PRE,  cfam,   "",             "reconcile{:02d}"),
    ("Ledger.java", JAVA_PRE, java,  "}\n",          "reconcile{:02d}"),
]

for fname, pre, fn, post, pattern in FILES:
    parts = [pre]
    for i in range(12):
        parts.append(fn(pattern.format(i), i + 1, i == BUGGY_INDEX))
    parts.append(post)
    with open(f"{OUT}/{fname}", "w") as fh:
        fh.write("".join(parts))
    print(fname)
