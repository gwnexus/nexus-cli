#!/usr/bin/env bash
# -------------------------------------------------------------------
# scripts/ci/gitleaks_selftest.sh — gitleaks config regression check
# @purpose Fail if .gitleaks.toml silently drops the built-in rules
#          (a synthetic token must be found) or if the synthetic test
#          tokens in the source tree start triggering.
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

# 2. The source tree (synthetic test tokens included) must stay clean.
for dir in nexus-core nexusctl tests; do
  count=$(findings "$tmp/$dir.json" --source "$dir")
  if [ "$count" -ne 0 ]; then
    echo "FAIL: $count finding(s) in $dir/:"
    jq -r '.[] | "  \(.RuleID) \(.File):\(.StartLine)"' "$tmp/$dir.json"
    exit 1
  fi
done
echo "OK: source tree clean."
