#!/usr/bin/env bash
# Test a published release archive against a development build on this platform.
# Usage: scripts/release-upgrade.sh <published archive> <published SHA256SUMS> <current bindir>
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
archive=$(cd "$(dirname "${1:?published archive required}")" && pwd)/$(basename "$1")
sums=$(cd "$(dirname "${2:?published SHA256SUMS required}")" && pwd)/$(basename "$2")
bin=$(cd "${3:?current bindir required}" && pwd)
case "$(uname -s)/$(uname -m)" in
  Darwin/arm64) target=aarch64-apple-darwin ;;
  Darwin/x86_64) target=x86_64-apple-darwin ;;
  Linux/x86_64|Linux/amd64) target=x86_64-unknown-linux-musl ;;
  Linux/aarch64|Linux/arm64) target=aarch64-unknown-linux-musl ;;
  *) echo 'unsupported platform' >&2; exit 1 ;;
esac
file=$(basename "$archive")
case "$file" in agentspeedrun-v*"-$target.tar.gz") ;; *) echo 'archive does not match this platform' >&2; exit 1 ;; esac
prior_version=${file#agentspeedrun-}; prior_version=${prior_version%-"$target".tar.gz}
scratch=$(mktemp -d)
work="$scratch/path with spaces"
mkdir -p "$work/assets" "$work/stage" "$work/repo" "$work/home"
export HOME="$work/home" SPEEDRUN_HOME="$work/data" XDG_CONFIG_HOME="$work/config" XDG_DATA_HOME="$work/xdg-data"
export SPEEDRUN_INSTALL_DIR="$work/bin" SPEEDRUN_SKIP_ATTESTATION=1
SPEEDRUN_ADDR="127.0.0.1:$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
export SPEEDRUN_ADDR
unset SPEEDRUN_NO_AUTOSTART SPEEDRUN_BIN
cp "$archive" "$work/assets/"
cp "$sums" "$work/assets/baseline-sums"
(cd "$work/assets"; grep " $file\$" baseline-sums > expected; if command -v sha256sum >/dev/null; then sha256sum -c expected; else shasum -a 256 -c expected; fi)
printf 'baseline: %s\n' "$(cat "$work/assets/expected")"
if command -v sha256sum >/dev/null; then sha256sum "$bin/speedrun" "$bin/speedrund"; else shasum -a 256 "$bin/speedrun" "$bin/speedrund"; fi
python3 - "$work/assets" "$work/port" <<'PY' &
import http.server, os, sys
os.chdir(sys.argv[1])
server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), http.server.SimpleHTTPRequestHandler)
with open(sys.argv[2], 'w') as f:
    f.write(str(server.server_port))
server.serve_forever()
PY
server=$!
cleanup() {
  "$SPEEDRUN_INSTALL_DIR/speedrun" daemon stop >/dev/null 2>&1 || true
  kill "$server" 2>/dev/null || true
  wait "$server" 2>/dev/null || true
  rm -rf "$scratch"
}
trap cleanup EXIT
for _ in {1..100}; do [ -s "$work/port" ] && break; sleep 0.05; done
SPEEDRUN_DOWNLOAD_URL="http://127.0.0.1:$(cat "$work/port")"
export SPEEDRUN_DOWNLOAD_URL
export SPEEDRUN_VERSION="$prior_version"
cp "$work/assets/baseline-sums" "$work/assets/SHA256SUMS"
sh "$root/install.sh"
if cmp -s "$SPEEDRUN_INSTALL_DIR/speedrun" "$bin/speedrun"; then
  echo "development binary is identical to the published binary" >&2; exit 1
fi
sr() { "$SPEEDRUN_INSTALL_DIR/speedrun" "$@"; }
cd "$work/repo"
git init -q
git -c user.name=test -c user.email=test@example.com commit -q --allow-empty -m init
sr init
printf 'published release prompt\n' | sr ingest prompt --host test --session upgrade | grep -q '"recorded":true'
sr log > "$work/before-log"
sr verify
cp "$SPEEDRUN_HOME/trusted-projects.json" "$work/trust"
[ ! -e "$SPEEDRUN_HOME/preferences.json" ] || cp "$SPEEDRUN_HOME/preferences.json" "$work/preferences"
sr daemon stop
printf 'offline release prompt\n' | SPEEDRUN_NO_AUTOSTART=1 sr ingest prompt --host test --session upgrade | grep -q '"spooled":true'
[ -s "$SPEEDRUN_HOME/spool/pending.jsonl" ] || { echo 'published release did not create a pending outbox' >&2; exit 1; }
SPEEDRUN_VERSION="v$("$root/scripts/version.py" get)"
export SPEEDRUN_VERSION
name="agentspeedrun-$SPEEDRUN_VERSION-$target"
mkdir "$work/stage/$name"
cp "$bin/speedrun" "$bin/speedrund" "$root/README.md" "$root/LICENSE-MIT" "$root/LICENSE-APACHE" "$work/stage/$name/"
SOURCE_DATE_EPOCH=0 "$root/scripts/archive.py" "$work/stage" "$name" "$work/assets/$name.tar.gz"
(cd "$work/assets"; if command -v sha256sum >/dev/null; then sha256sum "$name.tar.gz" > SHA256SUMS; else shasum -a 256 "$name.tar.gz" > SHA256SUMS; fi)
sh "$root/install.sh"
cmp "$work/trust" "$SPEEDRUN_HOME/trusted-projects.json"
if [ -e "$work/preferences" ]; then cmp "$work/preferences" "$SPEEDRUN_HOME/preferences.json"; fi
sr log > "$work/after-log"
cmp "$work/before-log" "$work/after-log"
sr verify
sr flush | grep -q 'delivered 1 spooled events'
sr doctor | grep -q 'spooled: 0'
sr log > "$work/flushed-log"
[ "$(wc -l < "$work/flushed-log")" -gt "$(wc -l < "$work/before-log")" ] || { echo 'offline event was not recovered' >&2; exit 1; }
sr verify
printf 'release upgrade: ok (%s -> development build v%s; checkout %s)\n' "$prior_version" "$("$root/scripts/version.py" get)" "$(git -C "$root" rev-parse HEAD)"
