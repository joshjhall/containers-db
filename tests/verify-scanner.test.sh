#!/usr/bin/env bash
# Reject-path tests for bin/verify-scanner.sh, the deliver job's sha256 gate
# (#436). Every real run takes the matching path, so a regression that let a
# substituted binary through would show up nowhere else.
#
# Each case asserts the exit code AND the exact `::error::` line, so a case
# cannot pass by failing for an unrelated reason.
#
# Usage: tests/verify-scanner.test.sh [path/to/verify-scanner.sh]
set -uo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)
script=${1:-$repo_root/bin/verify-scanner.sh}

invalid_msg='::error::build did not publish a valid scanner sha256.'
mismatch_msg='::error::Scanner binary does not match the sha256 build published; refusing to run it.'

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

binary=$scratch/scanner
printf 'scanner fixture\n' > "$binary"
good=$(sha256sum "$binary" | cut -d' ' -f1)

failures=0

# run_case NAME WANT_EXIT WANT_ERROR SHA256 BINARY
#   WANT_EXIT is "0" or "nonzero"; WANT_ERROR is the exact ::error:: line
#   expected on stderr, or "" for none at all.
run_case() {
  local name=$1 want_exit=$2 want_error=$3 sha=$4 bin=$5
  local stderr_file=$scratch/stderr rc errors ok=1

  bash "$script" "$sha" "$bin" 2> "$stderr_file" > /dev/null
  rc=$?
  errors=$(grep '^::error::' "$stderr_file")

  if [ "$want_exit" = 0 ]; then
    [ "$rc" -eq 0 ] || ok=0
  else
    [ "$rc" -ne 0 ] || ok=0
  fi
  [ "$errors" = "$want_error" ] || ok=0

  if [ "$ok" -eq 1 ]; then
    echo "PASS: $name"
  else
    echo "FAIL: $name (exit=$rc, ::error:: lines: ${errors:-<none>})"
    sed 's/^/  stderr: /' "$stderr_file"
    failures=$((failures + 1))
  fi
}

upper=$(printf '%s' "$good" | tr 'a-f' 'A-F')

run_case "matching hash exits 0" 0 "" "$good" "$binary"

flipped=$scratch/flipped
# Same length as the fixture, one byte different.
printf 'Scanner fixture\n' > "$flipped"
run_case "flipped byte is rejected" nonzero "$mismatch_msg" "$good" "$flipped"

run_case "empty hash is rejected" nonzero "$invalid_msg" "" "$binary"
run_case "uppercase hash is rejected" nonzero "$invalid_msg" "$upper" "$binary"
run_case "63-char hash is rejected" nonzero "$invalid_msg" "${good:0:63}" "$binary"
run_case "65-char hash is rejected" nonzero "$invalid_msg" "${good}0" "$binary"
run_case "hash with trailing newline is rejected" nonzero "$invalid_msg" \
  "$good"$'\n' "$binary"

run_case "missing binary is rejected" nonzero "$mismatch_msg" "$good" \
  "$scratch/does-not-exist"

# A newline in the path splits the check input into a valid line for the
# real binary plus a malformed one. Without --strict, sha256sum only warns
# about the malformed line and exits 0, so this case is what pins --strict.
run_case "path with an embedded newline is rejected" nonzero "$mismatch_msg" \
  "$good" "$binary"$'\n'"junk"

if [ "$failures" -ne 0 ]; then
  echo "$failures case(s) failed"
  exit 1
fi
echo "all cases passed"
