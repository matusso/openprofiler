#!/usr/bin/env bash
# Builds openprofiler-boot.jar and openprofiler-api.jar with plain javac/jar (Java 11 bytecode).
# Output: java/openprofiler-boot/build/openprofiler-boot.jar, java/openprofiler-api/build/openprofiler-api.jar
set -euo pipefail
cd "$(dirname "$0")"
JAVAC="${JAVA_HOME:+$JAVA_HOME/bin/}javac"
JAR="${JAVA_HOME:+$JAVA_HOME/bin/}jar"

build() {
  local name="$1"
  rm -rf "$name/build"
  mkdir -p "$name/build/classes"
  "$JAVAC" --release 11 -Xlint:all -Werror -d "$name/build/classes" $(find "$name/src" -name '*.java')
  "$JAR" --create --file "$name/build/$name.jar" -C "$name/build/classes" .
  echo "built $name/build/$name.jar"
}

build openprofiler-boot
build openprofiler-api
