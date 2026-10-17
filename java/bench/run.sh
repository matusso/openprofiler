#!/usr/bin/env bash
# Agent overhead on demo applications (roadmap phase 3 exit criterion): Spring Petclinic and the
# Quarkus Hibernate ORM Panache quickstart, each run without the agent, with the agent's defaults
# (CPU, allocation and monitor sampling, all probes) and with call counting of the application's
# packages on top. Every configuration gets the same closed-loop load (LoadGen.java); rounds
# alternate the configurations. The recordings are checked for SQL and URL hot spots linked to
# call trees (<app>.expect, see crates/openprofiler-link/examples/oplink_check.rs).
#
#   java/bench/run.sh                    # both apps
#   java/bench/run.sh petclinic          # one
#   WARMUP=30 MEASURE=60 ROUNDS=3 THREADS=8 java/bench/run.sh
#
# Needs Docker (builds the apps with Maven, runs PostgreSQL for Quarkus) and a JDK 17+ (JAVA_HOME
# or `java` on the PATH). Apps are cloned and built in target/demos/; results go to target/bench/.
# java/bench/linux.sh runs the same on Linux in a container (the async sampler; AGENT, AGENT_EXTRA,
# OUT, PG_HOST and SKIP_CHECK are for that).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BENCH="$ROOT/java/bench"
DEMOS="$ROOT/target/demos"
OUT="${OUT:-$ROOT/target/bench}"
JAVA="${JAVA_HOME:+$JAVA_HOME/bin/}java"
MAVEN_IMAGE="maven:3.9-eclipse-temurin-21"
# As in java/matrix/run.sh: on Linux the user's ~/.m2, with Maven running as the user.
if [[ "$(uname -s)" == Linux ]]; then
  M2_VOLUME="${M2_VOLUME:-$HOME/.m2}"
  DOCKER_USER=(-u "$(id -u):$(id -g)")
else
  M2_VOLUME="${M2_VOLUME:-openprofiler-matrix-m2}"
  DOCKER_USER=()
fi
WARMUP="${WARMUP:-30}"
MEASURE="${MEASURE:-60}"
ROUNDS="${ROUNDS:-3}"
THREADS="${THREADS:-8}"
PORT=18080
mkdir -p "$DEMOS" "$OUT"

PG_HOST="${PG_HOST:-127.0.0.1}"
EXTRA="${AGENT_EXTRA:+,$AGENT_EXTRA}"
if [[ -z "${AGENT:-}" ]]; then
  case "$(uname -s)" in
    Darwin) AGENT="$ROOT/target/release/libopenprofiler_agent.dylib" ;;
    *) AGENT="$ROOT/target/release/libopenprofiler_agent.so" ;;
  esac
  (cd "$ROOT" && cargo build -q --release -p openprofiler-agent && cargo build -q -p openprofiler-link --example oplink_check)
fi
CHECK="$ROOT/target/debug/examples/oplink_check"

mvn_build() { # dir, args...
  local dir="$1"; shift
  docker run --rm "${DOCKER_USER[@]+"${DOCKER_USER[@]}"}" -v "$M2_VOLUME:/var/maven/.m2" -e MAVEN_CONFIG=/var/maven/.m2 \
    -v "$dir:/w" -w /w "$MAVEN_IMAGE" mvn -B -q -Duser.home=/var/maven package -DskipTests "$@"
}

# ---- apps ----------------------------------------------------------------------------------------

petclinic_build() {
  local d="$DEMOS/spring-petclinic"
  [[ -d "$d" ]] || git clone -q --depth 1 https://github.com/spring-projects/spring-petclinic.git "$d"
  compgen -G "$d/target/spring-petclinic-*.jar" >/dev/null || mvn_build "$d" -Dcheckstyle.skip -Dspring-javaformat.skip=true
}
petclinic_cmd() {
  echo "-Xmx1g -jar $(ls "$DEMOS"/spring-petclinic/target/spring-petclinic-*.jar | head -1) --server.port=$PORT"
}
petclinic_instrument() { echo "org.springframework.samples.petclinic.*"; }
petclinic_urls() {
  local b="http://127.0.0.1:$PORT"
  for i in 1 2 3 4 5 6 7 8 9 10; do echo "$b/owners/$i"; done
  echo "$b/owners?lastName=Davis" "$b/owners?page=1" "$b/vets.html" "$b/owners/find"
}

quarkus_build() {
  local d="$DEMOS/quarkus-quickstarts"
  [[ -d "$d" ]] || git clone -q --depth 1 https://github.com/quarkusio/quarkus-quickstarts.git "$d"
  [[ -f "$d/hibernate-orm-panache-quickstart/target/quarkus-app/quarkus-run.jar" ]] || mvn_build "$d/hibernate-orm-panache-quickstart"
  if command -v docker >/dev/null && ! docker inspect opmatrix-postgres >/dev/null 2>&1; then
    docker run -d --name opmatrix-postgres -p 55432:5432 -e POSTGRES_PASSWORD=matrix -e POSTGRES_USER=matrix \
      -e POSTGRES_DB=matrix postgres:16-alpine >/dev/null
    for _ in $(seq 1 60); do docker exec opmatrix-postgres pg_isready -U matrix >/dev/null 2>&1 && break; sleep 1; done
  fi
}
quarkus_cmd() {
  echo "-Xmx1g -Dquarkus.http.port=$PORT -Dquarkus.datasource.jdbc.url=jdbc:postgresql://$PG_HOST:55432/matrix" \
    "-Dquarkus.datasource.username=matrix -Dquarkus.datasource.password=matrix" \
    "-Dquarkus.hibernate-orm.schema-management.strategy=drop-and-create -Dquarkus.hibernate-orm.log.sql=false" \
    "-jar $DEMOS/quarkus-quickstarts/hibernate-orm-panache-quickstart/target/quarkus-app/quarkus-run.jar"
}
quarkus_instrument() { echo "org.acme.*"; }
quarkus_urls() {
  local b="http://127.0.0.1:$PORT"
  echo "$b/entity/fruits" "$b/entity/fruits/1" "$b/entity/fruits/2" "$b/repository/fruits" "$b/repository/fruits/3"
}

# ---- runs ----------------------------------------------------------------------------------------

cpu_seconds() { # pid → CPU time of the process in seconds
  if [[ -r "/proc/$1/stat" ]]; then
    # utime and stime (fields 14 and 15, after the parenthesised command name) in clock ticks.
    awk -v hz="$(getconf CLK_TCK)" '{ sub(/.*\) /, ""); print ($12 + $13) / hz }' "/proc/$1/stat"
  else
    ps -o time= -p "$1" | awk -F: '{ s = 0; for (i = 1; i <= NF; i++) s = s * 60 + $i; print s }'
  fi
}

run_one() { # app config round → prints "rps p50 p90 p99 errors cpu_ms_per_request"
  local app="$1" config="$2" round="$3" agent=()
  local rec="$OUT/$app-$config.oplink"
  case "$config" in
    baseline) ;;
    probes) agent=("-agentpath:$AGENT=mode=offline,file=$rec$EXTRA") ;;
    instrument) agent=("-agentpath:$AGENT=mode=offline,file=$rec,instrument=$("${app}_instrument")$EXTRA") ;;
    # Experiments: CONFIGS="baseline custom" OPTS="probes=none" (agent options added to offline mode).
    custom) agent=("-agentpath:$AGENT=mode=offline,file=$rec${OPTS:+,$OPTS}$EXTRA") ;;
  esac
  rm -f "$rec"
  # shellcheck disable=SC2046
  "$JAVA" "${agent[@]+"${agent[@]}"}" $("${app}_cmd") >"$OUT/$app-$config-$round.log" 2>&1 &
  local pid=$!
  local urls
  read -r -a urls <<<"$("${app}_urls" | tr '\n' ' ')"
  for _ in $(seq 1 120); do
    curl -fs -o /dev/null "${urls[0]}" && break
    sleep 0.5
  done
  "$JAVA" "$BENCH/LoadGen.java" "$WARMUP" "$THREADS" "${urls[@]}" >/dev/null
  local cpu0 cpu1 result
  cpu0="$(cpu_seconds "$pid")"
  result="$("$JAVA" "$BENCH/LoadGen.java" "$MEASURE" "$THREADS" "${urls[@]}")"
  cpu1="$(cpu_seconds "$pid")"
  kill "$pid"
  wait "$pid" || true
  local requests
  requests="$(sed -E 's/.*requests=([0-9]+).*/\1/' <<<"$result")"
  echo "$result cpu_ms_per_request=$(awk -v a="$cpu0" -v b="$cpu1" -v n="$requests" 'BEGIN { printf "%.4f", (b - a) * 1000 / n }')"
}

report() { # app → table of the medians over rounds, with changes against the baseline
  awk -v app="$1" -v threads="$THREADS" -v measure="$MEASURE" -v rounds="$ROUNDS" '
    function val(line, key) {
      return match(line, key "=[0-9.]+") ? substr(line, RSTART + length(key) + 1, RLENGTH - length(key) - 1) : ""
    }
    function median(list,   a, n, i, j, t) {
      n = split(list, a, " ")
      for (i = 2; i <= n; i++) for (j = i; j > 1 && a[j - 1] + 0 > a[j] + 0; j--) { t = a[j]; a[j] = a[j - 1]; a[j - 1] = t }
      return n ? a[int((n + 1) / 2)] : ""
    }
    $1 == app {
      if (!($2 in seen)) { seen[$2] = 1; order[++configs] = $2 }
      split("rps p50 p90 p99 cpu_ms_per_request", keys, " ")
      for (k = 1; k <= 5; k++) values[$2, keys[k]] = values[$2, keys[k]] " " val($0, keys[k])
    }
    END {
      if (!configs) exit
      printf "\n%s (%s threads, %ss measured, median of %s rounds)\n", app, threads, measure, rounds
      printf "%-11s %10s %8s %8s %8s %12s %9s %10s\n", "config", "req/s", "p50 ms", "p90 ms", "p99 ms", "CPU ms/req", "Δ req/s", "Δ CPU/req"
      base_rps = median(values["baseline", "rps"]); base_cpu = median(values["baseline", "cpu_ms_per_request"])
      for (c = 1; c <= configs; c++) {
        cfg = order[c]
        rps = median(values[cfg, "rps"]); cpu = median(values[cfg, "cpu_ms_per_request"])
        printf "%-11s %10s %8s %8s %8s %12s", cfg, rps, median(values[cfg, "p50"]), median(values[cfg, "p90"]),
          median(values[cfg, "p99"]), cpu
        if (base_rps > 0 && base_cpu > 0) printf " %+8.1f%% %+9.1f%%", (rps / base_rps - 1) * 100, (cpu / base_cpu - 1) * 100
        printf "\n"
      }
    }' "$OUT/results.txt"
}

apps=("$@")
[[ ${#apps[@]} -eq 0 ]] && apps=(petclinic quarkus)
: >"$OUT/results.txt"
for app in "${apps[@]}"; do
  "${app}_build"
  [[ -n "${BUILD_ONLY:-}" ]] && continue
  for round in $(seq 1 "$ROUNDS"); do
    for config in ${CONFIGS:-baseline probes instrument}; do
      r="$(run_one "$app" "$config" "$round")"
      echo "$app $config $round $r" | tee -a "$OUT/results.txt"
    done
  done
  for config in ${CONFIGS:-probes instrument}; do
    [[ "$config" == baseline || "$config" == custom || -n "${SKIP_CHECK:-}" ]] && continue
    if out="$("$CHECK" "$OUT/$app-$config.oplink" "$BENCH/$app.expect" 2>&1)"; then
      echo "$app $config recording: $out"
    else
      echo "$app $config recording: FAIL"
      echo "$out" | sed 's/^/     /'
    fi
  done
done
[[ -n "${BUILD_ONLY:-}" ]] && exit 0
for app in "${apps[@]}"; do report "$app"; done | tee "$OUT/summary.txt"
