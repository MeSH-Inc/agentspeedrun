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
# Reproducible archives (scripts/archive.py): the same binaries give the same
# bytes. Entries are dated by the packaged commit unless SOURCE_DATE_EPOCH says.
SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git -C "$root" log -1 --format=%ct)}
export SOURCE_DATE_EPOCH
"$root/scripts/archive.py" "$stage" "$name" "$root/dist/$name.tar.gz"
echo "dist/$name.tar.gz"
