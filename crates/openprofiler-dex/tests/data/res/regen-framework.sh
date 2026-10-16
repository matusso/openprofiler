#!/usr/bin/env bash
# Regenerates src/res/framework.txt: names of the public framework (android:) resources.
# Format: "=<type> <first id hex>" starts a run of consecutive ids, one name per line
# (empty line = non-public id).
set -euo pipefail
cd "$(dirname "$0")"
SDK="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
"$SDK/build-tools/36.0.0/aapt2" dump resources "$SDK/platforms/android-36/android.jar" |
    python3 -I -c '
import sys
rows = sorted(
    (int(f[1], 16), f[2]) for f in (l.split() for l in sys.stdin)
    if len(f) >= 4 and f[0] == "resource" and f[1].startswith("0x01") and f[3] == "PUBLIC"
)
typ, nxt = None, -1
for rid, tn in rows:
    t, name = tn.split("/", 1)
    if t != typ or rid < nxt or rid - nxt > 16:
        print(f"={t} {rid:08x}")
        typ = t
    else:
        print("\n" * (rid - nxt), end="")
    print(name)
    nxt = rid + 1
' > ../../../src/res/framework.txt
