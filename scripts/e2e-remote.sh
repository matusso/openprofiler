#!/usr/bin/env bash
# End-to-end remote attach through the openprofiler CLI, with no port exposed by the JVM:
#   ssh     an OpenSSH container with a JVM (key auth)          openprofiler attach --ssh
#   docker  the same container through `docker exec`           openprofiler attach --docker
#   kube    a distroless, non-root JVM pod in a kind cluster,   openprofiler attach --kube --debug-container
#           reached through an ephemeral debug container
# Each attach samples for a few seconds and must report the workload's hot method.
#
# Usage: scripts/e2e-remote.sh [ssh] [docker] [kube]     (default: all)
# Needs docker, a built bundle for this machine's Docker architecture (scripts/build-bundle.sh),
# and for kube: kind and kubectl. OPENPROFILER (default target/debug/openprofiler) is the CLI.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OP=${OPENPROFILER:-$ROOT/target/debug/openprofiler}
export OPENPROFILER_BUNDLE_DIR=${OPENPROFILER_BUNDLE_DIR:-$ROOT/dist/bundle}
CASES=("$@")
[[ ${#CASES[@]} -gt 0 ]] || CASES=(ssh docker kube)
WORK=$(mktemp -d)
CLUSTER=openprofiler-e2e
SSHD=openprofiler-e2e-sshd
cleanup() {
  docker rm -f "$SSHD" >/dev/null 2>&1 || true
  [[ -n "${KEEP_CLUSTER:-}" ]] || kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# Attaches with the given target flags and checks the hot spots.
check_attach() {
  local name=$1
  shift
  echo "==> $name: openprofiler attach $*"
  "$OP" attach "$@" --duration 5s --limit 10 | tee "$WORK/$name.txt"
  grep -q "Busy.spinBusyLoop" "$WORK/$name.txt" || { echo "FAIL: $name: Busy.spinBusyLoop not in hot spots" >&2; exit 1; }
  echo "==> $name: ok"
}

start_sshd() {
  docker build -q -t openprofiler-e2e-sshd -f scripts/e2e/Dockerfile.sshd scripts/e2e >/dev/null
  docker rm -f "$SSHD" >/dev/null 2>&1 || true
  ssh-keygen -q -t ed25519 -N "" -f "$WORK/id"
  docker run -d --name "$SSHD" -p 127.0.0.1::22 openprofiler-e2e-sshd >/dev/null
  docker exec -i "$SSHD" sh -c 'cat > /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys' < "$WORK/id.pub"
  SSH_PORT=$(docker port "$SSHD" 22/tcp | head -1 | sed 's/.*://')
  for _ in $(seq 50); do docker exec "$SSHD" pgrep -f Busy >/dev/null && break; sleep 0.2; done
}

for c in "${CASES[@]}"; do
  case "$c" in
    ssh)
      [[ -n "${SSH_PORT:-}" ]] || start_sshd
      check_attach ssh --ssh "root@127.0.0.1:$SSH_PORT" --ssh-identity "$WORK/id" \
        --ssh-option StrictHostKeyChecking=no --ssh-option UserKnownHostsFile=/dev/null --ssh-option LogLevel=ERROR
      ;;
    docker)
      [[ -n "${SSH_PORT:-}" ]] || start_sshd
      check_attach docker --docker "$SSHD"
      ;;
    kube)
      docker build -q -t openprofiler-e2e-distroless:latest -f scripts/e2e/Dockerfile.distroless scripts/e2e >/dev/null
      kind get clusters 2>/dev/null | grep -qx "$CLUSTER" || kind create cluster --name "$CLUSTER" --wait 120s
      kind load docker-image --name "$CLUSTER" openprofiler-e2e-distroless:latest
      CTX="kind-$CLUSTER"
      kubectl --context "$CTX" delete pod busy --ignore-not-found --wait
      kubectl --context "$CTX" apply -f scripts/e2e/busy-pod.yaml
      kubectl --context "$CTX" wait --for=condition=Ready pod/busy --timeout=120s
      # Plain exec fails: the image has no shell.
      if kubectl --context "$CTX" exec busy -- sh -c true 2>/dev/null; then
        echo "FAIL: the distroless pod unexpectedly has a shell" >&2; exit 1
      fi
      check_attach kube --kube default/busy/app --context "$CTX" --debug-container
      # A second attach reuses the running debug container.
      check_attach kube-again --kube default/busy/app --context "$CTX" --debug-container
      n=$(kubectl --context "$CTX" get pod busy -o jsonpath='{.spec.ephemeralContainers[*].name}' | wc -w)
      [[ "$n" -eq 1 ]] || { echo "FAIL: expected one debug container, found $n" >&2; exit 1; }
      ;;
    *) echo "unknown case $c (ssh, docker, kube)" >&2; exit 2 ;;
  esac
done
echo "All remote attach checks passed."
