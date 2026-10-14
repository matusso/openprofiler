#!/usr/bin/env bash
# Compares OpenProfiler's heap analysis with Eclipse MAT on one HPROF dump.
#
#   crates/openprofiler-heap/scripts/mat-parity.sh <dump.hprof> [mat_dir]
#
# MAT (an unpacked Memory Analyzer distribution, default $MAT_HOME) runs headless inside the
# eclipse-temurin:21-jdk Docker image. The script checks, exactly:
#   - reachable object count and total reachable shallow size
#   - shallow and retained size of every reachable object
#   - per-class histogram (instances and shallow size)
#   - the top 50 entries of the dominator tree
# Exit status 0 = parity, 1 = differences, 2 = usage / missing tools.
set -euo pipefail

dump=${1:?usage: mat-parity.sh <dump.hprof> [mat_dir]}
mat=${2:-${MAT_HOME:-}}
[[ -n "$mat" && -x "$mat/ParseHeapDump.sh" ]] || { echo "MAT not found (pass mat_dir or set MAT_HOME)" >&2; exit 2; }
command -v docker >/dev/null && docker info >/dev/null 2>&1 || { echo "docker is not available" >&2; exit 2; }
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 2; }
# Homebrew's rustup is keg-only; use its proxies when cargo is not on PATH.
command -v cargo >/dev/null || [[ ! -d /opt/homebrew/opt/rustup/bin ]] || PATH="/opt/homebrew/opt/rustup/bin:$PATH"
command -v cargo >/dev/null || { echo "cargo is required" >&2; exit 2; }

root=$(cd "$(dirname "$0")/../../.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/mat-parity.XXXXXX")
trap 'rm -rf "$work"' EXIT
cp "$dump" "$work/x.hprof"

run_mat() { # <name> <command>
  rm -rf "$work/x_Query" "$work/x_Query.zip"
  docker run --rm -v "$mat":/mat:ro -v "$work":/work -w /work eclipse-temurin:21-jdk \
    /mat/ParseHeapDump.sh /work/x.hprof "-command=$2" -format=csv -limit=100000000 -unzip \
    org.eclipse.mat.api:query >"$work/mat-$1.log" 2>&1
  cp "$work"/x_Query/pages/*.csv "$work/mat-$1.csv"
}
echo "running MAT…" >&2
run_mat objects 'oql "SELECT toHex(o.@objectAddress), o.@usedHeapSize, o.@retainedHeapSize FROM INSTANCEOF java.lang.Object o"'
run_mat histogram histogram
run_mat dominators dominator_tree

echo "running OpenProfiler…" >&2
(cd "$root" && CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target/heap} cargo run -q --release -p openprofiler-heap \
  --example parity_export -- "$work/x.hprof" "$work/openprofiler")

python3 - "$work" <<'PY'
import csv, sys, collections
work = sys.argv[1]
def rows(path):
    with open(path, newline='') as f:
        r = list(csv.reader(f))
    return r[1:]  # MAT CSVs have a header
def num(s): return int(s.replace(',', '').strip() or 0)
fail = []

mat = {int(r[0], 16): (num(r[1]), num(r[2])) for r in rows(f"{work}/mat-objects.csv")}
emb = {}
for line in open(f"{work}/openprofiler/objects.csv"):
    a, s, r = line.strip().split(',')
    emb[int(a, 16)] = (int(s), int(r))
print(f"objects:   MAT {len(mat):,}  OpenProfiler {len(emb):,}")
print(f"shallow:   MAT {sum(v[0] for v in mat.values()):,}  OpenProfiler {sum(v[0] for v in emb.values()):,}")
if set(mat) != set(emb):
    fail.append(f"object sets differ: {len(set(mat)-set(emb))} only in MAT, {len(set(emb)-set(mat))} only in OpenProfiler")
bad_s = [a for a in set(mat) & set(emb) if mat[a][0] != emb[a][0]]
bad_r = [a for a in set(mat) & set(emb) if mat[a][1] != emb[a][1]]
print(f"per-object shallow mismatches: {len(bad_s)}, retained mismatches: {len(bad_r)}")
for a in (bad_s + bad_r)[:10]:
    print(f"  0x{a:x}: MAT {mat[a]} OpenProfiler {emb[a]}")
if bad_s or bad_r: fail.append("per-object sizes differ")

def hist(path, skip_header):
    h = collections.Counter(); n = collections.Counter()
    with open(path, newline='') as f:
        r = list(csv.reader(f))
    for row in (r[1:] if skip_header else r):
        if not row or not row[0]: continue
        n[row[0]] += num(row[1]); h[row[0]] += num(row[2])
    return n, h
mn, ms = hist(f"{work}/mat-histogram.csv", True)
en, es = hist(f"{work}/openprofiler/histogram.csv", False)
hd = [c for c in set(mn) | set(en) if (mn[c], ms[c]) != (en[c], es[c])]
print(f"histogram: {len(mn)} MAT classes, {len(en)} OpenProfiler classes, {len(hd)} differences")
for c in sorted(hd)[:10]:
    print(f"  {c}: MAT {mn[c]}/{ms[c]}  OpenProfiler {en[c]}/{es[c]}")
if hd: fail.append("histogram differs")

md = []
for row in rows(f"{work}/mat-dominators.csv"):
    if not row or '@ 0x' not in row[0]: continue
    addr = int(row[0].split('@ 0x')[1].split()[0], 16)
    md.append((addr, num(row[1]), num(row[2])))
ed = []
for line in open(f"{work}/openprofiler/dominators.csv"):
    p = line.strip().split(',')
    ed.append((int(p[0], 16), int(p[-2]), int(p[-1])))
k = min(50, len(md))
mtop = sorted(md, key=lambda x: (-x[2], x[0]))[:k]
etop = sorted(ed, key=lambda x: (-x[2], x[0]))[:k]
same = mtop == etop
print(f"top-{k} dominators identical: {same}")
if not same:
    for m, e in zip(mtop, etop):
        if m != e: print(f"  MAT 0x{m[0]:x} {m[1]} {m[2]}  |  OpenProfiler 0x{e[0]:x} {e[1]} {e[2]}")
    fail.append("dominator tree top differs")
if mtop:
    print(f"top dominator: 0x{mtop[0][0]:x} retained {mtop[0][2]:,}")

print("PARITY OK" if not fail else "PARITY FAILED: " + "; ".join(fail))
sys.exit(1 if fail else 0)
PY
