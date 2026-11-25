# Fork tools

This folder exists only on the fork (branch `fork-tools`); it is never sent upstream.

- `main` mirrors upstream `main`. Nothing is committed to it.
- Every change lives on one topic branch started from `main`, reviewed once on its own fork PR.
- `integration` is rebuilt, never committed to: `fork/integration-rebuild.sh --push` resets it to
  upstream `main` and merges each branch in `integration-topics.txt`, then this branch. A permanent
  draft PR ("do not merge") from `integration` runs CI on the combination.
- A release candidate is a tag on an `integration` build, for example `rc-2026-11-25`.
- When upstream merges a topic, delete the branch and its line in `integration-topics.txt`.
