"""Name the function the bughunt fixture made faulty.

Derived from `bughunt.py`'s own `BUGGY_INDEX` rather than hardcoded, so the two
cannot drift apart silently, and then asserted against the generated fixture:
if the name is not in the file the model was asked about, the run would read a
correct answer as a wrong one.

Prints the name, or exits non-zero with the reason the premise does not hold.
"""
import re
import sys

helper, fixture_dir = sys.argv[1], sys.argv[2]

source = open(helper, encoding="utf-8").read()
found = re.search(r"^BUGGY_INDEX\s*=\s*(\d+)", source, re.M)
if not found:
    sys.exit("bughunt.py no longer declares BUGGY_INDEX")
name = "reconcile%02d" % int(found.group(1))

# The twelve functions are named alike on purpose, so a typo here would match
# nothing and look like a model failure.
import glob
import os

files = [f for f in glob.glob(os.path.join(fixture_dir, "ledger.*"))
         + glob.glob(os.path.join(fixture_dir, "Ledger.*"))]
if not files:
    sys.exit("no fixture files were generated")
# Case-insensitively: Go exports its functions, so the same index is
# `Reconcile07` there and `reconcile07` everywhere else.
missing = [os.path.basename(f) for f in files
           if name.lower() not in open(f, encoding="utf-8", errors="replace").read().lower()]
if missing:
    sys.exit(f"{name} is absent from {', '.join(missing)}")
print(name)
