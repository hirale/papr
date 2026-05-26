#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${PAPR_WINDOWS_IMAGE:-papr-tauri-build:bookworm-xwin-warm}"
BASE_IMAGE="${PAPR_WINDOWS_BASE_IMAGE:-papr-tauri-build:bookworm-xwin}"
DOCKERFILE="$ROOT/docker/windows-xwin-warm.Dockerfile"

if ! docker image inspect "$BASE_IMAGE" >/dev/null 2>&1; then
  echo "Missing base image: $BASE_IMAGE" >&2
  echo "Build or import the base image first, then rerun this script." >&2
  exit 1
fi

export DOCKER_BUILDKIT="${DOCKER_BUILDKIT:-1}"

echo "Building warm Windows builder image: $IMAGE"
docker build \
  --build-arg "BASE_IMAGE=$BASE_IMAGE" \
  -f "$DOCKERFILE" \
  -t "$IMAGE" \
  "$ROOT"

echo "Verifying bundled tools and warm caches"
docker run --rm "$IMAGE" bash -c '
  set -euo pipefail
  node --version
  pnpm --version
  rustc --version
  cargo xwin --version
  makensis -VERSION | head -1
  test -f /root/.cache/tauri/NSIS/Plugins/x86-unicode/additional/nsis_tauri_utils.dll
  test -f /usr/share/nsis/Plugins/x86-unicode/additional/nsis_tauri_utils.dll
'

echo "Ready: $IMAGE"
