#!/usr/bin/env bash
# Agent overhead on Renaissance and DaCapo (roadmap phase 2 exit criterion: at most 2% at 100 Hz
# async sampling). Every benchmark runs in its own JVM without the agent ("baseline") and with it
# ("cpu": CPU sampling every 10 ms with the async sampler, no allocation or monitor recording,
# no probes or instrumentation; "defaults": the agent's defaults), ROUNDS times, alternating the
# configurations. A run's time is the median of the second half of its iterations (steady
# state); a benchmark's overhead is the ratio of the medians over rounds, and a suite's the
# geometric mean over its benchmarks. Fails when a suite's "cpu" overhead exceeds MAX_OVERHEAD
# percent (default 2; empty to only report).
#
#   java/bench/suites.sh                            # both suites
#   java/bench/suites.sh dacapo                     # one
#   RENAISSANCE="scala-doku dotty" ROUNDS=1 java/bench/suites.sh renaissance
#   CONFIGS="baseline cpu defaults" java/bench/suites.sh
#
# The async sampler is Linux-only: elsewhere the script builds the Linux agent with a Rust
# container and runs itself in an eclipse-temurin container (JDK_IMAGE). On Linux it needs a
# JDK 21 (JAVA_HOME or `java` on the PATH). The suites are downloaded to target/bench-suites/cache
# (Renaissance MIT 0.16.2 and DaCapo 23.11-MR2 chopin minimal, 1.9 GB together); results go to
# target/bench-suites/: results.txt, summary.txt and bench.json (for github-action-benchmark).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="${OUT:-$ROOT/target/bench-suites}"
CACHE="$ROOT/target/bench-suites/cache"
JAVA="${JAVA_HOME:+$JAVA_HOME/bin/}java"
ROUNDS="${ROUNDS:-3}"
CONFIGS="${CONFIGS:-baseline cpu}"
MAX_OVERHEAD="${MAX_OVERHEAD-2}"
JVM_OPTS="${JVM_OPTS:--Xms2g -Xmx2g}"
# Iterations per run (Renaissance repetitions, DaCapo iterations).
RENAISSANCE_REPS="${RENAISSANCE_REPS:-20}"
DACAPO_ITERATIONS="${DACAPO_ITERATIONS:-10}"
RENAISSANCE="${RENAISSANCE:-akka-uct chi-square dotty finagle-http fj-kmeans future-genetic mnemonics page-rank reactors scala-doku}"
DACAPO="${DACAPO:-avrora batik biojava fop h2 jython luindex lusearch pmd sunflow xalan zxing}"
RENAISSANCE_JAR="renaissance-mit-0.16.2.jar"
RENAISSANCE_URL="https://github.com/renaissance-benchmarks/renaissance/releases/download/v0.16.2/$RENAISSANCE_JAR"
DACAPO_NAME="dacapo-23.11-MR2-chopin-minimal"
DACAPO_URL="https://download.dacapobench.org/chopin/$DACAPO_NAME.zip"
suites=("$@")
[[ ${#suites[@]} -eq 0 ]] && suites=(renaissance dacapo)
mkdir -p "$CACHE" "$OUT"

[[ -f "$CACHE/$RENAISSANCE_JAR" ]] || curl -fsSL -o "$CACHE/$RENAISSANCE_JAR" "$RENAISSANCE_URL"
if [[ ! -f "$CACHE/$DACAPO_NAME.jar" ]]; then
  curl -fsSL -o "$CACHE/$DACAPO_NAME.zip" "$DACAPO_URL"
  (cd "$CACHE" && unzip -q -o "$DACAPO_NAME.zip" && rm "$DACAPO_NAME.zip")
fi

if [[ "$(uname -s)" != Linux ]]; then
  JDK_IMAGE="${JDK_IMAGE:-eclipse-temurin:21-jdk}"
  docker run --rm -v openprofiler-cargo:/usr/local/cargo/registry -v "$ROOT:/repo" -w /repo \
    -e CARGO_TARGET_DIR=/repo/target/linux rust:1-bookworm cargo build -q --release -p openprofiler-agent
  "$ROOT/java/build.sh" >/dev/null
  exec docker run --rm -v "$ROOT:/repo" -w /repo \
    -e AGENT=/repo/target/linux/release/libopenprofiler_agent.so \
    -e AGENT_EXTRA=bootjar=/repo/java/openprofiler-boot/build/openprofiler-boot.jar \
    -e OUT="/repo/${OUT#"$ROOT"/}" -e ROUNDS="$ROUNDS" -e CONFIGS="$CONFIGS" -e MAX_OVERHEAD="$MAX_OVERHEAD" \
    -e JVM_OPTS="$JVM_OPTS" -e RENAISSANCE_REPS="$RENAISSANCE_REPS" -e DACAPO_ITERATIONS="$DACAPO_ITERATIONS" \
    -e RENAISSANCE="$RENAISSANCE" -e DACAPO="$DACAPO" \
    "$JDK_IMAGE" java/bench/suites.sh "${suites[@]}"
fi

EXTRA="${AGENT_EXTRA:+,$AGENT_EXTRA}"
if [[ -z "${AGENT:-}" ]]; then
  AGENT="$ROOT/target/release/libopenprofiler_agent.so"
  (cd "$ROOT" && cargo build -q --release -p openprofiler-agent)
fi

run_one() { # suite bench config round → prints the steady-state time in ms
  local suite="$1" bench="$2" config="$3" round="$4" agent=() cmd=()
  local log="$OUT/logs/$suite-$bench-$config-$round.log" rec="$OUT/logs/$suite-$bench-$config.oplink"
  case "$config" in
    baseline) ;;
    cpu) agent=("-agentpath:$AGENT=mode=offline,file=$rec,sampler=async,interval=10ms,alloc=0,monitors=0,probes=none,noinstrument$EXTRA") ;;
    defaults) agent=("-agentpath:$AGENT=mode=offline,file=$rec$EXTRA") ;;
    *) echo "unknown config $config" >&2; return 1 ;;
  esac
  case "$suite" in
    renaissance) cmd=(-jar "$CACHE/$RENAISSANCE_JAR" -r "$RENAISSANCE_REPS" --no-forced-gc "$bench") ;;
    dacapo) cmd=(-jar "$CACHE/$DACAPO_NAME.jar" -n "$DACAPO_ITERATIONS" --scratch-directory "$OUT/scratch" "$bench") ;;
  esac
  rm -f "$rec"
  # shellcheck disable=SC2086
  if ! (cd "$OUT" && "$JAVA" $JVM_OPTS "${agent[@]+"${agent[@]}"}" "${cmd[@]}") >"$log" 2>&1; then
    echo "$suite $bench ($config) failed, see $log" >&2
    return 1
  fi
  if [[ "$config" != baseline && ! -s "$rec" ]]; then
    echo "$suite $bench ($config): the agent wrote no recording, see $log" >&2
    return 1
  fi
  # Iteration times: Renaissance "iteration 3 completed (123.456 ms)", DaCapo "completed warmup 3
  # in 1234 msec" and the final "PASSED in 1234 msec".
  sed -nE 's/.*iteration [0-9]+ completed \(([0-9.]+) ms\).*/\1/p; s/.*(completed warmup [0-9]+|PASSED) in ([0-9]+) msec.*/\2/p' "$log" |
    awk '{ t[++n] = $1 }
      END {
        if (n == 0) exit 1
        m = 0; for (i = int(n / 2) + 1; i <= n; i++) s[++m] = t[i]
        for (i = 2; i <= m; i++) for (j = i; j > 1 && s[j - 1] + 0 > s[j] + 0; j--) { x = s[j]; s[j] = s[j - 1]; s[j - 1] = x }
        printf "%.3f\n", (m % 2) ? s[(m + 1) / 2] : (s[m / 2] + s[m / 2 + 1]) / 2
      }'
}

report() { # suite → per-benchmark table and the suite's geometric mean; appends to bench.json
  awk -v suite="$1" -v rounds="$ROUNDS" -v configs="$CONFIGS" '
    function median(list,   a, n, i, j, t) {
      n = split(list, a, " ")
      for (i = 2; i <= n; i++) for (j = i; j > 1 && a[j - 1] + 0 > a[j] + 0; j--) { t = a[j]; a[j] = a[j - 1]; a[j - 1] = t }
      return (n % 2) ? a[(n + 1) / 2] : (a[n / 2] + a[n / 2 + 1]) / 2
    }
    $1 == suite {
      if (!($2 in seen)) { seen[$2] = 1; order[++benches] = $2 }
      times[$2, $3] = times[$2, $3] " " $5
    }
    END {
      if (!benches) exit
      nc = split(configs, cfg, " ")
      printf "\n%s (median of %s rounds, steady-state ms per iteration)\n%-16s", suite, rounds, "benchmark"
      for (c = 1; c <= nc; c++) printf " %12s", cfg[c]
      for (c = 1; c <= nc; c++) if (cfg[c] != "baseline") printf " %10s", "Δ " cfg[c]
      printf "\n"
      for (b = 1; b <= benches; b++) {
        name = order[b]; printf "%-16s", name
        for (c = 1; c <= nc; c++) { med[name, cfg[c]] = median(times[name, cfg[c]]); printf " %12.1f", med[name, cfg[c]] }
        for (c = 1; c <= nc; c++) {
          if (cfg[c] == "baseline") continue
          r = med[name, cfg[c]] / med[name, "baseline"]
          logsum[cfg[c]] += log(r); printf " %+9.1f%%", (r - 1) * 100
          printf "{ \"name\": \"%s %s: %s\", \"unit\": \"× baseline\", \"value\": %.4f }\n", suite, name, cfg[c], r > "/dev/stderr"
        }
        printf "\n"
      }
      printf "%-16s", "geometric mean"
      for (c = 1; c <= nc; c++) printf " %12s", ""
      for (c = 1; c <= nc; c++) {
        if (cfg[c] == "baseline") continue
        g = (exp(logsum[cfg[c]] / benches) - 1) * 100
        printf " %+9.1f%%", g
        printf "{ \"name\": \"%s: %s (geometric mean)\", \"unit\": \"× baseline\", \"value\": %.4f }\n", suite, cfg[c], 1 + g / 100 > "/dev/stderr"
        printf "%s %s %.2f\n", suite, cfg[c], g > "'"$OUT/geomean.txt"'"
      }
      printf "\n"
    }' "$OUT/results.txt" 2>>"$OUT/bench.lines"
}

mkdir -p "$OUT/logs" "$OUT/scratch"
: >"$OUT/results.txt"
: >"$OUT/bench.lines"
: >"$OUT/geomean.txt"
"$JAVA" -version 2>&1 | head -1
for suite in "${suites[@]}"; do
  case "$suite" in
    renaissance) benches="$RENAISSANCE" ;;
    dacapo) benches="$DACAPO" ;;
    *) echo "unknown suite $suite (renaissance, dacapo)" >&2; exit 1 ;;
  esac
  for bench in $benches; do
    for round in $(seq 1 "$ROUNDS"); do
      for config in $CONFIGS; do
        t="$(run_one "$suite" "$bench" "$config" "$round")"
        echo "$suite $bench $config $round $t" | tee -a "$OUT/results.txt"
      done
    done
  done
done
for suite in "${suites[@]}"; do report "$suite"; done | tee "$OUT/summary.txt"
(echo "["; paste -sd, "$OUT/bench.lines"; echo "]") >"$OUT/bench.json"
rm -f "$OUT/bench.lines"
[[ -z "$MAX_OVERHEAD" ]] && exit 0
status=0
while read -r suite config g; do
  [[ "$config" == cpu ]] || continue
  if awk -v g="$g" -v m="$MAX_OVERHEAD" 'BEGIN { exit !(g > m) }'; then
    echo "FAIL: $suite overhead at 100 Hz async sampling is $g% (limit $MAX_OVERHEAD%)" | tee -a "$OUT/summary.txt"
    status=1
  fi
done <"$OUT/geomean.txt"
exit "$status"
