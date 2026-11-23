#!/usr/bin/env bash
# -------------------------------------------------------------------
# scripts/ci/internal_refs_check.sh: public repo hygiene check
# @purpose Fail if a tracked text file cites internal decision records
#          or work items (e.g. "ADR-" plus a number, "dispatch" plus an
#          8-character hex id) or a private IPv4 address (RFC 1918).
#          This repository is public; examples use 192.0.2.0/24.
# @usage   scripts/ci/internal_refs_check.sh            # check the tree
#          scripts/ci/internal_refs_check.sh --selftest # prove it fires
# -------------------------------------------------------------------

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."
ROOT=$PWD
ALLOW="$ROOT/scripts/ci/internal_refs_allow.txt"
PATTERN='ADR-C?[0-9]{2,4}|[Dd]ispatch(es)? [0-9a-f]{8}'
# Private IPv4 ranges (10/8, 172.16/12, 192.168/16) as a whole address.
PATTERN+='|(^|[^0-9.])(10\.[0-9]{1,3}|172\.(1[6-9]|2[0-9]|3[01])|192\.168)\.[0-9]{1,3}\.[0-9]{1,3}([^0-9.]|\.([^0-9]|$)|$)'

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
  printf 'url = "http://%s.%s/v1"\n' "10.1" "2.3" > "$tmp/c.toml"
  printf 'host: %s.%s\n' "192.168" "0.7" > "$tmp/d.yml"
  printf 'Endpoint: %s.%s.\n' "172.16" "4.2" > "$tmp/e.md"
  # Must pass: documentation range and a four-part version number.
  printf 'url = "http://%s/v1"\nversion 1.10.2.3\n' "192.0.2.10" > "$tmp/ok.txt"
  git -C "$tmp" add a.md b.rs c.toml d.yml e.md ok.txt
  found=$(scan "$tmp" || true)
  for f in a.md b.rs c.toml d.yml e.md; do
    if ! grep -q "^$f:" <<<"$found"; then
      echo "FAIL: planted reference in $f was not detected."
      exit 1
    fi
  done
  if grep -q "^ok.txt:" <<<"$found"; then
    echo "FAIL: a documentation address or version number was flagged."
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
