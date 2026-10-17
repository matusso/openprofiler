#!/usr/bin/env bash
# Smoke-tests dist/bundle under emulation (Docker with binfmt/QEMU for foreign architectures):
# each relay runs `version` and `list`, and each agent is loaded into a JVM of its libc that
# writes an offline recording.
# Usage: scripts/smoke-bundle.sh [arch...]     (arch: x86_64 aarch64 armv7; default all)
set -euo pipefail
cd "$(dirname "$0")/.."
B="$PWD/dist/bundle"
ARCHES=("$@")
[[ ${#ARCHES[@]} -gt 0 ]] || ARCHES=(x86_64 aarch64 armv7)

# A JVM that loads the agent, runs briefly and must leave a recording behind.
agent_check() { # platform image agent
  local platform=$1 image=$2 agent=$3
  docker run --rm --platform "$platform" -v "$B":/b:ro "$image" sh -c "
    java -agentpath:/b/$agent=mode=offline,file=/tmp/smoke.oplink -version 2>&1 | tail -1 &&
    test -s /tmp/smoke.oplink && echo \"recording: \$(wc -c < /tmp/smoke.oplink) bytes\""
}

for arch in "${ARCHES[@]}"; do
  case "$arch" in
    x86_64) platform=linux/amd64 musl=x86_64-unknown-linux-musl gnu=x86_64-unknown-linux-gnu ;;
    aarch64) platform=linux/arm64 musl=aarch64-unknown-linux-musl gnu=aarch64-unknown-linux-gnu ;;
    armv7 | armhf) platform=linux/arm/v7 musl=armv7-unknown-linux-musleabihf gnu=armv7-unknown-linux-gnueabihf ;;
    *) echo "unknown arch $arch" >&2; exit 2 ;;
  esac
  echo "==> $arch: relay"
  docker run --rm --platform "$platform" -v "$B":/b:ro alpine:3.22 sh -c \
    "/b/$musl/openprofiler-relay version && /b/$musl/openprofiler-relay list"
  echo "==> $arch: glibc agent"
  agent_check "$platform" eclipse-temurin:17-jre "$gnu/libopenprofiler_agent.so"
  # Temurin publishes Alpine JREs for x86_64 and aarch64 only.
  if [[ "$arch" != armv7 && "$arch" != armhf ]]; then
    echo "==> $arch: musl agent"
    agent_check "$platform" eclipse-temurin:21-jre-alpine "$musl/libopenprofiler_agent.so"
  fi
done
