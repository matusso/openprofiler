#!/usr/bin/env bash
# Soak test (roadmap phase 2 exit criterion: 24 h without leaks or crashes). Runs a demo app
# (apps.sh, Spring Petclinic by default) with the agent's defaults in live mode under constant
# closed-loop load (LoadGen.java, in 10 min chunks) while a client (the soak_client example)
# connects, receives and acknowledges the agent's data, takes a thread dump and disconnects, over
# and over. Every minute it records the JVM's resident memory, threads and open files.
#
#   java/bench/soak.sh                          # 24 h, Petclinic
#   SOAK_HOURS=0.25 java/bench/soak.sh quarkus  # a quick check
#
# Fails when the JVM dies or writes an hs_err file, a client cycle fails, resident memory grows
# by more than MAX_RSS_GROWTH MB/h (default 8, a least-squares fit after the warm-up: the first
# hour, or a quarter of shorter runs) or open files grow by more than MAX_FD_GROWTH (default
# 50). The heap is fixed and pre-touched (-Xms1g -Xmx1g -XX:+AlwaysPreTouch) so that growth is
# native memory. Results go to target/soak/: metrics.tsv, load.txt, client.txt, summary.txt and
# bench.json (for github-action-benchmark). Needs what run.sh needs.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BENCH="$ROOT/java/bench"
DEMOS="$ROOT/target/demos"
OUT="${OUT:-$ROOT/target/soak}"
JAVA="${JAVA_HOME:+$JAVA_HOME/bin/}java"
SOAK_HOURS="${SOAK_HOURS:-24}"
THREADS="${THREADS:-4}"
CYCLE="${CYCLE:-300}"
MAX_RSS_GROWTH="${MAX_RSS_GROWTH:-8}"
MAX_FD_GROWTH="${MAX_FD_GROWTH:-50}"
APP="${1:-petclinic}"
mkdir -p "$DEMOS" "$OUT"
rm -f "$OUT"/*
# shellcheck source=java/bench/apps.sh
source "$BENCH/apps.sh"
EXTRA="${AGENT_EXTRA:+,$AGENT_EXTRA}"
if [[ -z "${AGENT:-}" ]]; then
  case "$(uname -s)" in
    Darwin) AGENT="$ROOT/target/release/libopenprofiler_agent.dylib" ;;
    *) AGENT="$ROOT/target/release/libopenprofiler_agent.so" ;;
  esac
  (cd "$ROOT" && cargo build -q --release -p openprofiler-agent)
fi
(cd "$ROOT" && cargo build -q --release -p openprofiler-link --example soak_client)
CLIENT="$ROOT/target/release/examples/soak_client"

"${APP}_build"
seconds="$(awk -v h="$SOAK_HOURS" 'BEGIN { printf "%d", h * 3600 }')"
warmup="$(awk -v s="$seconds" 'BEGIN { w = s / 4; printf "%d", (w < 3600 ? w : 3600) }')"

# shellcheck disable=SC2046
"$JAVA" -Xms1g -Xmx1g -XX:+AlwaysPreTouch -XX:ErrorFile="$OUT/hs_err_pid%p.log" \
  "-agentpath:$AGENT${EXTRA:+=${EXTRA#,}}" $("${APP}_cmd") >"$OUT/app.log" 2>&1 &
pid=$!
read -r -a urls <<<"$("${APP}_urls" | tr '\n' ' ')"
for _ in $(seq 1 240); do
  curl -fs -o /dev/null "${urls[0]}" && break
  sleep 0.5
done
start=$(date +%s)
end=$((start + seconds))
echo "soak: $APP (pid $pid) for ${SOAK_HOURS} h, warm-up ${warmup} s, results in $OUT"

(
  while (($(date +%s) < end)); do
    left=$((end - $(date +%s)))
    if r="$("$CLIENT" "$pid" $((left < CYCLE ? left : CYCLE)) 2>&1)"; then
      echo "$(date +%s) ok $r"
    else
      echo "$(date +%s) FAIL $(tr '\n' ' ' <<<"$r")"
      sleep 5
    fi
  done >>"$OUT/client.txt"
) &
client_loop=$!
(
  while (($(date +%s) < end)); do
    left=$((end - $(date +%s)))
    ((left < 1)) && break
    "$JAVA" "$BENCH/LoadGen.java" $((left < 600 ? left : 600)) "$THREADS" "${urls[@]}"
  done >>"$OUT/load.txt"
) &
load_loop=$!

metrics() { # pid → "rss_kb threads fds"
  if [[ -r "/proc/$1/status" ]]; then
    echo "$(awk '/^VmRSS:/ { r = $2 } /^Threads:/ { t = $2 } END { print r, t }' "/proc/$1/status") $(ls "/proc/$1/fd" | wc -l)"
  else
    echo "$(ps -o rss= -p "$1") $(($(ps -M -p "$1" | wc -l) - 1)) $(lsof -p "$1" 2>/dev/null | wc -l)"
  fi | awk '{ print $1, $2, $3 }'
}
printf "elapsed_s\trss_kb\tthreads\tfds\n" >"$OUT/metrics.tsv"
died=""
while (($(date +%s) < end)); do
  if ! kill -0 "$pid" 2>/dev/null; then
    died="JVM exited after $(($(date +%s) - start)) s"
    break
  fi
  read -r rss threads fds <<<"$(metrics "$pid")"
  printf "%s\t%s\t%s\t%s\n" $(($(date +%s) - start)) "$rss" "$threads" "$fds" >>"$OUT/metrics.tsv"
  sleep 60
done
wait "$client_loop" "$load_loop" || true
kill "$pid" 2>/dev/null || true
wait "$pid" || true

failures=()
[[ -n "$died" ]] && failures+=("$died")
compgen -G "$OUT/hs_err_pid*.log" >/dev/null && failures+=("hs_err file written")
cycles="$(grep -c ' ok ' "$OUT/client.txt" || true)"
failed="$(grep -c ' FAIL ' "$OUT/client.txt" || true)"
((failed > 0)) && failures+=("$failed of $((cycles + failed)) client cycles failed")
((cycles == 0)) && failures+=("no client cycle completed")
# Least-squares slope of RSS (MB/h) and growth of threads and open files after the warm-up.
read -r slope rss_first rss_last thr_min thr_last fd_min fd_last <<<"$(awk -v w="$warmup" '
  NR > 1 && $1 >= w {
    x = $1 / 3600; y = $2 / 1024; n++; sx += x; sy += y; sxx += x * x; sxy += x * y
    if (n == 1) { first = y; tmin = $3; fmin = $4 }
    if ($3 < tmin) tmin = $3
    if ($4 < fmin) fmin = $4
    last = y; tlast = $3; flast = $4
  }
  END {
    d = n * sxx - sx * sx
    printf "%.2f %.0f %.0f %d %d %d %d\n", (n > 2 && d > 0) ? (n * sxy - sx * sy) / d : 0, first, last, tmin, tlast, fmin, flast
  }' "$OUT/metrics.tsv")"
awk -v s="$slope" -v m="$MAX_RSS_GROWTH" 'BEGIN { exit !(s > m) }' &&
  failures+=("resident memory grows by $slope MB/h (limit $MAX_RSS_GROWTH)")
((fd_last - fd_min > MAX_FD_GROWTH)) && failures+=("open files grew from $fd_min to $fd_last")
load="$(awk '
  { for (i = 1; i <= NF; i++) { split($i, kv, "="); v[kv[1]] = kv[2] }
    n++; req += v["requests"]; err += v["errors"]; if (n == 1) first = v["rps"]; last = v["rps"] }
  END { printf "%d requests, %d errors, %s req/s in the first chunk, %s in the last", req, err, first, last }' "$OUT/load.txt")"
{
  echo "soak: $APP, ${SOAK_HOURS} h, $THREADS load threads, agent defaults (live mode)"
  echo "load: $load"
  echo "client: $cycles cycles of up to ${CYCLE} s, $failed failed"
  echo "after ${warmup} s warm-up: RSS ${rss_first} → ${rss_last} MB (${slope} MB/h), threads ${thr_min} → ${thr_last}, open files ${fd_min} → ${fd_last}"
  if ((${#failures[@]})); then
    printf "FAIL: %s\n" "${failures[@]}"
  else
    echo "PASS"
  fi
} | tee "$OUT/summary.txt"
cat >"$OUT/bench.json" <<EOF
[
  { "name": "soak $APP: RSS growth", "unit": "MB/h", "value": $slope },
  { "name": "soak $APP: open files growth", "unit": "files", "value": $((fd_last - fd_min)) },
  { "name": "soak $APP: failed client cycles", "unit": "cycles", "value": $failed }
]
EOF
((${#failures[@]} == 0))
