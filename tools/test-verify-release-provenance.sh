#!/usr/bin/env bash
# Exercises verify-release-provenance.sh against a real remote, so the release
# path's checks are covered on every pull request even though the release job
# itself only runs for tags.

set -euo pipefail

script=$(cd "$(dirname "$0")" && pwd)/verify-release-provenance.sh
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

BRANCH=maintained
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t

git init --quiet --bare "$work/origin.git"
git clone --quiet "$work/origin.git" "$work/repo"
cd "$work/repo"
git checkout --quiet -b "$BRANCH"
echo one > f && git add f && git commit --quiet -m one
git push --quiet origin "$BRANCH"
merged_sha=$(git rev-parse HEAD)
git tag v1 && git push --quiet origin v1

# An unmerged commit, reachable from no branch on the remote.
git checkout --quiet -b side
echo two > f && git commit --quiet -am two
unmerged_sha=$(git rev-parse HEAD)
git tag v2 && git push --quiet origin v2
git checkout --quiet "$BRANCH"

fail=0
check() {
  local name=$1 want=$2; shift 2
  local rc=0
  "$@" >/dev/null 2>&1 || rc=$?
  if [ "$rc" != "$want" ]; then
    echo "FAIL: $name (exit $rc, want $want)" >&2
    fail=1
  else
    echo "ok: $name"
  fi
}

check "a tag on the maintained branch is publishable" 0 \
  "$script" v1 "$merged_sha" "$BRANCH"

check "a tag not reachable from the maintained branch is refused" 1 \
  "$script" v2 "$unmerged_sha" "$BRANCH"

check "a tag that does not resolve to the built commit is refused" 1 \
  "$script" v1 "$unmerged_sha" "$BRANCH"

check "a missing tag is refused" 1 \
  "$script" v-nonexistent "$merged_sha" "$BRANCH"

# The case the immutable-tag ruleset exists to prevent: the tag moves after
# the artifact is built. The check must catch it rather than publish the old
# bytes under the new commit.
git tag -f v1 "$unmerged_sha" >/dev/null 2>&1
git push --quiet --force origin v1
check "a tag moved after the build is refused" 1 \
  "$script" v1 "$merged_sha" "$BRANCH"

exit "$fail"
