#!/usr/bin/env bash
# Regenerates corpus.dex from src/ with javac and d8 (Android build-tools).
# ANDROID_HOME defaults to ~/Library/Android/sdk; BUILD_TOOLS to the newest build-tools there.
set -euo pipefail
cd "$(dirname "$0")"
SDK="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
BT="${BUILD_TOOLS:-$(ls -d "$SDK"/build-tools/* | sort -V | tail -1)}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
javac --release 11 -g -d "$tmp/classes" $(find src -name '*.java')
"$BT/d8" --min-api 26 --output "$tmp" $(find "$tmp/classes" -name '*.class')
cp "$tmp/classes.dex" corpus.dex
# A release-style build without debug info (no local names or line numbers).
javac --release 11 -g:none -d "$tmp/nodebug" $(find src -name '*.java')
mkdir -p "$tmp/rel"
"$BT/d8" --release --min-api 26 --output "$tmp/rel" $(find "$tmp/nodebug" -name '*.class')
cp "$tmp/rel/classes.dex" corpus-release.dex
ls -l corpus.dex corpus-release.dex

# Expected harness output, from the original classes.
javac -d "$tmp/harness" -cp "$tmp/classes" harness/Check.java
java -cp "$tmp/classes:$tmp/harness" Check > expected.txt
