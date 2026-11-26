#!/usr/bin/env bash
# Rebuild the fork's integration branch: upstream main, then every topic branch
# listed in fork/integration-topics.txt (read from the fork-tools branch), in
# order, then fork-tools itself. Never commit on integration: change a topic
# branch and rebuild.
#
# Usage: fork/integration-rebuild.sh [--push]
#   --push  force-push the result to the fork (remote "fork").
# The rebuild runs in its own worktree (INTEGRATION_WORKTREE, default
# ../NeuroStrata-integration next to this checkout).
set -euo pipefail

repo=$(git rev-parse --show-toplevel)
wt="${INTEGRATION_WORKTREE:-$(dirname "$repo")/NeuroStrata-integration}"

git fetch --quiet origin main
# A conflict between two topics is resolved once; rerere replays it on every
# later rebuild.
git config rerere.enabled true
git config rerere.autoupdate true

topics=$(git show fork-tools:fork/integration-topics.txt | sed 's/#.*//' | awk 'NF { print $1 }')

if [ ! -d "$wt" ]; then
    git worktree add --quiet --detach "$wt" origin/main
fi
cd "$wt"
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    echo "integration worktree $wt has uncommitted changes; stopping" >&2
    exit 1
fi
git checkout --quiet -B integration origin/main

for topic in $topics fork-tools; do
    if ! git rev-parse --verify --quiet "$topic^{commit}" >/dev/null; then
        echo "topic $topic: no such branch" >&2
        exit 1
    fi
    if git merge --quiet --no-ff --no-edit -m "integration: merge $topic" "$topic"; then
        echo "merged $topic"
    elif [ -z "$(git diff --name-only --diff-filter=U)" ]; then
        git commit --quiet --no-edit
        echo "merged $topic (conflicts replayed by rerere)"
    else
        echo "topic $topic conflicts in:" >&2
        git diff --name-only --diff-filter=U >&2
        echo "resolve them in $wt and commit; rerere records the resolution, so rerun the script afterwards" >&2
        exit 1
    fi
done

echo "integration = $(git rev-parse --short HEAD) on origin/main $(git rev-parse --short origin/main)"
if [ "${1:-}" = "--push" ]; then
    git push --force-with-lease fork integration
fi
