#!/usr/bin/env bash
# Remove a per-user kompressor config and its stores from the default paths.
#
# ~/.config/kompressor is on every kompressor's search path, the packaged
# /usr/bin one's included, so a stray config there redirects a plain
# `kompressor stats` or `kompressor wrap` away from the system service. Older
# sweeps (scripts/kompressor-sweep/codex.sh) wrote one there, with the stats
# and refs databases beside it under ~/.local/share/kompressor.
#
# Lists what it found and asks before deleting anything. Directories go only
# when they are empty afterwards.
set -euo pipefail

files=(
  "$HOME/.config/kompressor/config.toml"
  "$HOME/.local/share/kompressor/stats.db"
  "$HOME/.local/share/kompressor/refs.db"
)
dirs=(
  "$HOME/.config/kompressor"
  "$HOME/.local/share/kompressor"
)

found=()
for f in "${files[@]}"; do
  [ -e "$f" ] && found+=("$f")
done
if [ "${#found[@]}" -eq 0 ]; then
  echo "nothing to remove"
  exit 0
fi

echo "will delete:"
ls -la "${found[@]}" | sed 's/^/  /'
read -r -p "Delete these files? [y/N] " answer
case "$answer" in
  y|Y|yes|YES) ;;
  *) echo "nothing deleted"; exit 1 ;;
esac

rm -f -- "${found[@]}"
for d in "${dirs[@]}"; do
  if [ -d "$d" ] && ! rmdir -- "$d" 2>/dev/null; then
    echo "kept $d: not empty"
  fi
done
echo "removed ${#found[@]} file(s)"
