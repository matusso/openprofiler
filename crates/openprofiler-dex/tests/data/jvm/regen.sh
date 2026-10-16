#!/usr/bin/env bash
# Regenerates modern.jar (with debug info) and modern-nodebug.jar from src/ with javac, and the
# expected harness output from the original classes.
set -euo pipefail
cd "$(dirname "$0")"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
javac --release 21 -g -d "$tmp/classes" $(find src -name '*.java')
javac --release 21 -g:none -d "$tmp/nodebug" $(find src -name '*.java')
jar --create --file modern.jar -C "$tmp/classes" .
jar --create --file modern-nodebug.jar -C "$tmp/nodebug" .
ls -l modern.jar modern-nodebug.jar
javac -d "$tmp/harness" -cp "$tmp/classes" harness/Check.java
java -cp "$tmp/classes:$tmp/harness" Check > expected.txt
