#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

case "${1:-all}" in
  rust)
    cargo fmt --all --check
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --all --locked
    ;;
  opencode|pi)
    cargo build --locked
    cd "plugins/$1"
    npm ci --ignore-scripts --no-audit --no-fund
    npx tsc --noEmit
    bun test
    ;;
  vscode)
    cargo build --locked
    cd plugins/vscode
    npm ci --ignore-scripts --no-audit --no-fund
    npm run typecheck
    npm test
    xvfb-run -a npm run test:vscode
    npm run package
    ;;
  zed)
    cd plugins/zed
    cargo fmt --check
    cargo test --locked
    cargo clippy --locked --target wasm32-wasip2 -- -D warnings
    cargo build --locked --release --target wasm32-wasip2
    ;;
  package)
    shellcheck install.sh scripts/*.sh
    python3 scripts/version.py check
    cargo build --release --locked --target x86_64-unknown-linux-musl --bin speedrun --bin speedrund
    scripts/package.sh x86_64-unknown-linux-musl
    (cd dist && sha256sum -- *.tar.gz > /tmp/agentspeedrun-first.sha256)
    touch target/x86_64-unknown-linux-musl/release/speedrun README.md
    rm dist/*.tar.gz
    scripts/package.sh x86_64-unknown-linux-musl
    (cd dist && sha256sum -c /tmp/agentspeedrun-first.sha256 && sha256sum -- *.tar.gz > SHA256SUMS)
    python3 -m http.server 8765 --directory dist >/dev/null 2>&1 &
    server=$!
    trap 'kill "$server" 2>/dev/null || true; wait "$server" 2>/dev/null || true' EXIT
    sleep 1
    SPEEDRUN_VERSION="v$(scripts/version.py get)" SPEEDRUN_DOWNLOAD_URL=http://127.0.0.1:8765 \
      SPEEDRUN_SKIP_ATTESTATION=1 SPEEDRUN_INSTALL_DIR=/tmp/agentspeedrun-test-bin sh install.sh
    scripts/smoke.sh /tmp/agentspeedrun-test-bin
    ;;
  hooks)
    scripts/hooks_against_release.sh
    ;;
  all)
    failures=()
    for stage in rust opencode pi vscode zed package hooks; do
      echo "--- ${stage}"
      if bash "$0" "$stage"; then
        echo "PASS ${stage}"
      else
        failures+=("$stage")
        echo "FAIL ${stage}" >&2
      fi
    done
    if (( ${#failures[@]} )); then
      printf 'Failed verification stages: %s\n' "${failures[*]}" >&2
      exit 1
    fi
    ;;
  *) echo "Unknown verification stage: $1" >&2; exit 64 ;;
esac
