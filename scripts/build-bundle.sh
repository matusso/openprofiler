#!/usr/bin/env bash
# Builds the remote bundle for Linux on the host architecture using Docker:
#   dist/bundle/<arch>-unknown-linux-musl/openprofiler-relay          (static, ~1 MB)
#   dist/bundle/<arch>-unknown-linux-musl/libopenprofiler_agent.so    (for Alpine / musl JVMs)
#   dist/bundle/<arch>-unknown-linux-gnu/libopenprofiler_agent.so     (for glibc JVMs)
# Usage: scripts/build-bundle.sh [--platform linux/amd64]
set -euo pipefail
cd "$(dirname "$0")/.."
PLATFORM_ARGS=()
if [[ "${1:-}" == "--platform" ]]; then PLATFORM_ARGS=(--platform "$2"); fi
ROOT="$PWD"
OUT="$ROOT/dist/bundle"
arch=$(docker run --rm "${PLATFORM_ARGS[@]}" alpine uname -m)
musl="$arch-unknown-linux-musl"
gnu="$arch-unknown-linux-gnu"
mkdir -p "$OUT/$musl" "$OUT/$gnu"

echo "==> relay + musl agent ($musl)"
docker run --rm "${PLATFORM_ARGS[@]}" -v "$ROOT":/src -v openprofiler-cargo-registry:/usr/local/cargo/registry -w /src \
  -e CARGO_TARGET_DIR=/src/target/bundle-musl rust:alpine sh -c '
    apk add -q --no-cache musl-dev openjdk17-jdk >/dev/null
    export JAVA_HOME=/usr/lib/jvm/java-17-openjdk
    cargo build -q --profile release-relay -p openprofiler-relay
    RUSTFLAGS="-C target-feature=-crt-static" cargo build -q --profile release-agent -p openprofiler-agent'
cp target/bundle-musl/release-relay/openprofiler-relay "$OUT/$musl/"
cp target/bundle-musl/release-agent/libopenprofiler_agent.so "$OUT/$musl/"

echo "==> glibc agent ($gnu)"
docker build -q -t openprofiler-agent-linux -f crates/openprofiler-agent/docker/Dockerfile.linux-test crates/openprofiler-agent/docker >/dev/null
docker run --rm "${PLATFORM_ARGS[@]}" -v "$ROOT":/src -v openprofiler-cargo-registry:/usr/local/cargo/registry -w /src \
  -e CARGO_TARGET_DIR=/src/target/bundle-gnu openprofiler-agent-linux \
  cargo build -q --profile release-agent -p openprofiler-agent
cp target/bundle-gnu/release-agent/libopenprofiler_agent.so "$OUT/$gnu/"
ls -la "$OUT"/*/
