#!/usr/bin/env bash
# Decides whether a CI job has code to check, for the `driver` and `fuzz` workflows. Writes
# `code=true` or `code=false` to $GITHUB_OUTPUT.
#
# `code=false` only for a pull request whose every changed file is documentation, as listed
# below; anything else is `code=true`: another event, another file, an empty diff, or any error
# (fail-closed). File names are data: they are matched, never executed or interpolated.
#
# Expects $EVENT (the event name) and a checkout of the pull request's merge commit with its
# first parent (`fetch-depth: 2`).

set -uo pipefail

code=true
if [ "${EVENT:-}" = "pull_request" ]; then
  if files=$(git diff --name-only HEAD^1 HEAD) && [ -n "$files" ]; then
    echo "changed files:"
    sed 's/^/  /' <<<"$files"
    # Documentation only: Markdown anywhere (no build reads it), docs/, the license texts and
    # the issue and pull request templates. Never workflows, scripts, manifests or code.
    if ! grep -vE '^(docs/.*|LICENSES/.*|\.github/ISSUE_TEMPLATE/.*|.*\.md)$' <<<"$files" >/dev/null; then
      code=false
    fi
  else
    echo "could not list the changed files: checking everything"
  fi
fi
echo "code to check: $code"
echo "code=$code" >> "$GITHUB_OUTPUT"
