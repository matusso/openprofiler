#!/usr/bin/env bash
# Heap engine benchmark: writes synthetic HPROF dumps of the given sizes (the gen_hprof example),
# indexes each from scratch and reopens it warm (the analyze example), and records the times,
# throughput and peak resident memory.
#
#   crates/openprofiler-heap/scripts/bench.sh            # 1 GB
#   crates/openprofiler-heap/scripts/bench.sh 1 10 50    # the tracked sizes
#
# Dumps are kept in target/heap-bench/ (HEAP_BENCH_DIR) for later runs; their indexes are removed
# after each run. Writes summary.txt, bench.json (for github-action-benchmark) and appends one
# JSON line per dump to history.jsonl. A 10 GB dump must open in under 3 min cold and under 5 s
# warm (roadmap phase 1 exit criterion); the script fails otherwise.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
DIR="${HEAP_BENCH_DIR:-$ROOT/target/heap-bench}"
sizes=("$@")
[[ ${#sizes[@]} -eq 0 ]] && sizes=(1)
mkdir -p "$DIR"
(cd "$ROOT" && cargo build -q --release -p openprofiler-heap --example gen_hprof --example analyze)
GEN="$ROOT/target/release/examples/gen_hprof"
ANALYZE="$ROOT/target/release/examples/analyze"
commit="$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"

peak_rss() { # time output → peak RSS in MB
  awk '/Maximum resident set size/ { print int($NF / 1024) } /maximum resident set size/ { print int($1 / 1048576) }' "$1"
}
run_timed() { # time output file, command...
  local out="$1"; shift
  case "$(uname -s)" in
    Darwin) /usr/bin/time -l "$@" 2>"$out" ;;
    *) if [[ -x /usr/bin/time ]]; then /usr/bin/time -v "$@" 2>"$out"; else "$@" 2>"$out"; fi ;;
  esac
}

failures=()
json=()
: >"$DIR/summary.txt"
for gb in "${sizes[@]}"; do
  dump="$DIR/synthetic-${gb}g.hprof"
  [[ -f "$dump" ]] || "$GEN" "$dump" "$gb"
  rm -rf "$dump.openprofiler-idx"
  run_timed "$DIR/time-${gb}g.txt" "$ANALYZE" "$dump" --rebuild >"$DIR/analyze-${gb}g.txt"
  rm -rf "$dump.openprofiler-idx"
  out="$DIR/analyze-${gb}g.txt"
  cold="$(sed -nE 's/^open: ([0-9.]+)s.*/\1/p' "$out")"
  warm="$(sed -nE 's/^warm reopen: ([0-9.]+)s.*/\1/p' "$out")"
  objects="$(sed -nE 's/^open: .*, ([0-9]+) objects,.*/\1/p' "$out")"
  mbps="$(sed -nE 's/^open: .* ([0-9]+) MB\/s\)$/\1/p' "$out")"
  rss="$(peak_rss "$DIR/time-${gb}g.txt")"
  bytes="$(wc -c <"$dump" | tr -d ' ')"
  line="$(printf "%5s GB dump: %s objects, cold open %ss (%s MB/s), warm reopen %ss, peak RSS %s MB" \
    "$gb" "$objects" "$cold" "$mbps" "$warm" "${rss:-?}")"
  echo "$line" | tee -a "$DIR/summary.txt"
  printf '{"date":"%s","commit":"%s","gb":%s,"bytes":%s,"objects":%s,"cold_s":%s,"warm_s":%s,"mb_per_s":%s,"peak_rss_mb":%s}\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$commit" "$gb" "$bytes" "$objects" "$cold" "$warm" "$mbps" "${rss:-null}" \
    >>"$DIR/history.jsonl"
  json+=("{ \"name\": \"heap ${gb} GB: cold open\", \"unit\": \"s\", \"value\": $cold }")
  json+=("{ \"name\": \"heap ${gb} GB: warm reopen\", \"unit\": \"s\", \"value\": $warm }")
  [[ -n "$rss" ]] && json+=("{ \"name\": \"heap ${gb} GB: peak RSS\", \"unit\": \"MB\", \"value\": $rss }")
  if [[ "$gb" == 10 ]]; then
    awk -v c="$cold" -v w="$warm" 'BEGIN { exit !(c >= 180 || w >= 5) }' &&
      failures+=("10 GB dump: cold open ${cold}s (limit 180s), warm reopen ${warm}s (limit 5s)")
  fi
done
(IFS=,; echo "[${json[*]}]") >"$DIR/bench.json"
if ((${#failures[@]})); then
  printf "FAIL: %s\n" "${failures[@]}" | tee -a "$DIR/summary.txt"
  exit 1
fi
