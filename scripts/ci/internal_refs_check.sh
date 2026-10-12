#!/usr/bin/env bash
# -------------------------------------------------------------------
# scripts/ci/internal_refs_check.sh — public repo hygiene check
# @purpose Fail if a tracked text file cites internal decision records
#          or work items (e.g. "ADR-" plus a number, "dispatch" plus an
#          8-character hex id). This repository is public.
# @usage   scripts/ci/internal_refs_check.sh            # check the tree
#          scripts/ci/internal_refs_check.sh --selftest # prove it fires
# -------------------------------------------------------------------

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."
ROOT=$PWD
ALLOW="$ROOT/scripts/ci/internal_refs_allow.txt"
PATTERN='ADR-C?[0-9]{2,4}|[Dd]ispatch(es)? [0-9a-f]{8}'

# Print the offending lines of the git repository in $1 (path:line:text),
# minus exact lines listed in the allowlist. Exit status 1 when any remain.
scan() {
  local hits
  hits=$(git -C "$1" grep -nIE "$PATTERN" -- . || true)
  if [ -f "$ALLOW" ] && [ -n "$hits" ]; then
    hits=$(grep -vxF -f <(grep -v '^#' "$ALLOW" | grep -v '^$') <<<"$hits" || true)
  fi
  [ -z "$hits" ] && return 0
  echo "$hits"
  return 1
}

if [ "${1:-}" = "--selftest" ]; then
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  git -C "$tmp" init -q
  # Assembled at runtime, so this script holds no matching string.
  printf '%s%s\n' "See ADR-" "0123 for details." > "$tmp/a.md"
  printf '// %s %s\n' "dispatch" "deadbeef" > "$tmp/b.rs"
  git -C "$tmp" add a.md b.rs
  if scan "$tmp" >/dev/null; then
    echo "FAIL: planted references were not detected."
    exit 1
  fi
  echo "OK: planted references detected."
fi

if ! scan "$ROOT"; then
  echo "FAIL: internal references in tracked files (see above). Rephrase them;"
  echo "      only lines that must stay go into scripts/ci/internal_refs_allow.txt."
  exit 1
fi
echo "OK: no internal references in tracked files."
