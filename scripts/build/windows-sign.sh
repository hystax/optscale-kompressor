#!/usr/bin/env bash
# Sign one Windows executable in place with Authenticode, or do nothing.
#
#   scripts/build/windows-sign.sh FILE
#
# Called by windows.sh for kompressor.exe, and by makensis (!uninstfinalize,
# !finalize) for uninstall.exe and the setup program.
#
# Off unless one of these is set:
#
#   WIN_SIGN_CMD       A command that signs a file in place; the path is appended.
#                      For a cloud HSM key, through jsign:
#                        WIN_SIGN_CMD='jsign --storetype TRUSTEDSIGNING \
#                          --keystore weu.codesigning.azure.net \
#                          --storepass "$WIN_SIGN_PASSWORD" \
#                          --alias account/profile \
#                          --tsaurl http://timestamp.acs.microsoft.com \
#                          --tsmode RFC3161'
#                      Evaluated by the shell. Only the four WIN_SIGN_* variables
#                      reach the build container, so its secret goes in
#                      WIN_SIGN_PASSWORD.
#   WIN_SIGN_PFX       A PKCS#12 file, signed with osslsigncode. The path is seen
#                      from the container, i.e. relative to the checkout; keep it
#                      under target/.
#   WIN_SIGN_PASSWORD  The PFX password, or the secret WIN_SIGN_CMD uses.
#   WIN_SIGN_TSA       Timestamp server for the PFX route; default
#                      http://timestamp.digicert.com, empty for none.
#
# With both set, WIN_SIGN_CMD wins.
set -euo pipefail

FILE="${1:?usage: $0 FILE}"
[ -f "$FILE" ] || { echo "windows-sign: no such file: $FILE" >&2; exit 1; }

if [ -z "${WIN_SIGN_CMD:-}${WIN_SIGN_PFX:-}" ]; then
  exit 0
fi

# Sign a private copy and replace the original only once it is signed. The
# original is often on a host bind mount that Windows may hold open.
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
work="$scratch/$(basename "$FILE")"
cp "$FILE" "$work"

if [ -n "${WIN_SIGN_CMD:-}" ]; then
  echo "windows-sign: signing $(basename "$FILE") with WIN_SIGN_CMD" >&2
  eval "$WIN_SIGN_CMD \"\$work\""
else
  echo "windows-sign: signing $(basename "$FILE") with $WIN_SIGN_PFX" >&2
  tsa=()
  TSA="${WIN_SIGN_TSA-http://timestamp.digicert.com}"
  [ -n "$TSA" ] && tsa=(-ts "$TSA")
  # Password via a file, not the command line.
  printf '%s' "${WIN_SIGN_PASSWORD:-}" > "$scratch/pass"
  osslsigncode sign -pkcs12 "$WIN_SIGN_PFX" -readpass "$scratch/pass" \
    -h sha256 -n "OptScale Kompressor" -i "https://github.com/hystax/optscale-kompressor" \
    "${tsa[@]}" -in "$work" -out "$work.signed" >&2
  mv -f "$work.signed" "$work"
fi

# Fail if the signer exited 0 without signing. Only checks that a signature
# exists; `verify` would also reject a test certificate's chain.
if ! osslsigncode extract-signature -in "$work" -out "$scratch/signature" >/dev/null 2>&1; then
  echo "windows-sign: $(basename "$FILE") carries no signature after signing" >&2
  exit 1
fi
# `cat >` keeps the original's mode and owner.
cat "$work" > "$FILE"
