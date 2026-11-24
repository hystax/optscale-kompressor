#!/usr/bin/env bash
# Re-render docs/charts/*.drawio.png and docs/charts/*/*.drawio.png from the
# diagram each one already carries.
#
# Each chart is a single file: a PNG that renders anywhere, with the drawio XML
# embedded in it. Editing one in drawio and saving updates both halves at once,
# so this script is not part of the normal loop — the PNG cannot go stale
# against a source it *is*. What it is for is normalising: a hand-edited file
# may have been saved at a different scale or border, and this re-exports every
# chart at the same `-s 2 -b 20` the rest were made with.
#
# `-e` is the flag the whole arrangement depends on. Without it the export is a
# flat picture and the diagram is gone, so the export writes to a temporary
# file and only replaces the original once the result is verified to carry its
# diagram back.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT/docs/charts"

if ! command -v drawio >/dev/null; then
  cat >&2 <<'MSG'
drawio is not installed, so the charts cannot be re-rendered.

  Debian/Ubuntu: download the .deb from https://github.com/jgraph/drawio-desktop/releases
  macOS:         brew install --cask drawio

Nothing is lost by skipping this: each .drawio.png already holds its own
diagram, and the committed rendering is current unless someone resaved one at a
different scale.
MSG
  exit 1
fi

# The export needs a display. Under CI or over ssh, xvfb-run supplies one.
runner=()
if [[ -z "${DISPLAY:-}" ]] && command -v xvfb-run >/dev/null; then
  runner=(xvfb-run -a)
fi

status=0
# Charts may sit one folder down (keepwarm/), grouped by feature.
shopt -s nullglob
for chart in *.drawio.png */*.drawio.png; do
  printf '  %-30s ... ' "$chart"
  tmp="$(mktemp -t "$(basename "${chart%.drawio.png}").XXXXXX.png")"
  if ! "${runner[@]}" drawio -x -f png -e -s 2 -b 20 -o "$tmp" "$chart" >/dev/null 2>&1; then
    echo "FAILED (export)"; rm -f "$tmp"; status=1; continue
  fi
  # Prove the diagram survived before overwriting the only copy of it.
  if ! "${runner[@]}" drawio -x -f xml -o "$tmp.xml" "$tmp" >/dev/null 2>&1 \
     || ! grep -q 'mxCell' "$tmp.xml" 2>/dev/null; then
    echo "FAILED (the export carries no diagram; original kept)"
    rm -f "$tmp" "$tmp.xml"; status=1; continue
  fi
  rm -f "$tmp.xml"
  mv -f "$tmp" "$chart"
  echo "$(stat -c%s "$chart" 2>/dev/null || stat -f%z "$chart") bytes"
done
exit $status
