#!/usr/bin/env bash
# Runs the overhead benchmark (run.sh) on Linux in a container, where the agent uses its async
# sampler: builds the Linux agent with a Rust container, runs the apps and the load generator in
# an eclipse-temurin container, then checks the recordings on the host. Arguments and WARMUP,
# MEASURE, ROUNDS, THREADS are passed on. Results go to target/bench-linux/.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
JDK_IMAGE="${JDK_IMAGE:-eclipse-temurin:21-jdk}"
docker run --rm -v openprofiler-cargo:/usr/local/cargo/registry -v "$ROOT:/repo" -w /repo \
  -e CARGO_TARGET_DIR=/repo/target/linux rust:1-bookworm cargo build -q --release -p openprofiler-agent
"$ROOT/java/build.sh" >/dev/null
# Builds the apps (and starts PostgreSQL) on the host side, where Docker is.
BUILD_ONLY=1 OUT="$ROOT/target/bench-linux" AGENT=none "$ROOT/java/bench/run.sh" "$@"
docker run --rm -v "$ROOT:/repo" -w /repo --add-host=host.docker.internal:host-gateway \
  -e AGENT=/repo/target/linux/release/libopenprofiler_agent.so \
  -e AGENT_EXTRA=bootjar=/repo/java/openprofiler-boot/build/openprofiler-boot.jar \
  -e OUT=/repo/target/bench-linux -e PG_HOST=host.docker.internal -e SKIP_CHECK=1 \
  -e WARMUP="${WARMUP:-30}" -e MEASURE="${MEASURE:-60}" -e ROUNDS="${ROUNDS:-3}" -e THREADS="${THREADS:-8}" \
  -e CONFIGS="${CONFIGS:-}" -e OPTS="${OPTS:-}" \
  "$JDK_IMAGE" java/bench/run.sh "$@"
(cd "$ROOT" && cargo build -q -p openprofiler-link --example oplink_check)
apps=("$@")
[[ ${#apps[@]} -eq 0 ]] && apps=(petclinic quarkus)
for app in "${apps[@]}"; do
  for config in probes instrument; do
    if out="$("$ROOT/target/debug/examples/oplink_check" "$ROOT/target/bench-linux/$app-$config.oplink" "$ROOT/java/bench/$app.expect" 2>&1)"; then
      echo "$app $config recording: $out"
    else
      echo "$app $config recording: FAIL"
      echo "$out" | sed 's/^/     /'
    fi
  done
done
