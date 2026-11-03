#!/usr/bin/env bash
# Package and exercise the VS Code extension CLI with isolated profile and extensions.
# Usage: scripts/vsix-lifecycle.sh </path/to/code>
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
code=${1:?usage: scripts/vsix-lifecycle.sh /path/to/code}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
export HOME="$work/home" XDG_CONFIG_HOME="$work/config" XDG_DATA_HOME="$work/data"
mkdir -p "$HOME" "$work/user" "$work/extensions"
(cd "$root/plugins/vscode" && npm run build && ./node_modules/.bin/vsce package --no-dependencies -o "$work/agentspeedrun.vsix")
run_code() { "$code" --user-data-dir "$work/user" --extensions-dir "$work/extensions" "$@"; }
run_code --install-extension "$work/agentspeedrun.vsix" --force
run_code --list-extensions | grep -ix 'mesh-inc.agentspeedrun'
run_code --install-extension "$work/agentspeedrun.vsix" --force
run_code --list-extensions | grep -ix 'mesh-inc.agentspeedrun'
run_code --uninstall-extension mesh-inc.agentspeedrun
if run_code --list-extensions | grep -ix 'mesh-inc.agentspeedrun'; then
  echo 'extension remains installed' >&2
  exit 1
fi
echo 'vsix lifecycle: ok (same-version replacement)'
