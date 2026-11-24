#!/usr/bin/env bash
# FreeBSD x86_64 binary and package (.pkg), cross-built on Linux.
#
#   make freebsd                          # the binary, in the container
#   make freebsd-pkg                      # the .pkg, in the same container
#   scripts/build/freebsd.sh [bin|pkg]
#
# The container's entrypoint (scripts/build/Dockerfile.freebsd carries the
# sysroot, the clang wrapper and `pkg`). Also runs natively on FreeBSD, where
# cargo needs no cross compiler and `pkg` is already installed.
#
# The package installs under /usr/local as a port would: the binary, an rc.d
# script (`sysrc kompressor_enable=YES && service kompressor start`), a sample
# config copied to /usr/local/etc/kompressor/config.toml if none is there, and
# a `kompressor` user whose state lives in /var/db/kompressor.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

MODE="${1:-bin}"
case "$MODE" in
  bin|pkg) ;;
  *) echo "usage: $0 [bin|pkg]" >&2; exit 2 ;;
esac

TARGET=x86_64-unknown-freebsd
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1 | tr -d '\r')"
# A package file is named <name>-<version>.pkg and pkg splits that at the last
# `-`, so the version itself cannot contain one: 1.4.0-rc.1 -> 1.4.0.rc.1.
PKG_VERSION="$(echo "$VERSION" | tr '-' '.')"
# Matched with fnmatch by pkg. The binary is linked against the oldest supported
# release's libc, whose symbol versions every newer major keeps, so one package
# serves all of them rather than refusing to install on the next release.
PKG_ABI="${FREEBSD_PKG_ABI:-FreeBSD:*:amd64}"
OUTDIR="target/freebsd"
BIN_OUT="$OUTDIR/kompressor-${VERSION}-x86_64-freebsd"
PKG_OUT="$OUTDIR/kompressor-${VERSION}-x86_64-freebsd.pkg"

native=0
[ "$(uname -s)" = FreeBSD ] && native=1

missing=()
if [ "$native" = 1 ]; then
  BIN="target/release/kompressor"
else
  BIN="target/$TARGET/release/kompressor"
  rustup target list --installed 2>/dev/null | grep -qx "$TARGET" \
    || missing+=("the Rust target — rustup target add $TARGET")
  [ -n "${CARGO_TARGET_X86_64_UNKNOWN_FREEBSD_LINKER:-}" ] \
    || missing+=("a FreeBSD linker and sysroot — set CC_x86_64_unknown_freebsd and CARGO_TARGET_X86_64_UNKNOWN_FREEBSD_LINKER")
fi
if [ "$MODE" = pkg ]; then
  command -v pkg >/dev/null || missing+=("pkg — https://github.com/freebsd/pkg (builds on Linux)")
fi
if ((${#missing[@]})); then
  echo "cannot build for FreeBSD here; missing:" >&2
  printf '  %s\n' "${missing[@]}" >&2
  echo "  or build in the container: make freebsd" >&2
  exit 1
fi

# Always build: cargo decides what is stale, not a file-existence check.
if [ "$native" = 1 ]; then
  echo "building natively ..." >&2
  cargo build --release -p kompressor-cli >&2
else
  echo "building $TARGET ..." >&2
  cargo build --release -p kompressor-cli --target "$TARGET" >&2
fi
[ -f "$BIN" ] || { echo "the build produced no $BIN" >&2; exit 1; }

mkdir -p "$OUTDIR"
cp -f "$BIN" "$BIN_OUT"
echo "$BIN_OUT"

if [ "$MODE" = pkg ]; then
  STAGE="$(mktemp -d)"
  trap 'rm -rf "$STAGE"' EXIT
  ROOTDIR="$STAGE/root"
  META="$STAGE/meta"
  P="$ROOTDIR/usr/local"
  mkdir -p "$P/bin" "$P/etc/rc.d" "$P/etc/kompressor" "$P/etc/newsyslog.conf.d" \
           "$P/share/doc/kompressor" "$META"

  install -m 755 "$BIN_OUT"                              "$P/bin/kompressor"
  install -m 755 packaging/freebsd/kompressor.rc         "$P/etc/rc.d/kompressor"
  install -m 644 packaging/freebsd/newsyslog.conf        "$P/etc/newsyslog.conf.d/kompressor.conf"
  install -m 644 README.md                               "$P/share/doc/kompressor/README.md"
  install -m 644 docs/QUICKSTART.md                      "$P/share/doc/kompressor/QUICKSTART.md"
  install -m 644 LICENSE                                 "$P/share/doc/kompressor/LICENSE"
  # The one config, with FreeBSD's paths: the service's state is in /var/db,
  # local configuration under /usr/local/etc. Shipped as a .sample and copied
  # into place by post-install only when absent, which is what a port's
  # @sample does: an upgrade never overwrites an operator's edits.
  {
    echo "# FreeBSD: installed by the kompressor package as"
    echo "# /usr/local/etc/kompressor/config.toml (from config.toml.sample), and read"
    echo "# by the rc.d script. State lives in /var/db/kompressor, owned by the"
    echo "# 'kompressor' user. Where the notes below say systemd, read rc.d."
    echo "#"
    sed -e 's|/var/lib/kompressor|/var/db/kompressor|g' \
        -e 's|/etc/kompressor|/usr/local/etc/kompressor|g' \
        packaging/kompressor.toml
  } > "$P/etc/kompressor/config.toml.sample"
  chmod 644 "$P/etc/kompressor/config.toml.sample"

  sed -e "s|@VERSION@|$PKG_VERSION|g" -e "s|@ABI@|$PKG_ABI|g" \
      packaging/freebsd/MANIFEST.in > "$META/+MANIFEST"
  for s in POST_INSTALL PRE_DEINSTALL POST_DEINSTALL; do
    cp packaging/freebsd/"$s" "$META/+$s"
  done

  # Ownership is stated, not inherited: the stage is owned by whoever ran
  # this, and a package that installs files owned by uid 1000 is a bug.
  cat > "$STAGE/plist" <<'PLIST'
@(root,wheel,0755) bin/kompressor
@(root,wheel,0555) etc/rc.d/kompressor
@(root,wheel,0644) etc/newsyslog.conf.d/kompressor.conf
@(root,wheel,0644) etc/kompressor/config.toml.sample
@(root,wheel,0644) share/doc/kompressor/README.md
@(root,wheel,0644) share/doc/kompressor/QUICKSTART.md
@(root,wheel,0644) share/doc/kompressor/LICENSE
@dir(root,wheel,0755) etc/kompressor
@dir(root,wheel,0755) share/doc/kompressor
PLIST

  rm -f "$OUTDIR"/kompressor-"$PKG_VERSION".pkg
  pkg create -m "$META" -p "$STAGE/plist" -r "$ROOTDIR" -o "$OUTDIR"
  mv -f "$OUTDIR/kompressor-${PKG_VERSION}.pkg" "$PKG_OUT"
  echo "$PKG_OUT"
fi

for f in "$OUTDIR"/*"${VERSION}"*; do
  ls -lh "$f" | awk '{print "  " $5 "  " $9}'
done
if [ "$MODE" = pkg ]; then
  echo
  echo "  install on FreeBSD:  pkg add ./$(basename "$PKG_OUT")"
  echo "  then:                sysrc kompressor_enable=YES && service kompressor start"
fi
