#!/usr/bin/env bash
# Smoke-test installed or packaged binaries end to end, in a throwaway
# data dir and repo: daemon autostart from the CLI's own directory, a
# prompt starting a run, splits, the hash chain, embedded adapters.
#   scripts/smoke.sh <dir containing speedrun and speedrund>
set -euo pipefail

bin=$(cd "${1:?usage: scripts/smoke.sh <bindir>}" && pwd)
work=$(mktemp -d)
export SPEEDRUN_HOME="$work/home"
export SPEEDRUN_ADDR="127.0.0.1:$((20000 + RANDOM % 20000))"
unset SPEEDRUN_NO_AUTOSTART SPEEDRUN_BIN
cleanup() {
  # The stop command authenticates against this temporary home before shutdown.
  # Never kill by executable name: other users may be running the same binary.
  "$bin/speedrun" daemon stop >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT
fail() {
  echo "smoke: FAIL: $*" >&2
  cat "$SPEEDRUN_HOME/speedrund.log" >&2 2>/dev/null || true
  exit 1
}
sr() { "$bin/speedrun" "$@"; }

# Capture before matching: `| grep -q` would close the pipe early.
has() { grep -q -- "$2" <<<"$1"; }

has "$(sr --version)" "speedrun" || fail "--version"

mkdir "$work/repo" && cd "$work/repo"
git init -q
git -c user.name=smoke -c user.email=smoke@example.com commit -q --allow-empty -m init
sr init --category any-percent >/dev/null

out=$(echo "build a todo app" | sr ingest prompt --host smoke --session s1)
if has "$out" '"run_started":true'; then
  : # The daemon accepted the prompt during the autostart window.
elif has "$out" '"recorded":true' && has "$out" '"spooled":true'; then
  echo 'smoke: prompt queued during daemon startup; verifying replay'
  # A slow autostart may leave the prompt durably queued. Do not start a
  # replacement daemon: prove that the original autostart becomes ready,
  # then deliver the queued prompt before making commit evidence.
  ready=false
  for _ in {1..100}; do
    if sr daemon status >/dev/null 2>&1; then ready=true; break; fi
    sleep 0.1
  done
  [[ "$ready" == true ]] || fail "autostart did not become ready: $out"
  flushed=$(sr flush) || fail "could not deliver the queued prompt"
  has "$flushed" 'delivered 1 spooled events' || fail "queued prompt was not delivered: $flushed"
else
  fail "prompt was neither started nor durably spooled: $out"
fi
# Commit evidence must be made after the run started, so commit once the prompt has started it.
git -c user.name=smoke -c user.email=smoke@example.com commit -q --allow-empty -m scaffold

sr split scaffold-runs --commit HEAD >/dev/null || fail "split"
has "$(sr status)" "✔ scaffold-runs" || fail "split did not count: $(sr status)"
sr split happy-path-demo --commit HEAD >/dev/null || fail "happy-path split"
has "$(sr status)" "completed" || fail "run did not complete: $(sr status)"
has "$(sr verify)" "^ok" || fail "hash chain"

sr setup opencode --project >/dev/null || fail "setup opencode"
find .opencode -name agentspeedrun.ts | grep -q . || fail "opencode adapter not written"

echo "smoke: ok ($("$bin/speedrun" --version))"
