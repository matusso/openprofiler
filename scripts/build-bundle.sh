#!/usr/bin/env bash
# Builds the remote bundle for Linux on the host architecture using Docker:
#   dist/bundle/<arch>-unknown-linux-musl/ember-relay          (static, ~1 MB)
#   dist/bundle/<arch>-unknown-linux-musl/libember_agent.so    (for Alpine / musl JVMs)
#   dist/bundle/<arch>-unknown-linux-gnu/libember_agent.so     (for glibc JVMs)
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
docker run --rm "${PLATFORM_ARGS[@]}" -v "$ROOT":/src -v ember-cargo-registry:/usr/local/cargo/registry -w /src \
  -e CARGO_TARGET_DIR=/src/target/bundle-musl rust:alpine sh -c '
    apk add -q --no-cache musl-dev openjdk17-jdk >/dev/null
    export JAVA_HOME=/usr/lib/jvm/java-17-openjdk
    cargo build -q --profile release-relay -p ember-relay
    RUSTFLAGS="-C target-feature=-crt-static" cargo build -q --profile release-agent -p ember-agent'
cp target/bundle-musl/release-relay/ember-relay "$OUT/$musl/"
cp target/bundle-musl/release-agent/libember_agent.so "$OUT/$musl/"

echo "==> glibc agent ($gnu)"
docker build -q -t ember-agent-linux -f crates/ember-agent/docker/Dockerfile.linux-test crates/ember-agent/docker >/dev/null
docker run --rm "${PLATFORM_ARGS[@]}" -v "$ROOT":/src -v ember-cargo-registry:/usr/local/cargo/registry -w /src \
  -e CARGO_TARGET_DIR=/src/target/bundle-gnu ember-agent-linux \
  cargo build -q --profile release-agent -p ember-agent
cp target/bundle-gnu/release-agent/libember_agent.so "$OUT/$gnu/"
ls -la "$OUT"/*/
