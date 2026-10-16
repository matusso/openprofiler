#!/usr/bin/env bash
# Regenerates the binary resource fixtures from ./src with the Android SDK.
# Outputs: AndroidManifest.xml (binary), resources.arsc, main.xml (binary layout) and
# resources-compact.arsc (sparse types, compact entries) and resources-collapsed.arsc
# (all key names collapsed to one string, as obfuscators do).
set -euo pipefail
cd "$(dirname "$0")"
SDK="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
AAPT2="$SDK/build-tools/36.0.0/aapt2"
ANDROID_JAR="$SDK/platforms/android-36/android.jar"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
"$AAPT2" compile --dir src/res -o "$WORK/compiled.zip"
"$AAPT2" link -o "$WORK/out.apk" -I "$ANDROID_JAR" --manifest src/AndroidManifest.xml "$WORK/compiled.zip"
unzip -o -q "$WORK/out.apk" AndroidManifest.xml resources.arsc res/layout/main.xml -d "$WORK/x"
cp "$WORK/x/AndroidManifest.xml" "$WORK/x/resources.arsc" .
cp "$WORK/x/res/layout/main.xml" main.xml
"$AAPT2" optimize -o "$WORK/compact.apk" --force-sparse-encoding --enable-compact-entries "$WORK/out.apk"
"$AAPT2" optimize -o "$WORK/collapsed.apk" --collapse-resource-names "$WORK/out.apk"
unzip -o -q "$WORK/compact.apk" resources.arsc -d "$WORK/c"
unzip -o -q "$WORK/collapsed.apk" resources.arsc -d "$WORK/n"
cp "$WORK/c/resources.arsc" resources-compact.arsc
cp "$WORK/n/resources.arsc" resources-collapsed.arsc
# Reference dumps for manual comparison.
"$AAPT2" dump xmltree --file AndroidManifest.xml "$WORK/out.apk" > AndroidManifest.dump.txt
"$AAPT2" dump resources "$WORK/out.apk" > resources.dump.txt
