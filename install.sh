#!/bin/sh
# Install a prebuilt kompressor from the GitHub release. No Rust toolchain.
#
#   curl -fsSL https://raw.githubusercontent.com/hystax/optscale-kompressor/main/install.sh | sh
#
# What it does, per platform:
#
#   Linux x86_64   the static (musl) binary into ~/.local/bin, and a user config
#                  at ~/.config/kompressor/config.toml if there is none. No root.
#   Linux arm64    the same, with the glibc binary (glibc 2.28 or newer).
#   macOS          the universal .pkg through `sudo installer`: the binary in
#                  /usr/local/bin, a user config, and a LaunchAgent.
#
# Every download is checked against the release's SHA256SUMS before anything is
# installed, and a mismatch stops the install.
#
# Settings, all optional, read from the environment:
#
#   KOMPRESSOR_VERSION      a release tag, e.g. 2.6.0     (default: the latest)
#   KOMPRESSOR_INSTALL_DIR  where the Linux binary goes   (default: ~/.local/bin)
#   KOMPRESSOR_NO_CONFIG=1  Linux: do not write a config file
#
# Other platforms (Windows, FreeBSD, .deb and .rpm system packages) are on the
# release page: https://github.com/hystax/optscale-kompressor/releases/latest
set -eu

REPO="hystax/optscale-kompressor"
RELEASES="https://github.com/$REPO/releases"

say()  { printf 'kompressor-install: %s\n' "$*"; }
warn() { printf 'kompressor-install: %s\n' "$*" >&2; }
die()  { warn "$*"; exit 1; }

# The whole script is a function called on the last line, so a download cut
# short under `curl | sh` runs nothing instead of half of it.
main() {
  command -v curl >/dev/null 2>&1 || die "curl is required"

  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Linux)
      case "$arch" in
        x86_64|amd64)  target="x86_64-linux-musl" ;;
        aarch64|arm64) target="aarch64-linux-gnu" ;;
        *) die "no prebuilt Linux binary for $arch; build from source: https://github.com/$REPO#install" ;;
      esac
      ;;
    Darwin) ;;
    *) die "no installer for $os here; see the packages on $RELEASES/latest" ;;
  esac

  version="${KOMPRESSOR_VERSION:-}"
  if [ -z "$version" ]; then
    # The redirect of /releases/latest names the tag. This avoids the API and
    # its rate limit for unauthenticated callers.
    latest="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$RELEASES/latest")" \
      || die "could not reach $RELEASES/latest"
    version="${latest##*/tag/}"
    case "$version" in
      ""|*/*) die "could not work out the latest version from $latest; set KOMPRESSOR_VERSION" ;;
    esac
  fi

  case "$os" in
    Linux)  asset="kompressor-$version-$target" ;;
    Darwin) asset="kompressor-$version-universal2.pkg" ;;
  esac

  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT INT TERM

  say "downloading $asset"
  fetch "$RELEASES/download/$version/$asset" "$tmp/$asset" \
    || die "release $version has no $asset; see $RELEASES"
  fetch "$RELEASES/download/$version/SHA256SUMS" "$tmp/SHA256SUMS" \
    || die "release $version has no SHA256SUMS, so the download cannot be checked"
  verify "$tmp" "$asset"

  case "$os" in
    Linux)  install_linux "$tmp/$asset" "$version" ;;
    Darwin) install_macos "$tmp/$asset" ;;
  esac
}

fetch() {
  curl -fsSL --retry 3 -o "$2" "$1"
}

# Compare the file against its line in SHA256SUMS. Fails closed: no checksum
# tool, or no line for the asset, is an error rather than a skipped check.
verify() {
  dir="$1"
  name="$2"
  want="$(awk -v n="$name" '$2 == n || $2 == "*" n { print $1 }' "$dir/SHA256SUMS")"
  [ -n "$want" ] || die "SHA256SUMS has no entry for $name"
  if command -v sha256sum >/dev/null 2>&1; then
    got="$(sha256sum "$dir/$name" | awk '{ print $1 }')"
  elif command -v shasum >/dev/null 2>&1; then
    got="$(shasum -a 256 "$dir/$name" | awk '{ print $1 }')"
  else
    die "need sha256sum or shasum to check the download"
  fi
  [ "$got" = "$want" ] || die "checksum mismatch for $name: expected $want, got $got"
  say "checksum ok"
}

install_linux() {
  src="$1"
  version="$2"
  dir="${KOMPRESSOR_INSTALL_DIR:-$HOME/.local/bin}"
  mkdir -p "$dir"
  # Copy then rename, so a running kompressor is replaced in one step.
  cp "$src" "$dir/.kompressor.new"
  chmod 755 "$dir/.kompressor.new"
  mv -f "$dir/.kompressor.new" "$dir/kompressor"
  # The arm64 build links glibc; on an older or non-glibc system it will not start.
  # No pipe here: a pipeline would report the status of its last command.
  if ! installed="$("$dir/kompressor" version 2>/dev/null)"; then
    rm -f "$dir/kompressor"
    die "the binary does not run on this system (the arm64 build needs glibc 2.28 or newer); build from source: https://github.com/$REPO#install"
  fi
  say "installed $dir/kompressor ($(printf '%s\n' "$installed" | head -n 1))"

  [ -n "${KOMPRESSOR_NO_CONFIG:-}" ] || write_config "$dir/kompressor" "$version"

  case ":$PATH:" in
    *":$dir:"*) ;;
    *) say "$dir is not on your PATH; add it: export PATH=\"$dir:\$PATH\"" ;;
  esac
  say "next: kompressor wrap codex   (or: claude, opencode, aider)"
}

# A bare binary runs on built-in defaults, which leave Claude Code traffic
# nearly untouched: the Anthropic profile lives in the packaged configuration.
# So write that configuration for this user, the same file the macOS package
# installs, unless one is already there. A failure here is a warning, because
# the binary works without it.
write_config() {
  bin="$1"
  version="$2"
  conf_dir="${XDG_CONFIG_HOME:-$HOME/.config}/kompressor"
  conf="$conf_dir/config.toml"
  state="${XDG_STATE_HOME:-$HOME/.local/state}/kompressor"
  template="https://raw.githubusercontent.com/$REPO/$version/packaging/macos/config.toml.in"

  if [ -e "$conf" ]; then
    say "keeping existing $conf"
    return 0
  fi
  if ! fetch "$template" "$tmp/config.toml.in"; then
    warn "could not fetch the default config; Claude Code needs it, see"
    warn "  https://github.com/$REPO#quick-start-claude-code"
    return 0
  fi
  mkdir -p "$conf_dir" "$state"
  # The loader expands neither ~ nor $HOME, so the real path goes in here.
  sed "s|@STATE@|$state|g" "$tmp/config.toml.in" > "$tmp/config.toml"
  if ! "$bin" check-config --config "$tmp/config.toml" >/dev/null 2>&1; then
    warn "the default config does not load with this version; not installing it"
    return 0
  fi
  cp "$tmp/config.toml" "$conf"
  chmod 600 "$conf"   # it is where an API key would go
  say "wrote $conf"
}

install_macos() {
  pkg="$1"
  say "installing the package; sudo may ask for your password"
  if [ "$(id -u)" -eq 0 ]; then
    installer -pkg "$pkg" -target /
  else
    sudo installer -pkg "$pkg" -target /
  fi
  say "installed $(/usr/local/bin/kompressor version 2>/dev/null | head -n 1)"
  say "next: kompressor wrap codex   (or: claude, opencode, aider)"
}

main "$@"
