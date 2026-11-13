#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source .buildkite/source.sh
export SOURCE_DATE_EPOCH
SOURCE_DATE_EPOCH="$(git show -s --format=%ct HEAD)"
export COMPOSE_ANSI=never COMPOSE_PROGRESS=plain COMPOSE_MENU=false
export COMPOSE_PROJECT_NAME="ci-agentspeedrun-${BUILDKITE_JOB_ID:-local-$$}"
cleanup() {
  result=$?
  set +e
  docker compose -f .buildkite/compose.yml down --volumes --remove-orphans
  cleanup_result=$?
  if (( result == 0 && cleanup_result != 0 )); then result=$cleanup_result; fi
  exit "$result"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
docker compose -f .buildkite/compose.yml up --build --abort-on-container-exit --exit-code-from verify verify
