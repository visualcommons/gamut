#!/usr/bin/env bash
# Post-merge gates wait for master to go quiet (issue #956).
#
# The test/coverage gate (`.github/workflows/coverage.yml`) and the incremental mutation survey
# (`.github/workflows/mutants.yml`) run after merge, not per pull request: a ~2 h instrumented
# test run on every PR held up ad-hoc merging, and a release PR re-verifies master anyway. They
# do not start on every push either. A burst of merges would otherwise queue one ~2 h run per
# commit, each verifying a tree that is already stale. They start once master has gone
# QUIET_SECS without a push, and then verify only the head.
#
# Subcommands, each writing `key=value` lines to $GITHUB_OUTPUT (stdout when unset):
#
#   wait          Block until the last push to master is QUIET_SECS old, then emit
#                 `run=true` and `sha=<master head>`. The push time comes from the repository
#                 activity API, which records when the ref moved. A commit's own date can be
#                 arbitrarily old: a merge commit carries the merge time, but a rebased or
#                 locally made commit does not.
#                   EXPECT_SHA     (push runs) master's head must still be this commit. Once it
#                                  is not, a newer push's run owns the work, so emit `run=false`
#                                  and stop. That run's own `wait` normally cancels this one
#                                  through the job's concurrency group already; this check closes
#                                  the window before that cancellation lands.
#                   MAX_WAIT_SECS  (scheduled runs) give up waiting after this long and emit
#                                  `run=true` anyway with a warning, so a busy day cannot cost the
#                                  nightly mutation rotation its round. 0 or unset waits forever.
#
#   mutants-base  Emit `base=<sha>`: the commit the incremental mutation survey diffs against.
#                 This is the head of the newest push-triggered Mutants run in which every
#                 incremental shard job concluded `success`, when that commit is an ancestor of
#                 HEAD. Because pushes now supersede each other, a run's diff is no longer one
#                 push. It is everything since master was last scored clean, so a superseded,
#                 cancelled, pre-empted or red run leaves its diff to the next run rather than
#                 leaving it unscored. A survivor therefore stays red on later runs until it is
#                 fixed. With no such run (or one not in HEAD's history) it falls back to
#                 FALLBACK_BEFORE (the push's `github.event.before`) and then to HEAD^, with a
#                 warning.
#
# Environment: GH_TOKEN (any token that can read Actions and the repository), GITHUB_REPOSITORY
# (owner/name). Needs gh, jq and git.
set -euo pipefail

repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY must be owner/name}"
out="${GITHUB_OUTPUT:-/dev/stdout}"

# Print "<sha> <epoch seconds>" for the most recent change to master.
last_push() {
  gh api "repos/$repo/activity?ref=refs/heads/master&per_page=1" \
    --jq '.[0] | "\(.after) \(.timestamp | fromdateiso8601)"'
}

cmd_wait() {
  local quiet="${QUIET_SECS:-3600}" max_wait="${MAX_WAIT_SECS:-0}" expect="${EXPECT_SHA:-}"
  local start now sha pushed age nap
  start=$(date +%s)
  while :; do
    read -r sha pushed < <(last_push)
    if [ -z "$sha" ] || [ -z "$pushed" ]; then
      echo "::error::could not read the last push to master from the activity API"
      exit 1
    fi
    if [ -n "$expect" ] && [ "$sha" != "$expect" ]; then
      echo "master moved on to $sha since $expect was pushed; the newer run verifies it"
      echo "run=false" >> "$out"
      return 0
    fi
    now=$(date +%s)
    age=$((now - pushed))
    if [ "$age" -ge "$quiet" ]; then
      echo "master has been quiet for ${age}s (>= ${quiet}s) at $sha"
      break
    fi
    if [ "$max_wait" -gt 0 ] && [ $((now - start)) -ge "$max_wait" ]; then
      echo "::warning::master was still busy after ${max_wait}s of waiting; running at $sha anyway"
      break
    fi
    nap=$((quiet - age))
    if [ "$max_wait" -gt 0 ] && [ "$nap" -gt $((start + max_wait - now)) ]; then
      nap=$((start + max_wait - now))
    fi
    echo "last push to master ($sha) was ${age}s ago; waiting ${nap}s for ${quiet}s of quiet"
    sleep "$nap"
  done
  {
    echo "run=true"
    echo "sha=$sha"
  } >> "$out"
}

cmd_mutants_base() {
  local head base="" run_id run_sha shards
  head=$(git rev-parse HEAD)
  # Newest first. Only push-triggered runs carry the incremental job; scheduled ones run the
  # rotation instead.
  while read -r run_id run_sha; do
    [ -n "$run_id" ] || continue
    # Every incremental shard concluded success, and there was at least one. A run whose
    # `wait` said `run=false`, or that was cancelled, has its shards `skipped` or `cancelled`.
    shards=$(gh api "repos/$repo/actions/runs/$run_id/jobs?per_page=100" --jq '
      [.jobs[] | select(.name | startswith("Incremental (push diff)")) | .conclusion]
      | if length > 0 and all(. == "success") then "clean" else "no" end')
    if [ "$shards" = clean ] && git merge-base --is-ancestor "$run_sha" "$head" 2>/dev/null; then
      base="$run_sha"
      echo "last clean incremental survey: run $run_id at $run_sha"
      break
    fi
  done < <(gh api "repos/$repo/actions/workflows/mutants.yml/runs?branch=master&event=push&status=completed&per_page=50" \
    --jq '.workflow_runs[] | "\(.id) \(.head_sha)"')

  if [ -z "$base" ]; then
    base="${FALLBACK_BEFORE:-}"
    if [ -z "$base" ] || [ "$base" = 0000000000000000000000000000000000000000 ] ||
      ! git cat-file -e "$base^{commit}" 2>/dev/null; then
      base=$(git rev-parse HEAD^)
    fi
    echo "::warning::no clean incremental survey found in HEAD's history; diffing against $base"
  fi
  echo "base=$base" >> "$out"
}

case "${1:-}" in
  wait) cmd_wait ;;
  mutants-base) cmd_mutants_base ;;
  *)
    echo "usage: $0 wait|mutants-base" >&2
    exit 2
    ;;
esac
