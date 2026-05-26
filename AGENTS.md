# Papr Local Instructions

This checkout is for a personal RSS client, not an upstream PR workflow.

## Working Agreement

- Treat this repo as a self-use fork. Do not prepare PR text, upstream merge
  plans, release notes, or public-facing contribution workflow unless asked.
- Prefer direct local branches and commits when the user asks to ship work.
- For the current reader/UI cleanup, do not touch FreshRSS/Miniflux sync code
  unless the user explicitly asks. Avoid `src-tauri/src/sync.rs`,
  `src-tauri/src/sync/`, and sync-related DB migrations during UI-only fixes.
- Keep changes narrow. For visual bugs, fix the concrete rendered surface first
  and verify with build/runtime checks when possible.

## Normal Verification

Use the local binaries directly when `pnpm build` trips pnpm's build-script
approval check:

```sh
./node_modules/.bin/tsc --noEmit
./node_modules/.bin/vite build
```

Useful project commands:

```sh
pnpm test
pnpm tauri build --ci
```

If `pnpm build` creates a `pnpm-workspace.yaml` that only contains an
`allowBuilds` placeholder, treat it as tool-generated noise and do not commit it
unless the user intentionally wants to approve dependency build scripts.

## Ready Docker Images

Local images currently available for fast verification/packaging:

- `papr-tauri-build:bookworm-xwin-warm`:
  Warm Windows build image for normal local packaging. It layers this repo's
  current pnpm store, Cargo registry, xwin MSVC sysroot, and Tauri's NSIS helper
  on top of `papr-tauri-build:bookworm-xwin`.
- `papr-tauri-build:bookworm-xwin`:
  Node 22, pnpm 9, Rust 1.95, cargo-xwin, clang/lld, and NSIS. Use this first
  as the base image for rebuilding the warm image.
- `papr-tauri-build:bookworm-node22-pnpm9`:
  Node 22, pnpm 9, Rust 1.95. Good for Linux-side frontend/Rust checks.
- `papr-tauri-build:bookworm-rustfmt`:
  Rust 1.95 with rustfmt. Good for Rust formatting checks.
- `papr-tauri-build:bookworm`:
  Base Rust image; it may not have Node/pnpm ready for this repo.

Important: run these images with `bash -c`, not `bash -lc`. A login shell can
reset `PATH` and hide `/usr/local/cargo/bin`.

Check the warm package image:

```sh
docker run --rm papr-tauri-build:bookworm-xwin-warm bash -c \
  'node --version; pnpm --version; rustc --version; cargo xwin --version; makensis -VERSION | head -1'
```

Build or refresh the warm image after `pnpm-lock.yaml`, `package.json`,
`src-tauri/Cargo.lock`, `src-tauri/Cargo.toml`, or the base build image changes:

```sh
scripts/prepare-windows-builder.sh
```

Build the unsigned Windows NSIS installer during normal local work:

```sh
scripts/build-windows.sh
```

This is the self-use fast package path: it keeps Docker offline and reuses
`.cache/papr-build/target`, but overrides Cargo's release profile to avoid
full LTO. Build the slower, smaller full-LTO package only when needed:

```sh
PAPR_FULL_RELEASE=1 scripts/build-windows.sh
```

Expected artifact:

```text
artifacts/windows/Papr_0.5.0_x64-setup.exe
```

The normal build script runs Docker with `--network none`; if dependencies are
missing, refresh the warm image instead of letting the package build download
new files.
