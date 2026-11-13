#!/usr/bin/env bash
# Resolve the exact commit to verify (fleet convention, from honsemaxx): the
# requested head on a branch build; on a PR build, a deterministic local merge of
# the requested head into the fetched target branch.
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ -n "${BUILDKITE_COMMIT:-}" && "$BUILDKITE_COMMIT" != HEAD ]]; then
  test "$(git rev-parse HEAD)" = "$BUILDKITE_COMMIT"
fi
if [[ "${BUILDKITE_PULL_REQUEST:-false}" != false ]]; then
  [[ "$BUILDKITE_PULL_REQUEST" =~ ^[0-9]+$ ]]
  [[ "${BUILDKITE_COMMIT:-}" =~ ^[0-9a-f]{40}$ ]]
  test -z "$(git status --porcelain)"
  base_ref="refs/heads/${BUILDKITE_PULL_REQUEST_BASE_BRANCH:?PR target branch is required}"
  git check-ref-format "$base_ref"
  # GitHub deletes refs/pull/N/merge after merging/closing a PR. Build the merge
  # from durable commits instead; never substitute a newer PR head for this job.
  git fetch --no-tags origin "$base_ref"
  base=$(git rev-parse 'FETCH_HEAD^{commit}')
  head=$BUILDKITE_COMMIT
  printf 'PR source head: %s; target %s: %s\n' "$head" "$base_ref" "$base"
  if git merge-base --is-ancestor "$head" "$base"; then
    # A queued job may start after the PR landed. Verify the containing base.
    verified_commit=$base
  else
    # Conflicts are errors, not permission to test the head in isolation. merge-tree
    # does not change the index/worktree. Its nonzero exit stops before checkout.
    if tree=$(git merge-tree --write-tree "$base" "$head"); then
      :
    else
      result=$?
      printf '%s\n' "$tree" >&2
      exit "$result"
    fi
    stamp=$(git show -s --format=%cI "$head")
    verified_commit=$(GIT_AUTHOR_NAME='Buildkite verification' GIT_AUTHOR_EMAIL='buildkite@agentspeedrun.invalid' \
      GIT_COMMITTER_NAME='Buildkite verification' GIT_COMMITTER_EMAIL='buildkite@agentspeedrun.invalid' \
      GIT_AUTHOR_DATE="$stamp" GIT_COMMITTER_DATE="$stamp" \
      git -c commit.gpgSign=false commit-tree "$tree" -p "$base" -p "$head" \
      -m "Verify PR $BUILDKITE_PULL_REQUEST: $head against $base")
  fi
  git checkout --detach "$verified_commit"
fi
printf 'Verified source commit: %s\n' "$(git rev-parse HEAD)"
printf 'Verified source tree: %s\n' "$(git rev-parse 'HEAD^{tree}')"
