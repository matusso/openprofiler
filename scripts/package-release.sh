#!/usr/bin/env bash
# Packages one release archive from already built artifacts:
#   dist/openprofiler-<tag>-<os>-<arch>.tar.gz   (.zip on Windows) containing
#   openprofiler-<tag>-<os>-<arch>/
#     openprofiler[.exe]                 CLI and daemon
#     [lib]openprofiler_agent.{so,dylib,dll}  native agent for local JVMs (not on windows-arm64)
#     ui/                                web UI (`openprofiler daemon --web`)
#     bundle/<triple>/                   Linux relay + agents for remote and container attach
#     LICENSE (Apache-2.0), LICENSE-agent (MPL-2.0, the native agent), README.md
# Usage: scripts/package-release.sh <tag> <os-arch> <cli binary> [agent library]
# Expects the web UI in ui/dist and the remote bundle in dist/bundle.
set -euo pipefail
cd "$(dirname "$0")/.."

tag=${1:?usage: $0 <tag> <os-arch> <cli> [agent]}
platform=${2:?usage: $0 <tag> <os-arch> <cli> [agent]}
cli=${3:?usage: $0 <tag> <os-arch> <cli> [agent]}
agent=${4:-}
name="openprofiler-$tag-$platform"
stage="dist/stage/$name"

[[ -f ui/dist/index.html ]] || { echo "ui/dist is missing (run npm run build in ui/)" >&2; exit 1; }
compgen -G "dist/bundle/*/openprofiler-relay" >/dev/null || { echo "dist/bundle is missing (run scripts/build-bundle.sh)" >&2; exit 1; }

rm -rf "$stage"
mkdir -p "$stage"
cp "$cli" "$stage/"
[[ -n "$agent" ]] && cp "$agent" "$stage/"
cp -R ui/dist "$stage/ui"
cp -R dist/bundle "$stage/bundle"
chmod +x "$stage"/bundle/*/openprofiler-relay "$stage/$(basename "$cli")"
cp LICENSE README.md "$stage/"
cp crates/openprofiler-agent/LICENSE "$stage/LICENSE-agent"

cd dist/stage
case "$platform" in
  windows-*) archive="$name.zip"; rm -f "../$archive"; 7z a -tzip -bso0 "../$archive" "$name" ;;
  *) archive="$name.tar.gz"; tar -czf "../$archive" "$name" ;;
esac
echo "packaged dist/$archive"
