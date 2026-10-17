#!/usr/bin/env bash
# Probe version matrix: runs each case of matrix.txt (a scenario from scenarios/ with one set of
# library versions) with the agent in offline mode and checks the recording against the
# scenario's expect.txt (see crates/openprofiler-link/examples/oplink_check.rs).
#
#   java/matrix/run.sh                 # every case
#   java/matrix/run.sh jdbc- grpc      # cases whose name starts with one of the arguments
#
# Needs Docker (Maven resolves the dependencies in a container, and PostgreSQL, MySQL and Kafka run
# in containers) and a JDK 17+ on the host (JAVA_HOME, or `java` on the PATH). Work files go to
# target/probe-matrix/<case>/; KEEP_SERVICES=1 leaves the service containers running.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
MATRIX="$ROOT/java/matrix"
WORK="$ROOT/target/probe-matrix"
JAVA="${JAVA_HOME:+$JAVA_HOME/bin/}java"
JAVAC="${JAVA_HOME:+$JAVA_HOME/bin/}javac"
MAVEN_IMAGE="maven:3.9-eclipse-temurin-21"
# Maven's repository: a Docker volume or a host directory (absolute path). On Linux it is the
# user's ~/.m2 and Maven runs as the user, so nothing in the checkout ends up owned by root.
if [[ "$(uname -s)" == Linux ]]; then
  M2_VOLUME="${M2_VOLUME:-$HOME/.m2}"
  DOCKER_USER=(-u "$(id -u):$(id -g)")
else
  M2_VOLUME="${M2_VOLUME:-openprofiler-matrix-m2}"
  DOCKER_USER=()
fi
mkdir -p "$WORK"

case "$(uname -s)" in
  Darwin) AGENT="$ROOT/target/debug/libopenprofiler_agent.dylib" ;;
  *) AGENT="$ROOT/target/debug/libopenprofiler_agent.so" ;;
esac
(cd "$ROOT" && cargo build -q -p openprofiler-agent && cargo build -q -p openprofiler-link --example oplink_check)
"$ROOT/java/build.sh" >/dev/null
API_JAR="$ROOT/java/openprofiler-api/build/openprofiler-api.jar"
CHECK="$ROOT/target/debug/examples/oplink_check"

# ---- services ------------------------------------------------------------------------------------

started=()
stop_services() {
  if [[ "${KEEP_SERVICES:-}" != 1 ]]; then
    for s in "${started[@]+"${started[@]}"}"; do docker rm -f "opmatrix-$s" >/dev/null 2>&1 || true; done
  fi
}
trap stop_services EXIT

wait_for() { # name, command...
  local name="$1"; shift
  for _ in $(seq 1 90); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 1
  done
  echo "service $name did not become ready" >&2
  docker logs "opmatrix-$name" 2>&1 | tail -20 >&2
  return 1
}

service() {
  local s="$1"
  [[ " ${started[*]+"${started[*]}"} " == *" $s "* ]] && return 0
  if ! docker inspect "opmatrix-$s" >/dev/null 2>&1; then
    case "$s" in
      postgres)
        docker run -d --name opmatrix-postgres -p 55432:5432 -e POSTGRES_PASSWORD=matrix -e POSTGRES_USER=matrix \
          -e POSTGRES_DB=matrix postgres:16-alpine >/dev/null ;;
      mysql)
        docker run -d --name opmatrix-mysql -p 53306:3306 -e MYSQL_ROOT_PASSWORD=matrix -e MYSQL_DATABASE=matrix \
          -e MYSQL_USER=matrix -e MYSQL_PASSWORD=matrix mysql:8.4 >/dev/null ;;
      kafka)
        # The broker listens on the host's port inside the container too, so the advertised
        # address works on both sides.
        docker run -d --name opmatrix-kafka -p 59092:59092 \
          -e KAFKA_NODE_ID=1 -e KAFKA_PROCESS_ROLES=broker,controller \
          -e KAFKA_LISTENERS=PLAINTEXT://:59092,CONTROLLER://:9093 \
          -e KAFKA_ADVERTISED_LISTENERS=PLAINTEXT://127.0.0.1:59092 \
          -e KAFKA_CONTROLLER_LISTENER_NAMES=CONTROLLER -e KAFKA_CONTROLLER_QUORUM_VOTERS=1@localhost:9093 \
          -e KAFKA_LISTENER_SECURITY_PROTOCOL_MAP=CONTROLLER:PLAINTEXT,PLAINTEXT:PLAINTEXT \
          -e KAFKA_OFFSETS_TOPIC_REPLICATION_FACTOR=1 -e KAFKA_GROUP_INITIAL_REBALANCE_DELAY_MS=0 \
          -e KAFKA_TRANSACTION_STATE_LOG_REPLICATION_FACTOR=1 -e KAFKA_TRANSACTION_STATE_LOG_MIN_ISR=1 \
          apache/kafka:3.9.1 >/dev/null ;;
      *) echo "unknown service $s" >&2; return 1 ;;
    esac
  fi
  started+=("$s")
  case "$s" in
    postgres) wait_for postgres docker exec opmatrix-postgres pg_isready -U matrix -d matrix ;;
    mysql) wait_for mysql docker exec opmatrix-mysql mysql -umatrix -pmatrix -h127.0.0.1 -e 'select 1' matrix ;;
    kafka) wait_for kafka docker exec opmatrix-kafka /opt/kafka/bin/kafka-topics.sh --bootstrap-server 127.0.0.1:59092 --list ;;
  esac
}

# ---- cases ---------------------------------------------------------------------------------------

resolve() { # dir, scope (runtime: lib/, all: tools/), -Dprops...
  local dir="$1" scope="$2"; shift 2
  local out=lib extra=(-DincludeScope=runtime)
  if [[ "$scope" == all ]]; then out=tools; extra=(); fi
  docker run --rm "${DOCKER_USER[@]+"${DOCKER_USER[@]}"}" -v "$M2_VOLUME:/var/maven/.m2" -e MAVEN_CONFIG=/var/maven/.m2 \
    -v "$dir:/w" -w /w "$MAVEN_IMAGE" \
    mvn -B -q -Duser.home=/var/maven -f pom.xml dependency:copy-dependencies -DoutputDirectory="$out" "${extra[@]+"${extra[@]}"}" "$@"
}

run_case() { # name scenario services props...
  local name="$1" scenario="$2" services="$3"; shift 3
  local props=("$@") dprops=() ee=jakarta
  for p in "${props[@]+"${props[@]}"}"; do
    dprops+=("-D$p")
    [[ "$p" == ee=* ]] && ee="${p#ee=}"
  done
  local dir="$WORK/$name"
  rm -rf "$dir/src" "$dir/classes" "$dir/rec.oplink"
  mkdir -p "$dir/src" "$dir/classes"
  local src="$MATRIX/scenarios/$scenario"
  cp "$src/pom.xml" "$dir/pom.xml"
  local includes=("$src")
  if [[ -f "$src/include" ]]; then
    while read -r inc; do [[ -n "$inc" ]] && includes+=("$MATRIX/scenarios/$inc"); done <"$src/include"
  fi
  for d in "${includes[@]}"; do
    find "$d" -maxdepth 1 \( -name '*.java' -o -name '*.kt' \) -exec cp {} "$dir/src/" \;
    for r in "$d/resources" "$d/resources.$ee"; do
      [[ -d "$r" ]] && cp -R "$r/." "$dir/classes/"
    done
  done
  # Sources and resources may use ${prop} from the case's properties.
  for p in "${props[@]+"${props[@]}"}"; do
    local k="${p%%=*}" v="${p#*=}"
    find "$dir/src" "$dir/classes" -type f \( -name '*.java' -o -name '*.kt' -o -name '*.xml' \) \
      -exec sed -i.bak "s|\${$k}|$v|g" {} \;
  done
  find "$dir" -name '*.bak' -delete
  if [[ "$ee" == javax ]]; then
    # Sources are written against Jakarta EE; javax cases use the old package names.
    find "$dir/src" "$dir/classes" -type f \( -name '*.java' -o -name '*.kt' -o -name '*.xml' \) \
      -exec sed -i.bak -e 's/jakarta\.servlet/javax.servlet/g' -e 's/jakarta\.persistence/javax.persistence/g' {} \;
    find "$dir" -name '*.bak' -delete
  fi
  for s in $services; do [[ "$s" != - ]] && service "$s"; done

  local stamp
  stamp="$(printf '%s\n' "${props[@]+"${props[@]}"}" | cat - "$dir/pom.xml" | cksum | cut -d' ' -f1)"
  if [[ ! -f "$dir/lib.stamp" || "$(cat "$dir/lib.stamp")" != "$stamp" ]]; then
    rm -rf "${dir:?}/lib" "${dir:?}/tools"
    resolve "$dir" runtime "${dprops[@]+"${dprops[@]}"}"
    if compgen -G "$dir/src/*.kt" >/dev/null; then resolve "$dir" all "${dprops[@]+"${dprops[@]}"}"; fi
    echo "$stamp" >"$dir/lib.stamp"
  fi

  local cp="$dir/lib/*:$API_JAR"
  if compgen -G "$dir/src/*.kt" >/dev/null; then
    local kcp
    kcp="$(printf '%s:' "$dir"/lib/*.jar)$API_JAR"
    "$JAVA" -cp "$dir/tools/*" org.jetbrains.kotlin.cli.jvm.K2JVMCompiler -nowarn -no-stdlib -no-reflect \
      -jvm-target 11 -cp "$kcp" -d "$dir/classes" "$dir"/src/*.kt "$dir"/src/*.java
    cp="$dir/classes:$cp"
  fi
  "$JAVAC" -nowarn -encoding UTF-8 -cp "$dir/classes:$cp" -d "$dir/classes" "$dir"/src/*.java
  cp="$dir/classes:$cp"

  local log="$dir/run.log"
  local opts="mode=offline,file=$dir/rec.oplink,instrument=matrix.*,autotune=0,probe_events=1000"
  if ! "$JAVA" "-agentpath:$AGENT=$opts" -Dopenprofiler.probes.debug=true -Xmx512m -cp "$cp" "${dprops[@]+"${dprops[@]}"}" \
      matrix.Main >"$log" 2>&1; then
    echo "FAIL $name: the workload failed (see $log)"
    tail -20 "$log" | sed 's/^/     /'
    return 1
  fi
  # Expectations may use ${prop} from the case's properties.
  local expect="$dir/expect.txt"
  cp "$src/expect.txt" "$expect"
  for p in "${props[@]+"${props[@]}"}"; do
    local k="${p%%=*}" v="${p#*=}"
    sed -i.bak "s|\${$k}|$v|g" "$expect" && rm -f "$expect.bak"
  done
  if out="$("$CHECK" "$dir/rec.oplink" "$expect" 2>&1)"; then
    echo "ok   $name ($out)"
  else
    echo "FAIL $name"
    echo "$out" | sed 's/^/     /'
    return 1
  fi
}

selected() {
  [[ $# -eq 1 ]] && return 0
  local name="$1"; shift
  for f in "$@"; do [[ "$name" == "$f"* ]] && return 0; done
  return 1
}

failed=()
passed=0
while read -r name scenario services props <&3; do
  [[ -z "$name" || "$name" == \#* ]] && continue
  selected "$name" "$@" || continue
  # shellcheck disable=SC2086
  if run_case "$name" "$scenario" "$services" $props; then
    passed=$((passed + 1))
  else
    failed+=("$name")
  fi
done 3<"$MATRIX/matrix.txt"

echo "$passed passed, ${#failed[@]} failed${failed[*]+: ${failed[*]}}"
[[ ${#failed[@]} -eq 0 ]]
