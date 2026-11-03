#!/usr/bin/env bash
# Every binary and package this machine can build, in one pass, collected into
# target/dist with checksums.
#
# "This machine" is the load-bearing part. A full release matrix needs a C
# toolchain per target — the tree-sitter grammars, bundled SQLite and aws-lc-rs
# are all C — and no single host has them all. So each leg is probed before it
# is attempted and skipped with the command that would enable it, rather than
# run and left to fail three layers down in a linker. What is skipped is
# printed as loudly as what is built: a release that quietly lacks its macOS
# package is worse than one that failed.
#
# The legs are the existing make targets, called recursively. That is
# deliberate: this script decides *whether* to build something, never *how*, so
# `make deb` and the deb inside `make all` can never drift apart.
#
# Exit status is 0 when everything attempted succeeded, even if much was
# skipped, and 1 if any attempted leg failed. Set STRICT=1 to make a skip a
# failure too, which is what a release build wants.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

MAKE="${MAKE:-make}"
STATIC_TARGET="${STATIC_TARGET:-x86_64-unknown-linux-musl}"
STRICT="${STRICT:-0}"
DIST="target/dist"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"

ok=()
skipped=()
failed=()

have() { command -v "$1" >/dev/null 2>&1; }

# Why a leg cannot run here, or empty if it can. Each reason ends with the
# command that fixes it, because "skipped" without that is just a dead end.
want_cargo_deb() {
  have cargo-deb || echo "cargo-deb is not installed: cargo install cargo-deb"
}
want_cargo_rpm() {
  have cargo-generate-rpm || echo "cargo-generate-rpm is not installed: cargo install cargo-generate-rpm"
}
want_musl() {
  if ! rustup target list --installed 2>/dev/null | grep -qx "$STATIC_TARGET"; then
    echo "the Rust target is missing: rustup target add $STATIC_TARGET"
  elif ! have musl-gcc && ! have "${STATIC_TARGET}-gcc"; then
    echo "no musl C compiler: apt install musl-tools   (the tree-sitter grammars are C)"
  fi
}
want_docker() {
  have docker || { echo "docker is not installed"; return; }
  docker info >/dev/null 2>&1 || echo "the docker daemon is not reachable"
}

# Copy a leg's output into the distribution directory. `glob=>name` renames;
# a bare glob keeps the basename.
#
# Every glob is scoped to $VERSION, which is not belt and braces. `make rpm`
# and the macOS build write a versioned filename into a directory they never
# clean, so after a version bump the previous release is still sitting there —
# and a bare `*.rpm` collects it, putting last version's package in the
# directory you are about to upload. Observed on the 1.2.0 to 1.3.0 bump: the
# 1.2.0 .rpm and .pkg came along. (The .deb directories happen to be cleaned,
# one by cargo-deb and one by us, but scoping them too means that is a
# coincidence rather than a dependency.)
#
# This runs the moment a leg succeeds rather than once at the end, because a
# later leg can destroy an earlier one's output: `cargo deb -o target/debian-static`
# empties `target/debian` on its way past, so collecting afterwards loses the
# glibc .deb that `make deb` had just built. Harvesting as we go also means an
# interrupted run leaves whatever did finish.
harvest() {
  local spec src dst f
  for spec in "$@"; do
    if [[ "$spec" == *"=>"* ]]; then
      src="${spec%%=>*}"
      dst="${spec##*=>}"
    else
      src="$spec"
      dst=""
    fi
    for f in $src; do
      [ -e "$f" ] || continue
      cp -f "$f" "$DIST/${dst:-$(basename "$f")}"
    done
  done
}

# leg <label> <make target> <reason-or-empty> [artifact ...]
leg() {
  local label="$1" target="$2" reason="$3"
  shift 3
  if [ -n "$reason" ]; then
    skipped+=("$label	$reason")
    return
  fi
  printf '\n\033[1m==> %s\033[0m  (make %s)\n' "$label" "$target"
  if "$MAKE" "$target"; then
    ok+=("$label")
    harvest "$@"
  else
    failed+=("$label")
  fi
}

musl_reason="$(want_musl)"

# One directory to upload, rather than six paths to remember. Binaries are
# renamed to carry their version and platform: `kompressor` tells you nothing
# once it has been downloaded twice.
rm -rf "$DIST"
mkdir -p "$DIST"

leg "linux x86_64 (glibc) binary" release "" \
    "target/release/kompressor=>kompressor-${VERSION}-x86_64-linux-gnu"
leg "linux x86_64 .deb"           deb         "$(want_cargo_deb)" \
    "target/debian/*${VERSION}*.deb"
leg "linux x86_64 .rpm"           rpm         "$(want_cargo_rpm)" \
    "target/generate-rpm/*${VERSION}*.rpm"
leg "linux x86_64 (musl) binary"  static      "$musl_reason" \
    "target/$STATIC_TARGET/release/kompressor=>kompressor-${VERSION}-x86_64-linux-musl"
leg "linux x86_64 static .deb"    deb-static  "${musl_reason:-$(want_cargo_deb)}" \
    "target/debian-static/*${VERSION}*.deb"
leg "linux x86_64 static .rpm"    rpm-static  "${musl_reason:-$(want_cargo_rpm)}" \
    "target/generate-rpm-static/*${VERSION}*.rpm"
# No file to harvest: the image goes into the local docker daemon, and the tag
# is printed by the target itself.
leg "container image"             docker      "${musl_reason:-$(want_docker)}"
# The macOS leg builds in a container that carries its own pinned zig, xar and
# mkbom, so the host needs nothing but docker — including on a host whose own
# zig is too old for cargo-zigbuild, which every current Debian is.
leg "macos ${MACOS_ARCH:-universal2} .pkg" macos-pkg-docker "$(want_docker)" \
    "target/macos/*${VERSION}*.pkg"

# Checksums over the directory as it ended up, so the file cannot describe an
# artifact that is not there.
if [ -n "$(ls -A "$DIST" 2>/dev/null)" ]; then
  ( cd "$DIST" && sha256sum -- * > SHA256SUMS 2>/dev/null ) || true
fi

# ----------------------------------------------------------------- report ---
printf '\n\033[1m%s\033[0m\n' "kompressor $VERSION"

if [ ${#ok[@]} -gt 0 ]; then
  printf '\n  built:\n'
  printf '    %s\n' "${ok[@]}"
fi

if [ -n "$(ls -A "$DIST" 2>/dev/null)" ]; then
  printf '\n  %s:\n' "$DIST"
  ( cd "$DIST" && ls -1 ) | sed 's/^/    /'
fi

if [ ${#skipped[@]} -gt 0 ]; then
  printf '\n  not built here:\n'
  printf '    %s\n' "${skipped[@]}" | expand -t 4,36
fi

if [ ${#failed[@]} -gt 0 ]; then
  printf '\n  \033[31mfailed:\033[0m\n'
  printf '    %s\n' "${failed[@]}"
  echo
  exit 1
fi

if [ "$STRICT" = 1 ] && [ ${#skipped[@]} -gt 0 ]; then
  printf '\n  STRICT=1 and %d leg(s) were skipped\n\n' "${#skipped[@]}"
  exit 1
fi

echo
