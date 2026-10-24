#!/usr/bin/env bash
# Build the container image around a static binary.
#
# `FROM scratch` with one file in it: the binary is static, needs no shell and
# no package manager, and an image with neither is an image with nothing to
# exploit. /data is where the fragment store and statistics live.
set -euo pipefail

IMAGE="${1:-kompressor}"
TAG="${2:-dev}"
TARGET="${3:-x86_64-unknown-linux-musl}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

command -v docker >/dev/null || { echo "docker is not installed" >&2; exit 1; }

BIN="target/$TARGET/release/kompressor"
[[ -x "$BIN" ]] || ./scripts/build/static.sh "$TARGET"

ctx="$(mktemp -d)"
trap 'rm -rf "$ctx"' EXIT
cp "$BIN" "$ctx/kompressor"
cat > "$ctx/Dockerfile" <<'DOCKER'
FROM scratch
COPY kompressor /kompressor
# Written to on first run; declared so a deployment that forgets a volume still
# keeps its fragments for the life of the container rather than failing to open
# the database.
VOLUME ["/data"]
EXPOSE 8817
ENTRYPOINT ["/kompressor"]
CMD ["serve", "--listen", "0.0.0.0:8817"]
DOCKER

docker build -t "$IMAGE:$TAG" "$ctx"
echo
echo "  $IMAGE:$TAG"
echo "  docker run --rm -p 8817:8817 -v kompressor-data:/data $IMAGE:$TAG"
