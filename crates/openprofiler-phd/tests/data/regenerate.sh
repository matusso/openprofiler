#!/usr/bin/env bash
# Regenerates the PHD fixtures with OpenJ9 (IBM Semeru) in Docker: one dump with compressed
# references (the default) and one without.
set -euo pipefail
cd "$(dirname "$0")"
image=${SEMERU_IMAGE:-ibm-semeru-runtimes:open-21-jdk}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp PhdFixture.java "$work/"
docker run --rm -v "$work":/w -w /w "$image" sh -c '
  javac PhdFixture.java &&
  java -Xshareclasses:none -Xmx32m PhdFixture /w/compressed.phd &&
  java -Xshareclasses:none -Xmx32m -Xnocompressedrefs PhdFixture /w/uncompressed.phd &&
  java -version 2>&1 | head -1 > /w/version.txt'
gzip -9 -c "$work/compressed.phd" > compressed.phd.gz
gzip -9 -c "$work/uncompressed.phd" > uncompressed.phd.gz
cat "$work/version.txt"
