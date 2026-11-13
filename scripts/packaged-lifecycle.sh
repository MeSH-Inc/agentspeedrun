#!/usr/bin/env bash
# Exercise an archive through the real HTTP installer, isolated from user data.
# Usage: scripts/packaged-lifecycle.sh <directory containing built speedrun and speedrund>
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
bin=$(cd "${1:?usage: scripts/packaged-lifecycle.sh <bindir>}" && pwd)
scratch=$(mktemp -d)
work="$scratch/path with spaces"
mkdir "$work"
export HOME="$work/home" SPEEDRUN_HOME="$work/data" XDG_CONFIG_HOME="$work/config" XDG_DATA_HOME="$work/xdg-data"
export SPEEDRUN_INSTALL_DIR="$work/bin" SPEEDRUN_SKIP_ATTESTATION=1
SPEEDRUN_VERSION="v$("$root/scripts/version.py" get)"
SPEEDRUN_ADDR="127.0.0.1:$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
export SPEEDRUN_VERSION SPEEDRUN_ADDR
unset SPEEDRUN_NO_AUTOSTART SPEEDRUN_BIN
mkdir -p "$HOME" "$work/assets" "$work/stage" "$work/repo" "$XDG_CONFIG_HOME/opencode"
printf '%s\n' '{"dependencies":{"other":"1"},"private":true}' > "$XDG_CONFIG_HOME/opencode/package.json"
case "$(uname -s)/$(uname -m)" in
  Darwin/arm64) target=aarch64-apple-darwin ;;
  Darwin/x86_64) target=x86_64-apple-darwin ;;
  Linux/x86_64|Linux/amd64) target=x86_64-unknown-linux-musl ;;
  Linux/aarch64|Linux/arm64) target=aarch64-unknown-linux-musl ;;
  *) echo 'unsupported test platform' >&2; exit 1 ;;
esac
checksum() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi
}
name="agentspeedrun-$SPEEDRUN_VERSION-$target"
mkdir "$work/stage/$name"
cp "$bin/speedrun" "$bin/speedrund" "$root/README.md" "$root/LICENSE-MIT" "$root/LICENSE-APACHE" "$work/stage/$name/"
SOURCE_DATE_EPOCH=0 "$root/scripts/archive.py" "$work/stage" "$name" "$work/assets/$name.tar.gz"
(cd "$work/assets" && checksum "$name.tar.gz" >SHA256SUMS)
python3 - "$work/assets" "$work/port" <<'PY' &
import http.server, os, sys
os.chdir(sys.argv[1])
server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), http.server.SimpleHTTPRequestHandler)
with open(sys.argv[2], 'w') as f:
    f.write(str(server.server_port))
server.serve_forever()
PY
server=$!
other_addr="127.0.0.1:$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
mkdir "$work/other-bin"
cp "$bin/speedrun" "$bin/speedrund" "$work/other-bin/"
other() { SPEEDRUN_HOME="$work/other-data" SPEEDRUN_ADDR="$other_addr" "$work/other-bin/speedrun" "$@"; }
cleanup() {
  other daemon stop >/dev/null 2>&1 || true
  "$SPEEDRUN_INSTALL_DIR/speedrun" daemon stop >/dev/null 2>&1 || true
  kill "$server" 2>/dev/null || true
  wait "$server" 2>/dev/null || true
  rm -rf "$scratch"
}
trap cleanup EXIT
for _ in {1..100}; do [ -s "$work/port" ] && break; sleep 0.05; done
SPEEDRUN_DOWNLOAD_URL="http://127.0.0.1:$(cat "$work/port")"
export SPEEDRUN_DOWNLOAD_URL
install() { sh "$root/install.sh"; }
sr() { "$SPEEDRUN_INSTALL_DIR/speedrun" "$@"; }
other daemon restart >/dev/null
other_before=$(other daemon status)
install
[ "$(other daemon status)" = "$other_before" ] || { echo 'unrelated daemon changed' >&2; exit 1; }
# Even an authenticated daemon must not be stopped when it came from another installation.
SPEEDRUN_HOME="$work/other-data" SPEEDRUN_ADDR="$other_addr" install
[ "$(other daemon status)" = "$other_before" ] || { echo 'other installation daemon changed' >&2; exit 1; }
install # idempotent before daemon start
cd "$work/repo"
git init -q
git -c user.name=test -c user.email=test@example.com commit -q --allow-empty -m init
sr setup --host opencode --scope user --capture content --yes >/dev/null
printf '%s\n' '{"sessionId":"lifecycle","prompt":"packaged lifecycle prompt"}' | sr hook copilot user-prompt-submit
sr capture | grep -qi captured
printf 'second packaged prompt\n' | sr ingest prompt --host test --session lifecycle | grep -q '"recorded":true'
sr preferences retention 37 >/dev/null
cp "$SPEEDRUN_HOME/preferences.json" "$work/preferences"
cp "$SPEEDRUN_HOME/trusted-projects.json" "$work/trust"
before=$(sr daemon status)
# Linux checks /proc without ps; elsewhere a failed ps must leave binaries alone.
mkdir "$work/no-ps"
printf '#!/bin/sh\nexit 127\n' > "$work/no-ps/ps"
chmod +x "$work/no-ps/ps"
cp "$SPEEDRUN_INSTALL_DIR/speedrun" "$work/before-no-ps"
if [ "$(uname -s)" = Linux ]; then
  PATH="$work/no-ps:$PATH" install
  [ "$(sr daemon status)" != "$before" ] || { echo 'daemon was not restarted without ps' >&2; exit 1; }
  before=$(sr daemon status)
else
  if PATH="$work/no-ps:$PATH" install; then echo 'missing ps was accepted' >&2; exit 1; fi
  cmp "$work/before-no-ps" "$SPEEDRUN_INSTALL_DIR/speedrun"
  [ "$(sr daemon status)" = "$before" ] || { echo 'daemon changed when ps failed' >&2; exit 1; }
fi
install # same-version replacement exercises upgrade/restart, not cross-version compatibility
after=$(sr daemon status)
cmp "$work/preferences" "$SPEEDRUN_HOME/preferences.json"
cmp "$work/trust" "$SPEEDRUN_HOME/trusted-projects.json"
[ "$(other daemon status)" = "$other_before" ] || { echo 'unrelated daemon changed' >&2; exit 1; }
[ "$before" != "$after" ] || { echo 'daemon was not restarted' >&2; exit 1; }
sr capture | grep -qi captured
cp "$SPEEDRUN_INSTALL_DIR/speedrun" "$work/expected"
printf '%064d  %s\n' 0 "$name.tar.gz" > "$work/assets/SHA256SUMS"
if install; then echo 'bad checksum accepted' >&2; exit 1; fi
cmp "$work/expected" "$SPEEDRUN_INSTALL_DIR/speedrun"
sr capture | grep -qi captured
# A checksum-valid but incomplete archive must not replace either installed binary.
cp "$work/assets/$name.tar.gz" "$work/valid.tar.gz"
cp "$SPEEDRUN_INSTALL_DIR/speedrund" "$work/expected-daemon"
rm "$work/stage/$name/speedrund"
SOURCE_DATE_EPOCH=0 "$root/scripts/archive.py" "$work/stage" "$name" "$work/assets/$name.tar.gz"
(cd "$work/assets" && checksum "$name.tar.gz" > SHA256SUMS)
if install; then echo 'incomplete archive accepted' >&2; exit 1; fi
cmp "$work/expected" "$SPEEDRUN_INSTALL_DIR/speedrun"
cmp "$work/expected-daemon" "$SPEEDRUN_INSTALL_DIR/speedrund"
mv "$work/valid.tar.gz" "$work/assets/$name.tar.gz"
sr setup opencode --remove >/dev/null
sr setup opencode --remove >/dev/null
adapter="$XDG_CONFIG_HOME/opencode/plugins/agentspeedrun.ts"
[ ! -e "$adapter" ]
printf 'foreign plugin\n' > "$adapter"
if sr setup opencode --remove; then echo 'foreign adapter removed' >&2; exit 1; fi
grep -q 'foreign plugin' "$adapter"
grep -q '"other"' "$XDG_CONFIG_HOME/opencode/package.json"
sh "$root/install.sh" --uninstall
[ ! -e "$SPEEDRUN_INSTALL_DIR/speedrun" ] || { echo "CLI was not removed" >&2; exit 1; }
[ ! -e "$SPEEDRUN_INSTALL_DIR/speedrund" ] || { echo "daemon was not removed" >&2; exit 1; }
[ -d "$SPEEDRUN_HOME" ] && [ -d .speedrun ]
(cd "$work/assets" && checksum "$name.tar.gz" > SHA256SUMS)
install
sr flush >/dev/null
sr capture | grep -qi captured
# Daemon liveness must not depend on the external kill utility.
mkdir "$work/no-kill"
# The generated command must expand the marker when it runs, not while written.
# shellcheck disable=SC2016
printf '#!/bin/sh\n: > "$SPEEDRUN_TEST_KILL_MARKER"\nexit 127\n' > "$work/no-kill/kill"
chmod +x "$work/no-kill/kill"
SPEEDRUN_TEST_KILL_MARKER="$work/kill-invoked" PATH="$work/no-kill:$PATH" sr daemon stop
[ ! -e "$work/kill-invoked" ] || { echo 'daemon stop invoked external kill' >&2; exit 1; }
printf 'modified' >> "$SPEEDRUN_INSTALL_DIR/speedrund"
sh "$root/install.sh" --uninstall
[ -e "$SPEEDRUN_INSTALL_DIR/speedrund" ] || { echo 'modified binary removed' >&2; exit 1; }
[ ! -e "$SPEEDRUN_INSTALL_DIR/speedrun" ]
install
sr flush >/dev/null
sr capture | grep -qi captured
echo 'packaged lifecycle: ok (same-version replacement)'
