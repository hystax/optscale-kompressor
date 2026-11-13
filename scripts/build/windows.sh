#!/usr/bin/env bash
# Windows x86_64 binary and installer, cross-built on Linux.
#
#   make win                              # kompressor.exe, in the container
#   make win-installer                    # the setup .exe, in the same container
#   scripts/build/windows.sh [exe|installer]
#
# The container's entrypoint; runs on any Linux host with mingw-w64, the Rust
# target, makensis and WinSW. The target is -gnu, not -msvc: the MSVC CRT and SDK
# cannot be redistributed in an image. The binary depends on system DLLs only.
#
# Unsigned unless WIN_SIGN_CMD or WIN_SIGN_PFX is set; then kompressor.exe,
# uninstall.exe and the setup program are signed (see windows-sign.sh).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

MODE="${1:-exe}"
case "$MODE" in
  exe|installer) ;;
  *) echo "usage: $0 [exe|installer]" >&2; exit 2 ;;
esac

TARGET=x86_64-pc-windows-gnu
# `tr`: a Windows checkout has CRLF in Cargo.toml.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1 | tr -d '\r')"
# The installer's version resource takes numbers only: 1.4.0-rc.1 -> 1.4.0.
VERSION_NUM="${VERSION%%[-+]*}"
BIN="target/$TARGET/release/kompressor.exe"
# The service wrapper the image carries; override to build elsewhere.
WINSW="${WINSW:-/opt/winsw/WinSW.exe}"
OUTDIR="target/windows"
EXE_OUT="$OUTDIR/kompressor-${VERSION}-x86_64-windows.exe"
SETUP_OUT="$OUTDIR/kompressor-${VERSION}-x86_64-setup.exe"

missing=()
command -v x86_64-w64-mingw32-gcc >/dev/null \
  || missing+=("x86_64-w64-mingw32-gcc — apt install gcc-mingw-w64-x86-64   (ring, SQLite and the grammars are C)")
rustup target list --installed 2>/dev/null | grep -qx "$TARGET" \
  || missing+=("the Rust target — rustup target add $TARGET")
if [ "$MODE" = installer ]; then
  command -v makensis >/dev/null || missing+=("makensis — apt install nsis")
  [ -f "$WINSW" ] || missing+=("WinSW at $WINSW — set WINSW to WinSW.NET461.exe")
fi
SIGN=""
if [ -n "${WIN_SIGN_CMD:-}${WIN_SIGN_PFX:-}" ]; then
  SIGN="$ROOT/scripts/build/windows-sign.sh"
  command -v osslsigncode >/dev/null || missing+=("osslsigncode — apt install osslsigncode")
fi
if ((${#missing[@]})); then
  echo "cannot build for Windows here; missing:" >&2
  printf '  %s\n' "${missing[@]}" >&2
  echo "  or build in the container: make win" >&2
  exit 1
fi

# Always build: cargo decides what is stale, not a file-existence check.
echo "building $TARGET ..." >&2
cargo build --release -p kompressor-cli --target "$TARGET" >&2
[ -f "$BIN" ] || { echo "the build produced no $BIN" >&2; exit 1; }

mkdir -p "$OUTDIR"
cp -f "$BIN" "$EXE_OUT"
# Sign the copy, never cargo's output.
if [ -n "$SIGN" ]; then
  "$SIGN" "$EXE_OUT"
else
  echo "not signing: neither WIN_SIGN_CMD nor WIN_SIGN_PFX is set" >&2
fi
echo "$EXE_OUT"

if [ "$MODE" = installer ]; then
  STAGE="$(mktemp -d)"
  trap 'rm -rf "$STAGE"' EXIT
  # CRLF for the license page and the text files opened in Notepad.
  sed 's/\r$//; s/$/\r/' LICENSE > "$STAGE/LICENSE.txt"
  sed 's/\r$//; s/$/\r/' packaging/windows/GETTING-STARTED.txt > "$STAGE/GETTING-STARTED.txt"
  cp "$WINSW" "$STAGE/kompressor-service.exe"
  [ -z "$SIGN" ] || "$SIGN" "$STAGE/kompressor-service.exe"
  # The installer packs $EXE_OUT, so it installs the same file `make win` produces.
  makensis -V2 -NOCD \
    -DVERSION="$VERSION" \
    -DVERSION_NUM="$VERSION_NUM" \
    -DEXE="$ROOT/$EXE_OUT" \
    -DWINSW="$STAGE/kompressor-service.exe" \
    -DROOT="$ROOT" \
    -DLICENSE="$STAGE/LICENSE.txt" \
    -DGUIDE="$STAGE/GETTING-STARTED.txt" \
    -DOUTFILE="$ROOT/$SETUP_OUT" \
    ${SIGN:+-DSIGN="$SIGN"} \
    packaging/windows/kompressor.nsi >&2
  echo "$SETUP_OUT"
fi

for f in "$OUTDIR"/*"${VERSION}"*; do
  ls -lh "$f" | awk '{print "  " $5 "  " $9}'
done
