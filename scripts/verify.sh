#!/usr/bin/env bash
# Local pre-release verification. Install Rust, bun, Node.js and the adapter
# dependencies first; see README.md. This never installs dependencies or CI.
set -euo pipefail
cd "$(dirname "$0")/.."
root=$PWD

cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all
cargo build
scripts/version.py check

for adapter in opencode pi; do
  (cd "plugins/$adapter" && npm run typecheck && npm test)
done
(cd plugins/vscode && npm run typecheck && npm test)
(cd plugins/zed && cargo test && cargo build --target wasm32-wasip2)

# Cargo tests check the committed protocol and bundle schemas and the example.
# Check the generated VS Code artifacts without modifying the checkout.
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/plugins/vscode/scripts" "$tmp/plugins/vscode/src" "$tmp/schema"
cp schema/protocol.schema.json "$tmp/schema/"
cp plugins/vscode/scripts/generate-protocol.cjs "$tmp/plugins/vscode/scripts/"
ln -s "$root/plugins/vscode/node_modules" "$tmp/plugins/vscode/node_modules"
(cd "$tmp/plugins/vscode" && node scripts/generate-protocol.cjs)
cmp plugins/vscode/src/protocol.generated.ts "$tmp/plugins/vscode/src/protocol.generated.ts"
cmp plugins/vscode/src/validate.generated.ts "$tmp/plugins/vscode/src/validate.generated.ts"

scripts/smoke.sh target/debug
printf '%s\n' 'Local verification passed.'
