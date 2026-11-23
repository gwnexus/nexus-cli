#!/usr/bin/env bash
# -------------------------------------------------------------------
# scripts/ci/gitleaks_selftest.sh: gitleaks config regression check
# @purpose Fail if .gitleaks.toml silently drops the built-in rules
#          (a synthetic token must be found), if any tracked file holds a
#          finding, or if the allowlist exempts more than the fixtures.
# -------------------------------------------------------------------

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."
CONFIG=.gitleaks.toml

findings() {
  local report="$1"
  shift
  gitleaks detect --no-git --no-banner --redact --log-level error \
    --config "$CONFIG" --report-format json --report-path "$report" "$@" || true
  jq length "$report"
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# 1. Synthetic GitHub PAT, assembled at runtime so this script itself
#    contains no token-shaped string.
mkdir "$tmp/fixture"
printf 'token = "%s%s"\n' "ghp_" "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8" > "$tmp/fixture/leak.txt"
count=$(findings "$tmp/fixture.json" --source "$tmp/fixture")
if [ "$count" -ne 1 ]; then
  echo "FAIL: expected 1 finding for the synthetic token, got $count."
  echo "      Does $CONFIG still contain [extend] useDefault = true?"
  exit 1
fi
echo "OK: synthetic token detected."

# 2. Every tracked file (synthetic test tokens included) must stay clean:
#    docs, workflows and the installer as well as the crates. The tracked
#    files are copied out, so build output and untracked files do not count.
mkdir "$tmp/tree"
git ls-files -z | tar --null -T - -cf - | tar -xf - -C "$tmp/tree"
count=$(findings "$tmp/tree.json" --source "$tmp/tree")
if [ "$count" -ne 0 ]; then
  echo "FAIL: $count finding(s) in tracked files:"
  jq -r --arg tree "$tmp/tree/" '.[] | "  \(.RuleID) \(.File | ltrimstr($tree)):\(.StartLine)"' "$tmp/tree.json"
  exit 1
fi
echo "OK: tracked files clean."

# 3. A real-looking credential next to a fixture name is still reported:
#    the allowlist only exempts the exact fixture values.
mkdir "$tmp/near"
#    (generic-api-key matches the whole assignment, name included).
printf 'nxs_pat_testtoken123_key = "%s%s"\n' "8f3kQ9zL2mX7vR4t" "W1yB6nC0pD5sH3jA" > "$tmp/near/near.txt"
count=$(findings "$tmp/near.json" --source "$tmp/near")
if [ "$count" -lt 1 ]; then
  echo "FAIL: a credential next to an allowlisted fixture was not reported."
  exit 1
fi
echo "OK: allowlist exempts only the fixture values."
