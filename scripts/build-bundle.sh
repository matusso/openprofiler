#!/usr/bin/env bash
# Builds the remote bundle that OpenProfiler uploads to remote hosts and containers:
#   dist/bundle/<arch>-unknown-linux-musl*/openprofiler-relay          (static, ~1 MB)
#   dist/bundle/<arch>-unknown-linux-musl*/libopenprofiler_agent.so    (for Alpine / musl JVMs)
#   dist/bundle/<arch>-unknown-linux-gnu*/libopenprofiler_agent.so     (for glibc JVMs, glibc 2.17+)
# for x86_64, aarch64 and armv7 (armhf), cross-compiled with cargo-zigbuild.
#
# Usage: scripts/build-bundle.sh [--docker] [arch...]     (arch: x86_64 aarch64 armv7; default all)
#   --docker  build inside scripts/docker/Dockerfile.bundle (no local toolchain needed)
# Needs rustup with the Linux targets, zig, cargo-zigbuild and a JDK (javac) otherwise.
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "${1:-}" == "--docker" ]]; then
  shift
  docker build -q -t openprofiler-bundle-builder -f scripts/docker/Dockerfile.bundle scripts/docker >/dev/null
  exec docker run --rm -v "$PWD":/src -v openprofiler-cargo-registry:/usr/local/cargo/registry -v openprofiler-zig-cache:/root/.cache -w /src \
    -e CARGO_TARGET_DIR=/src/target/bundle -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
    openprofiler-bundle-builder sh -c 'export JAVA_HOME=$(dirname $(dirname $(readlink -f $(command -v javac)))); scripts/build-bundle.sh "$@"; chown -R "$HOST_UID:$HOST_GID" dist/bundle target/bundle' _ "$@"
fi

ARCHES=("$@")
[[ ${#ARCHES[@]} -gt 0 ]] || ARCHES=(x86_64 aarch64 armv7)
OUT=dist/bundle
TARGET_DIR=${CARGO_TARGET_DIR:-target}
GLIBC=2.17

for arch in "${ARCHES[@]}"; do
  case "$arch" in
    x86_64 | aarch64) musl="$arch-unknown-linux-musl" gnu="$arch-unknown-linux-gnu" ;;
    armv7 | armhf) musl=armv7-unknown-linux-musleabihf gnu=armv7-unknown-linux-gnueabihf ;;
    *) echo "unknown arch $arch (x86_64, aarch64, armv7)" >&2; exit 2 ;;
  esac
  mkdir -p "$OUT/$musl" "$OUT/$gnu"

  echo "==> relay ($musl)"
  cargo zigbuild -q --profile release-relay -p openprofiler-relay --target "$musl"
  cp "$TARGET_DIR/$musl/release-relay/openprofiler-relay" "$OUT/$musl/"

  echo "==> musl agent ($musl)"
  RUSTFLAGS="-C target-feature=-crt-static" \
    cargo zigbuild -q --profile release-agent -p openprofiler-agent --target "$musl"
  cp "$TARGET_DIR/$musl/release-agent/libopenprofiler_agent.so" "$OUT/$musl/"

  echo "==> glibc agent ($gnu, glibc $GLIBC)"
  cargo zigbuild -q --profile release-agent -p openprofiler-agent --target "$gnu.$GLIBC"
  cp "$TARGET_DIR/$gnu/release-agent/libopenprofiler_agent.so" "$OUT/$gnu/"
done
ls -la "$OUT"/*/
