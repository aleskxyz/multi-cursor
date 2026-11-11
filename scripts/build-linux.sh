#!/usr/bin/env bash
# Build portable Linux packages (.deb, .rpm, .AppImage) inside Ubuntu 22.04.
#
# Linux binaries only run on systems whose glibc is at least as new as the one
# they were built against, and AppImages must be built on the oldest distro you
# want to support. Building on 22.04 (glibc 2.35) keeps the packages working on
# Ubuntu 22.04+, Debian 12+, Fedora 36+ and similar.
#
# Requires Docker. Output: dist-linux/ in the repository root.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${ROOT}/dist-linux"
IMAGE="ubuntu:22.04"
NODE_VERSION="20.18.1"

mkdir -p "${OUT}"

# Named volumes cache the toolchains and crates between runs.
docker run --rm \
  -v "${ROOT}":/src:ro \
  -v "${OUT}":/out \
  -v multi-cursor-linux-cargo:/root/.cargo \
  -v multi-cursor-linux-rustup:/root/.rustup \
  -v multi-cursor-linux-target:/build/src-tauri/target \
  -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
  -e NODE_VERSION="${NODE_VERSION}" \
  -e APPIMAGE_EXTRACT_AND_RUN=1 \
  "${IMAGE}" bash -euo pipefail -c '
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
      ca-certificates curl xz-utils file build-essential pkg-config \
      libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev \
      libxdo-dev libssl-dev >/dev/null

    if [ ! -x /opt/node/bin/node ]; then
      curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.xz" \
        | tar -xJ -C /opt && mv "/opt/node-v${NODE_VERSION}-linux-x64" /opt/node
    fi
    export PATH="/opt/node/bin:/root/.cargo/bin:${PATH}"

    if ! command -v cargo >/dev/null; then
      curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable >/dev/null
    fi

    # Copy sources (minus host build output) so the host tree stays untouched.
    mkdir -p /build
    tar -C /src --exclude=./node_modules --exclude=./src-tauri/target \
      --exclude=./dist --exclude=./dist-linux -cf - . | tar -C /build -xf -
    cd /build
    npm ci --silent
    npm run tauri build

    B=/build/src-tauri/target/release/bundle
    cp "$B"/deb/*.deb "$B"/rpm/*.rpm "$B"/appimage/*.AppImage /out/
    chown "${HOST_UID}:${HOST_GID}" /out/*
  '

echo "Packages written to ${OUT}:"
ls -lh "${OUT}"
