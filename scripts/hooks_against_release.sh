#!/usr/bin/env bash
# Run every command in the Claude Code plugin's hooks.json against the last
# released speedrun, as a user with the newest plugin and an older binary
# does (V0.2 C-4). Each must exit 0 within its timeout: on UserPromptSubmit
# any other code blocks the user's prompt.
#   scripts/hooks_against_release.sh [owner/repo]
# Public releases need curl and Python; authenticated gh also supports private repos.
# Requires a Linux x86_64 host.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
repo=${1:-MeSH-Inc/agentspeedrun}
target=x86_64-unknown-linux-musl
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

if [[ -n "${GH_TOKEN:-}${GITHUB_TOKEN:-}" ]] && command -v gh >/dev/null 2>&1; then
  tag=$(gh api "repos/$repo/releases" \
    --jq '[.[] | select(.draft | not) | select(.prerelease | not)][0].tag_name // empty')
  authenticated=1
else
  [[ "$repo" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || { echo 'invalid repository' >&2; exit 1; }
  tag=$(curl -fsSL --retry 2 --max-time 30 "https://api.github.com/repos/$repo/releases?per_page=100" |
    python3 -c 'import json,sys; releases=json.load(sys.stdin); assert isinstance(releases,list); print(next((r["tag_name"] for r in releases if not r["draft"] and not r["prerelease"]), ""))')
  authenticated=0
fi
if [ -z "$tag" ]; then
  echo "hooks-against-release: $repo has no release yet; nothing to check"
  exit 0
fi
archive="agentspeedrun-$tag-$target.tar.gz"
if (( authenticated )); then
  gh release download "$tag" -R "$repo" -D "$work" -p "$archive"
else
  encoded_tag=$(python3 -c 'import sys,urllib.parse; print(urllib.parse.quote(sys.argv[1],safe=""))' "$tag")
  curl -fsSL --retry 2 --max-time 120 -o "$work/$archive" "https://github.com/$repo/releases/download/$encoded_tag/$archive"
fi
tar -xzf "$work/$archive" -C "$work"
export PATH="$work/agentspeedrun-$tag-$target:$PATH"
export SPEEDRUN_HOME="$work/home" SPEEDRUN_NO_AUTOSTART=1
mkdir "$work/project"

python3 - "$root/plugins/claude-code/hooks/hooks.json" >"$work/commands" <<'EOF'
import json
import sys

for event, groups in json.load(open(sys.argv[1]))["hooks"].items():
    for group in groups:
        for hook in group["hooks"]:
            if hook["type"] == "command":
                print(f"{event}\t{hook.get('timeout', 60)}\t{hook['command']}")
EOF

failed=0
while IFS=$'\t' read -r event seconds command; do
  input=$(printf '{"session_id":"s","transcript_path":"","cwd":"%s","hook_event_name":"%s","prompt":"hello"}' \
    "$work/project" "$event")
  code=0
  (cd "$work/project" && printf '%s' "$input" | timeout "$seconds" sh -c "$command") || code=$?
  if [ "$code" -eq 0 ]; then
    echo "ok   $event: $command"
  else
    echo "FAIL $event: $command exited $code under $tag"
    failed=1
  fi
done <"$work/commands"
exit "$failed"
