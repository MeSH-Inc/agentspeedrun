#!/usr/bin/env bash
# Package release binaries for one target into dist/:
#   dist/agentspeedrun-v<version>-<target>.tar.gz
# containing agentspeedrun-v<version>-<target>/{speedrun,speedrund,README.md}.
# speedrun starts the speedrund next to it, so the two must stay together.
set -euo pipefail

target=${1:?usage: scripts/package.sh <target> [bindir]}
root=$(cd "$(dirname "$0")/.." && pwd)
bindir=${2:-$root/target/$target/release}
name="agentspeedrun-v$("$root/scripts/version.py" get)-$target"

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir "$stage/$name"
cp "$bindir/speedrun" "$bindir/speedrund" "$stage/$name/"
cp "$root/README.md" "$stage/$name/"
mkdir -p "$root/dist"
# Stable archives: fixed owner and order, so rebuilds of a tag compare equal.
tar -C "$stage" --owner=0 --group=0 -czf "$root/dist/$name.tar.gz" "$name" 2>/dev/null ||
  tar -C "$stage" -czf "$root/dist/$name.tar.gz" "$name" # BSD tar (macOS) has no --owner
echo "dist/$name.tar.gz"
