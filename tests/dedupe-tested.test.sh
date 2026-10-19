#!/usr/bin/env bash
# Tests for bin/dedupe-tested.sh, the one-off tested[] collapse (#450). Each
# case asserts the exit code AND the exact output or resulting rows, so a case
# cannot pass by failing for an unrelated reason.
#
# Usage: tests/dedupe-tested.test.sh [path/to/dedupe-tested.sh]
set -uo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)
script=${1:-$repo_root/bin/dedupe-tested.sh}

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

fails=0
pass() { echo "ok   $1"; }
fail() { echo "FAIL $1: $2"; fails=$((fails + 1)); }

# row OS VER ARCH TESTED_AT RESULT [ERROR_CLASS]
row() {
  jq -nc --arg os "$1" --arg v "$2" --arg a "$3" --arg t "$4" --arg r "$5" \
    --arg e "${6:-}" '{os: $os, os_version: $v, arch: $a, tested_at: $t,
      result: $r, image_digest: ("sha256:" + $t)}
      + (if $e == "" then {} else {error_class: $e} end)'
}

# fixture NAME ROW... — a version file in the repo's on-disk format.
fixture() {
  local name=$1; shift
  printf '%s\n' "$@" | jq -s --indent 2 '{tool: "t", version: "1", tested: .}' \
    >"$scratch/$name.json"
  printf '\n' >>"$scratch/$name.json"
  echo "$scratch/$name.json"
}

# run --MODE FILE: sets out and code.
run() { out=$("$script" "$@" 2>&1); code=$?; }

# 1. A uniform group collapses to its newest row; others are untouched.
f=$(fixture uniform \
  "$(row debian 12 amd64 2026-01-01T00:00:00Z pass)" \
  "$(row alpine 3.22 arm64 2026-02-01T00:00:00Z pass)" \
  "$(row debian 12 amd64 2026-03-01T00:00:00Z pass)" \
  "$(row debian 12 amd64 2026-02-01T00:00:00Z pass)")
run --write "$f"
got=$(jq -c '[.tested[] | [.os, .tested_at]]' "$f")
want='[["alpine","2026-02-01T00:00:00Z"],["debian","2026-03-01T00:00:00Z"]]'
if [ "$code" -eq 0 ] && [ "$out" = "$f: 4 -> 2" ] && [ "$got" = "$want" ]; then
  pass "uniform group keeps newest, preserves order"
else
  fail "uniform group" "code=$code out=$out rows=$got"
fi

# 2. Rewrite keeps the on-disk format: the result matches a fresh fixture.
expect=$(fixture expect \
  "$(row alpine 3.22 arm64 2026-02-01T00:00:00Z pass)" \
  "$(row debian 12 amd64 2026-03-01T00:00:00Z pass)")
if cmp -s "$f" "$expect"; then
  pass "rewrite is byte-identical to the canonical format"
else
  fail "format" "$(diff "$expect" "$f")"
fi

# 3. A second --write is a no-op.
before=$(cat "$f")
run --write "$f"
if [ "$code" -eq 0 ] && [ -z "$out" ] && [ "$(cat "$f")" = "$before" ]; then
  pass "idempotent"
else
  fail "idempotent" "code=$code out=$out"
fi

# 4. Mixed results are left as-is and reported.
f=$(fixture mixed \
  "$(row debian 12 amd64 2026-01-01T00:00:00Z pass)" \
  "$(row debian 12 amd64 2026-02-01T00:00:00Z fail)")
before=$(cat "$f")
run --write "$f"
if [ "$code" -eq 0 ] && [ "$out" = "MIXED $f debian/12/amd64" ] \
    && [ "$(cat "$f")" = "$before" ]; then
  pass "mixed results untouched and reported"
else
  fail "mixed results" "code=$code out=$out"
fi

# 5. Same result but differing error_class counts as mixed.
f=$(fixture errclass \
  "$(row debian 12 amd64 2026-01-01T00:00:00Z fail timeout)" \
  "$(row debian 12 amd64 2026-02-01T00:00:00Z fail install)")
before=$(cat "$f")
run --write "$f"
if [ "$code" -eq 0 ] && [ "$out" = "MIXED $f debian/12/amd64" ] \
    && [ "$(cat "$f")" = "$before" ]; then
  pass "differing error_class untouched and reported"
else
  fail "error_class" "code=$code out=$out"
fi

# 6. --check reports and exits 1 without writing; 0 on a clean file.
f=$(fixture check \
  "$(row debian 12 amd64 2026-01-01T00:00:00Z pass)" \
  "$(row debian 12 amd64 2026-02-01T00:00:00Z pass)")
before=$(cat "$f")
run --check "$f"
if [ "$code" -eq 1 ] && [ "$out" = "$f: 2 -> 1" ] && [ "$(cat "$f")" = "$before" ]; then
  pass "--check exits 1 and does not write"
else
  fail "--check dirty" "code=$code out=$out"
fi
run --check "$expect"
if [ "$code" -eq 0 ] && [ -z "$out" ]; then
  pass "--check exits 0 on a clean file"
else
  fail "--check clean" "code=$code out=$out"
fi

# 7. A file without tested[] passes through untouched.
printf '{\n  "tool": "t"\n}\n\n' >"$scratch/none.json"
cp "$scratch/none.json" "$scratch/none.orig"
run --write "$scratch/none.json"
if [ "$code" -eq 0 ] && [ -z "$out" ] \
    && cmp -s "$scratch/none.json" "$scratch/none.orig"; then
  pass "no tested[] key is a no-op"
else
  fail "no tested[]" "code=$code out=$out"
fi

# 8. An unknown flag is a usage error.
run --bogus "$expect"
if [ "$code" -eq 2 ]; then
  pass "unknown flag exits 2"
else
  fail "unknown flag" "code=$code out=$out"
fi

[ "$fails" -eq 0 ] || { echo "$fails case(s) failed"; exit 1; }
