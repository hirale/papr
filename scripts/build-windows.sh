#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${PAPR_WINDOWS_IMAGE:-papr-tauri-build:bookworm-xwin-warm}"
HOST_UID="$(id -u)"
HOST_GID="$(id -g)"

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  echo "Missing warm image: $IMAGE" >&2
  echo "Run scripts/prepare-windows-builder.sh once, then rerun this script." >&2
  exit 1
fi

docker run --rm \
  --network none \
  -e "HOST_UID=$HOST_UID" \
  -e "HOST_GID=$HOST_GID" \
  -e CI=true \
  -e PNPM_STORE_DIR=/opt/pnpm-store \
  -e CARGO_HOME=/usr/local/cargo \
  -e CARGO_NET_OFFLINE=true \
  -e CARGO_TARGET_DIR=/workspace/.cache/papr-build/target \
  -e XWIN_CACHE_DIR=/usr/local/xwin-cache \
  -e "PAPR_FULL_RELEASE=${PAPR_FULL_RELEASE:-0}" \
  -v "$ROOT":/workspace \
  -w /workspace \
  "$IMAGE" \
  bash -c '
    set -euo pipefail

    cleanup() {
      chown -R "$HOST_UID:$HOST_GID" artifacts .cache dist node_modules 2>/dev/null || true
    }
    trap cleanup EXIT

    mkdir -p artifacts/windows .cache/papr-build

    echo "[1/3] Linking JS dependencies from the warm image store"
    pnpm install --offline --frozen-lockfile --store-dir "$PNPM_STORE_DIR"

    echo "[2/3] Building unsigned Windows NSIS installer"
    if [ "$PAPR_FULL_RELEASE" != "1" ]; then
      echo "      using fast local release profile; set PAPR_FULL_RELEASE=1 for the smaller full-LTO package"
      export CARGO_INCREMENTAL=1
      export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
      export CARGO_PROFILE_RELEASE_INCREMENTAL=true
      export CARGO_PROFILE_RELEASE_LTO=false
      export CARGO_PROFILE_RELEASE_OPT_LEVEL=2
      export CARGO_PROFILE_RELEASE_STRIP=false
    fi

    pnpm tauri build \
      --ci \
      --runner cargo-xwin \
      --target x86_64-pc-windows-msvc \
      --bundles nsis \
      --no-sign

    echo "[3/3] Copying installer to artifacts/windows"
    installer="$(find "$CARGO_TARGET_DIR/x86_64-pc-windows-msvc/release/bundle/nsis" -maxdepth 1 -type f -name "*setup.exe" -print -quit)"
    if [ -z "$installer" ]; then
      echo "Installer not found under $CARGO_TARGET_DIR/x86_64-pc-windows-msvc/release/bundle/nsis" >&2
      exit 1
    fi

    cp -f "$installer" artifacts/windows/
    sha256sum "artifacts/windows/$(basename "$installer")" > "artifacts/windows/$(basename "$installer").sha256"

    echo "Built artifacts/windows/$(basename "$installer")"
    cat "artifacts/windows/$(basename "$installer").sha256"
  '
