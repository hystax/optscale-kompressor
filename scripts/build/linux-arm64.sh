#!/usr/bin/env bash
# Linux arm64 (aarch64, glibc) binary, .deb and .rpm, cross-built on any Linux.
#
#   make linux-arm64                 # the binary, in the container
#   make linux-arm64-deb             # the .deb
#   make linux-arm64-rpm             # the .rpm
#   make linux-arm64-all             # all three from one build
#   scripts/build/linux-arm64.sh [bin|deb|rpm|all]
#
# The container (scripts/build/Dockerfile.linux-arm64) carries zig,
# cargo-zigbuild, cargo-deb and cargo-generate-rpm. The script also runs
# directly on a host that has those, x86_64 or arm64.
#
# zig is the cross C compiler and linker. The C dependencies (ring, bundled
# SQLite, zstd, the tree-sitter grammars) need one, and zig also lets the
# build pick the glibc it links against. That choice is what lets the
# packages state their libc dependency instead of deriving it: the build host
# has no arm64 libc for dpkg-shlibdeps or ldd to inspect, and when they find
# nothing they quietly emit no dependency at all. The floor is checked against
# the binary after every build, so the number in Cargo.toml cannot drift from
# the file it describes.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

MODE="${1:-bin}"
case "$MODE" in
  bin|deb|rpm|all) ;;
  *) echo "usage: $0 [bin|deb|rpm|all]" >&2; exit 2 ;;
esac

TARGET=aarch64-unknown-linux-gnu
# The oldest glibc the binary may need: Debian 10, Ubuntu 20.04, RHEL 8 and
# newer. It must match `libc6 (>= …)` and `glibc >= …` in the arm64 variants
# in crates/kompressor-cli/Cargo.toml. Raise all three together.
GLIBC_FLOOR="${GLIBC_FLOOR:-2.28}"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1 | tr -d '\r')"
BIN="target/$TARGET/release/kompressor"
# The arm64 variants package this staged copy. Their asset lists do not use
# the target/release/… form, because that path is not rewritten for
# --target and on an x86_64 host it holds an x86_64 binary.
STAGE_BIN="target/arm64/kompressor"
OUTDIR="target/linux-arm64"
BIN_OUT="$OUTDIR/kompressor-${VERSION}-aarch64-linux-gnu"

have() { command -v "$1" >/dev/null 2>&1; }

# Building: cargo-zigbuild wherever it exists, because it is the only way to
# pin the glibc floor. On an arm64 host without it, plain cargo is accepted,
# and the floor check below decides whether that host's glibc is old enough.
builder=""
missing=()
if have cargo-zigbuild && have zig; then
  builder=zig
elif [ "$(uname -m)" = aarch64 ] && [ "$(uname -s)" = Linux ]; then
  builder=native
else
  have cargo-zigbuild || missing+=("cargo-zigbuild: cargo install --locked cargo-zigbuild")
  have zig || missing+=("zig (0.9 or newer): https://ziglang.org/download/, or pip install ziglang")
fi
rustup target list --installed 2>/dev/null | grep -qx "$TARGET" \
  || missing+=("the Rust target: rustup target add $TARGET")
have readelf || missing+=("readelf (binutils), for the glibc floor check")
case "$MODE" in
  deb|all) have cargo-deb || missing+=("cargo-deb: cargo install cargo-deb") ;;
esac
case "$MODE" in
  rpm|all) have cargo-generate-rpm || missing+=("cargo-generate-rpm: cargo install cargo-generate-rpm") ;;
esac
if ((${#missing[@]})); then
  echo "cannot build linux arm64 here; missing:" >&2
  printf '  %s\n' "${missing[@]}" >&2
  echo "  or build in the container, which needs only docker: make linux-arm64" >&2
  exit 1
fi

# Always build. Deciding what is stale is cargo's job, not a file-existence check.
echo "building $TARGET (glibc >= $GLIBC_FLOOR, via $builder) ..." >&2
if [ "$builder" = zig ]; then
  # The `.2.28` suffix is cargo-zigbuild's glibc selector. Output still lands
  # in target/aarch64-unknown-linux-gnu, without the suffix.
  cargo zigbuild --release -p kompressor-cli --target "$TARGET.$GLIBC_FLOOR" >&2
else
  cargo build --release -p kompressor-cli --target "$TARGET" >&2
fi
[ -f "$BIN" ] || { echo "the build produced no $BIN" >&2; exit 1; }

# The checks that make the packages' metadata true: the file is aarch64, and
# no symbol needs a glibc newer than the floor the packages declare.
readelf -h "$BIN" | grep -q 'AArch64' \
  || { echo "$BIN is not an aarch64 binary:" >&2; readelf -h "$BIN" | grep Machine >&2; exit 1; }
need="$(readelf -V "$BIN" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)"
if [ -n "$need" ] && [ "$(printf '%s\n%s\n' "$need" "$GLIBC_FLOOR" | sort -V | tail -1)" != "$GLIBC_FLOOR" ]; then
  echo "$BIN needs glibc $need, newer than the declared floor $GLIBC_FLOOR." >&2
  echo "  The packages would install on systems where it cannot run. Build with" >&2
  echo "  cargo-zigbuild (or in the container: make linux-arm64), or raise the" >&2
  echo "  floor here and in both arm64 variants in crates/kompressor-cli/Cargo.toml." >&2
  exit 1
fi

mkdir -p "$OUTDIR" "$(dirname "$STAGE_BIN")"
cp -f "$BIN" "$STAGE_BIN"
cp -f "$BIN" "$BIN_OUT"
echo "$BIN_OUT"

if [ "$MODE" = deb ] || [ "$MODE" = all ]; then
  rm -rf target/debian-arm64
  # --no-strip: the release profile already strips, and the host's `strip`
  # cannot read an aarch64 ELF. --target is what sets Architecture: arm64.
  cargo deb -p kompressor-cli --variant arm64 --target "$TARGET" \
    --no-build --no-strip -o target/debian-arm64 >&2
  cp -f target/debian-arm64/*"${VERSION}"*.deb "$OUTDIR"/
  ls -1 "$OUTDIR"/*"${VERSION}"*_arm64.deb
fi

if [ "$MODE" = rpm ] || [ "$MODE" = all ]; then
  rm -rf target/generate-rpm-arm64 && mkdir -p target/generate-rpm-arm64
  # --target here sets only the arch (aarch64); the variant names the binary.
  cargo generate-rpm -p crates/kompressor-cli --variant arm64 --target "$TARGET" \
    -o target/generate-rpm-arm64 >&2
  cp -f target/generate-rpm-arm64/*"${VERSION}"*.rpm "$OUTDIR"/
  ls -1 "$OUTDIR"/*"${VERSION}"*.aarch64.rpm
fi

echo
for f in "$OUTDIR"/*"${VERSION}"*; do
  ls -lh "$f" | awk '{print "  " $5 "  " $9}'
done
echo "  glibc floor: $GLIBC_FLOOR (highest symbol the binary uses: ${need:-none})"
