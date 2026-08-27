#!/usr/bin/env bash
# Verify that a tag may be published as a release.
#
# Two independent things have to hold, and both are re-read from the remote
# rather than trusted from the local checkout:
#
#   1. the tag still resolves to the commit whose artifact is being published,
#      so a tag moved after the build cannot relabel someone else's bytes;
#   2. that commit is reachable from the maintained branch, so a release
#      cannot be cut from code that was never reviewed or merged.
#
# Repository rulesets make release tags immutable, which is what actually
# prevents (1); this is the check that notices if that protection is ever
# missing or misconfigured.
#
# Usage: verify-release-provenance.sh <tag> <expected-sha> <maintained-branch>

set -euo pipefail

tag=${1:?tag required}
expected_sha=${2:?expected sha required}
maintained_branch=${3:?maintained branch required}

if ! git fetch --force --quiet origin "refs/tags/$tag:refs/tags/$tag"; then
  echo "$tag does not exist on the remote" >&2
  exit 1
fi
tag_commit=$(git rev-list -n 1 "$tag")
if [ "$tag_commit" != "$expected_sha" ]; then
  echo "$tag points at $tag_commit, not the built commit $expected_sha" >&2
  exit 1
fi

git fetch --quiet origin "$maintained_branch"
if ! git merge-base --is-ancestor "$expected_sha" "origin/$maintained_branch"; then
  echo "$tag ($expected_sha) is not reachable from $maintained_branch" >&2
  exit 1
fi

echo "$tag resolves to $expected_sha and is reachable from $maintained_branch"
