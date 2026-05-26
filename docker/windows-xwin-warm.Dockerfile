ARG BASE_IMAGE=papr-tauri-build:bookworm-xwin
FROM ${BASE_IMAGE}

ENV PNPM_STORE_DIR=/opt/pnpm-store \
    CARGO_HOME=/usr/local/cargo \
    XWIN_CACHE_DIR=/usr/local/xwin-cache \
    CARGO_NET_GIT_FETCH_WITH_CLI=true

WORKDIR /warm

COPY package.json pnpm-lock.yaml ./
RUN pnpm config set store-dir "${PNPM_STORE_DIR}" \
    && pnpm fetch --frozen-lockfile

COPY src-tauri/Cargo.toml src-tauri/Cargo.lock ./src-tauri/
RUN mkdir -p src-tauri/src \
    && printf 'pub fn __warm_fetch_placeholder() {}\n' > src-tauri/src/lib.rs
RUN cargo fetch \
      --manifest-path src-tauri/Cargo.toml \
      --locked \
    && cargo fetch \
      --manifest-path src-tauri/Cargo.toml \
      --locked \
      --target x86_64-pc-windows-msvc

RUN cargo xwin cache xwin --cross-compiler clang-cl

RUN install -d /usr/share/nsis/Plugins/x86-unicode/additional \
    && install -d /root/.cache/tauri/NSIS/Plugins/x86-unicode/additional \
    && curl -fsSL \
      -o /root/.cache/tauri/NSIS/Plugins/x86-unicode/additional/nsis_tauri_utils.dll \
      https://github.com/tauri-apps/nsis-tauri-utils/releases/download/nsis_tauri_utils-v0.5.3/nsis_tauri_utils.dll \
    && echo "75197FEE3C6A814FE035788D1C34EAD39349B860  /root/.cache/tauri/NSIS/Plugins/x86-unicode/additional/nsis_tauri_utils.dll" | sha1sum -c - \
    && cp /root/.cache/tauri/NSIS/Plugins/x86-unicode/additional/nsis_tauri_utils.dll \
      /usr/share/nsis/Plugins/x86-unicode/additional/nsis_tauri_utils.dll
