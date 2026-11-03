"""Write the retrieval fixture: long function bodies with one fact buried inside."""
import argparse

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("directory")
parser.add_argument("token")
parser.add_argument("--compact", action="store_true",
                    help="Keep Claude Code's Bash output below its persistence limit.")
args = parser.parse_args()
d, token = args.directory, args.token
with open(f"{d}/ledger.py", "w") as f:
    f.write('"""Generated ledger module. Do not edit by hand."""\n\n')
    # 22 functions of 34 lines lands around 36 KB / ~9.4K tokens, under the
    # ~10K-token cap at which codex truncates a tool result. The point is that
    # the *proxy* sees the whole file.
    #
    # 26 was over the cap, and the old note here had the truncation wrong in a
    # way that mattered: codex cuts the *middle*, not the tail. It spliced the
    # halves together inside a line, marked the join `…N tokens truncated…`,
    # and the invalid line stopped the parser recovering anything after it — so
    # the body holding the token was never elided, the model read it in the
    # clear, and three runs were filed against a premise that never held.
    # Measured 2026-09-29.
    # Claude Code persists the default ~36 KB Bash result and sends only a
    # preview. Keep reconcile_17 and its planted line in a ~21 KB variant.
    for n in range(18 if args.compact else 22):
        f.write(f"def reconcile_{n:02d}(entries, options=None):\n")
        f.write(f'    """Reconciliation pass {n}."""\n')
        for k in range(24 if args.compact else 34):
            # The fact sits deep inside one body on a line that says nothing
            # about itself, so only the body carries it.
            if n == 17 and k == 17:
                # The fact has to sit *inside* a line the fold removes, not on a
                # line of its own. A unique line survives compression in the
                # clear — correct behaviour, and it means a uniquely-shaped
                # secret can always be read without retrieving anything.
                f.write(f'    step_{k} = compute(entries, "{token}", offset={n * k})\n')
            else:
                f.write(f"    step_{k} = compute(entries, {k}, offset={n * k})\n")
        f.write("    return entries\n\n\n")
