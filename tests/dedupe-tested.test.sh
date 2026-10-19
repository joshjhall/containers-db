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

# 8. An unknown flag is a usage error and touches nothing.
before=$(cat "$expect")
run --bogus "$expect"
if [ "$code" -eq 2 ] && [ "$out" = "usage: $script [--check|--write] [file...]" ] \
    && [ "$(cat "$expect")" = "$before" ]; then
  pass "unknown flag exits 2 with usage"
else
  fail "unknown flag" "code=$code out=$out"
fi

# 9. In one file, a uniform tuple collapses while a mixed one keeps every row.
f=$(fixture both \
  "$(row debian 12 amd64 2026-01-01T00:00:00Z pass)" \
  "$(row alpine 3.22 arm64 2026-01-01T00:00:00Z pass)" \
  "$(row debian 12 amd64 2026-02-01T00:00:00Z pass)" \
  "$(row alpine 3.22 arm64 2026-02-01T00:00:00Z fail)")
run --write "$f"
got=$(jq -c '[.tested[] | [.os, .tested_at, .result]]' "$f")
want='[["alpine","2026-01-01T00:00:00Z","pass"],["debian","2026-02-01T00:00:00Z","pass"],["alpine","2026-02-01T00:00:00Z","fail"]]'
if [ "$code" -eq 0 ] \
    && [ "$out" = "$(printf 'MIXED %s alpine/3.22/arm64\n%s: 4 -> 3' "$f" "$f")" ] \
    && [ "$got" = "$want" ]; then
  pass "uniform collapses beside an untouched mixed tuple"
else
  fail "uniform + mixed" "code=$code out=$out rows=$got"
fi

# 10. A file without the trailing blank line gains none; its mode is kept.
f=$(fixture noblank \
  "$(row debian 12 amd64 2026-01-01T00:00:00Z pass)" \
  "$(row debian 12 amd64 2026-02-01T00:00:00Z pass)")
truncate -s -1 "$f"
chmod 640 "$f"
run --write "$f"
if [ "$code" -eq 0 ] && [ "$(tail -c 2 "$f" | od -An -c | tr -d ' ')" = '}\n' ] \
    && [ "$(stat -c %a "$f")" = 640 ] && [ "$(jq '.tested | length' "$f")" = 1 ]; then
  pass "no trailing blank line added; mode preserved"
else
  fail "noblank" "code=$code out=$out mode=$(stat -c %a "$f")"
fi

# 11. The (tuple, result) guard refuses a collapse that loses a fact. The real
# filter cannot produce one, so drive the guard with a mutated scratch copy
# whose uniform-outcome check is removed (mixed groups then collapse too).
mutant="$scratch/mutant.sh"
sed 's/| unique | length) == 1$/| unique | length) >= 1/' "$script" >"$mutant"
chmod +x "$mutant"
if cmp -s "$script" "$mutant"; then
  fail "guard" "mutation did not apply; the filter text changed"
else
  # A collapsible file ahead of the refused one must not be written either.
  ok=$(fixture guard-ok \
    "$(row debian 12 amd64 2026-01-01T00:00:00Z pass)" \
    "$(row debian 12 amd64 2026-02-01T00:00:00Z pass)")
  f=$(fixture guard \
    "$(row debian 12 amd64 2026-01-01T00:00:00Z pass)" \
    "$(row debian 12 amd64 2026-02-01T00:00:00Z fail)")
  before=$(cat "$f"); before_ok=$(cat "$ok")
  out=$("$mutant" --write "$ok" "$f" 2>&1); code=$?
  want=$(printf '%s: 2 -> 1\nMIXED %s debian/12/amd64\n%s: 2 -> 1\nerror: %s: (tuple, result) set changed; refusing to write' "$ok" "$f" "$f" "$f")
  if [ "$code" -eq 1 ] && [ "$out" = "$want" ] && [ "$(cat "$f")" = "$before" ] \
      && [ "$(cat "$ok")" = "$before_ok" ] \
      && [ -z "$(find "$scratch" -name 'guard*.json.*')" ]; then
    pass "guard refuses a fact-losing collapse and writes no file"
  else
    fail "guard" "code=$code out=$out"
  fi
fi

# 13. Newest is chronological, not lexical: RFC3339Nano drops trailing zeros,
# so within one second `53Z` < `53.5Z` < `53.85Z` though `Z` sorts last.
f=$(fixture nanos \
  "$(row debian 12 amd64 2026-10-18T08:15:53.85Z pass)" \
  "$(row debian 12 amd64 2026-10-18T08:15:53Z pass)" \
  "$(row debian 12 amd64 2026-10-18T08:15:53.5Z pass)")
run --write "$f"
got=$(jq -c '[.tested[].tested_at]' "$f")
if [ "$code" -eq 0 ] && [ "$out" = "$f: 3 -> 1" ] \
    && [ "$got" = '["2026-10-18T08:15:53.85Z"]' ]; then
  pass "newest by time across fraction lengths"
else
  fail "nanos" "code=$code out=$out rows=$got"
fi

# 14. A non-UTC tested_at cannot be ordered, so it is refused, not guessed.
f=$(fixture offset \
  "$(row debian 12 amd64 2026-10-18T08:15:53+02:00 pass)" \
  "$(row debian 12 amd64 2026-10-18T08:15:54Z pass)")
before=$(cat "$f")
run --write "$f"
if [ "$code" -ne 0 ] \
    && [[ "$out" == *'tested_at not a UTC RFC3339 timestamp: 2026-10-18T08:15:53+02:00'* ]] \
    && [ "$(cat "$f")" = "$before" ] && [ -z "$(find "$scratch" -name 'offset.json.*')" ]; then
  pass "non-UTC tested_at refused, file untouched"
else
  fail "offset" "code=$code out=$out"
fi

# 15. --write over several files rewrites every collapsible one.
a=$(fixture multi-a \
  "$(row debian 12 amd64 2026-01-01T00:00:00Z pass)" \
  "$(row debian 12 amd64 2026-02-01T00:00:00Z pass)")
b=$(fixture multi-b \
  "$(row alpine 3.22 arm64 2026-01-01T00:00:00Z pass)" \
  "$(row alpine 3.22 arm64 2026-03-01T00:00:00Z pass)")
run --write "$a" "$b"
if [ "$code" -eq 0 ] && [ "$out" = "$(printf '%s: 2 -> 1\n%s: 2 -> 1' "$a" "$b")" ] \
    && [ "$(jq -r '.tested[].tested_at' "$a")" = 2026-02-01T00:00:00Z ] \
    && [ "$(jq -r '.tested[].tested_at' "$b")" = 2026-03-01T00:00:00Z ]; then
  pass "multi-file --write rewrites each file"
else
  fail "multi-file" "code=$code out=$out"
fi

# 16. A bad stamp in a later file (or a missing one) stops the run before any
# file is written, earlier collapsible ones included.
a=$(fixture early \
  "$(row debian 12 amd64 2026-01-01T00:00:00Z pass)" \
  "$(row debian 12 amd64 2026-02-01T00:00:00Z pass)")
b=$(fixture missing \
  "$(row debian 12 amd64 2026-01-01T00:00:00Z pass)" \
  "$(row debian 12 amd64 2026-02-01T00:00:00Z pass | jq -c 'del(.tested_at)')")
before=$(cat "$a")
run --write "$a" "$b"
if [ "$code" -ne 0 ] && [[ "$out" == *'tested_at not a UTC RFC3339 timestamp: null'* ]] \
    && [ "$(cat "$a")" = "$before" ] \
    && [ -z "$(find "$scratch" -name 'early.json.*' -o -name 'missing.json.*')" ]; then
  pass "later bad tested_at writes no file"
else
  fail "later bad tested_at" "code=$code out=$out"
fi

# 17. A non-canonical stamp (no seconds) is refused, not string-ordered.
f=$(fixture short \
  "$(row debian 12 amd64 2026-10-18T08:15Z pass)" \
  "$(row debian 12 amd64 2026-10-18T08:15:54Z pass)")
before=$(cat "$f")
run --write "$f"
if [ "$code" -ne 0 ] \
    && [[ "$out" == *'tested_at not a UTC RFC3339 timestamp: 2026-10-18T08:15Z'* ]] \
    && [ "$(cat "$f")" = "$before" ]; then
  pass "short-form tested_at refused"
else
  fail "short-form" "code=$code out=$out"
fi

# 18. With no flag the default is --check: report, exit 1, write nothing.
f=$(fixture bare \
  "$(row debian 12 amd64 2026-01-01T00:00:00Z pass)" \
  "$(row debian 12 amd64 2026-02-01T00:00:00Z pass)")
before=$(cat "$f")
out=$("$script" "$f" 2>&1); code=$?
if [ "$code" -eq 1 ] && [ "$out" = "$f: 2 -> 1" ] && [ "$(cat "$f")" = "$before" ]; then
  pass "bare invocation defaults to --check"
else
  fail "bare default" "code=$code out=$out"
fi

# 19. A flag after a file is a usage error, not a path.
run "$f" --write
if [ "$code" -eq 2 ] && [ "$out" = "usage: $script [--check|--write] [file...]" ] \
    && [ "$(cat "$f")" = "$before" ]; then
  pass "misplaced flag is a usage error"
else
  fail "misplaced flag" "code=$code out=$out"
fi

# 20. Passing rows that differ in version_output are new evidence: the tuple
# is reported as DIVERGENT and the file is untouched.
f=$(fixture verout \
  "$(row debian 12 amd64 2026-01-01T00:00:00Z pass | jq -c '.version_output = "t 1.0"')" \
  "$(row debian 12 amd64 2026-02-01T00:00:00Z pass | jq -c '.version_output = "t 1.0.1"')")
before=$(cat "$f")
run --write "$f"
if [ "$code" -eq 0 ] && [ "$out" = "DIVERGENT $f debian/12/amd64 version_output" ] \
    && [ "$(cat "$f")" = "$before" ]; then
  pass "differing version_output untouched and reported"
else
  fail "version_output" "code=$code out=$out"
fi

# 21. Rows differing only in the volatile fields still collapse to the newest.
f=$(fixture volatile \
  "$(row debian 12 amd64 2026-01-01T00:00:00Z pass \
    | jq -c '. + {ci_run: "https://ci/1", duration_seconds: 10, version_output: "t 1", dependencies: [{name: "d", version: "2"}]}')" \
  "$(row debian 12 amd64 2026-02-01T00:00:00Z pass \
    | jq -c '. + {ci_run: "https://ci/2", duration_seconds: 12, version_output: "t 1", dependencies: [{name: "d", version: "2"}]}')")
run --write "$f"
got=$(jq -c '[.tested[] | [.tested_at, .ci_run]]' "$f")
if [ "$code" -eq 0 ] && [ "$out" = "$f: 2 -> 1" ] \
    && [ "$got" = '[["2026-02-01T00:00:00Z","https://ci/2"]]' ]; then
  pass "volatile-only differences collapse to newest"
else
  fail "volatile-only" "code=$code out=$out rows=$got"
fi

# 22. Null-vs-populated: an older row's null (or absent) field is subsumed by
# the newest row's value, as with the rust 1.95.0 collapse in #452.
f=$(fixture subsumed \
  "$(row debian 12 amd64 2026-01-01T00:00:00Z pass | jq -c '.dependencies = null')" \
  "$(row debian 12 amd64 2026-01-15T00:00:00Z pass)" \
  "$(row debian 12 amd64 2026-02-01T00:00:00Z pass | jq -c '.dependencies = [{name: "d", version: "2"}]')")
run --write "$f"
got=$(jq -c '[.tested[] | [.tested_at, .dependencies]]' "$f")
if [ "$code" -eq 0 ] && [ "$out" = "$f: 3 -> 1" ] \
    && [ "$got" = '[["2026-02-01T00:00:00Z",[{"name":"d","version":"2"}]]]' ]; then
  pass "older null or absent field is subsumed by newest"
else
  fail "subsumed" "code=$code out=$out rows=$got"
fi

# 23. The reverse is not subsumed: a value an older row records, which the
# newest row has as null or lacks, would be lost, so the tuple is kept.
f=$(fixture lost \
  "$(row debian 12 amd64 2026-01-01T00:00:00Z pass \
    | jq -c '. + {dependencies: [{name: "d", version: "2"}], image_ref: "img:1"}')" \
  "$(row debian 12 amd64 2026-02-01T00:00:00Z pass | jq -c '.dependencies = null')")
before=$(cat "$f")
run --check "$f"
if [ "$code" -eq 0 ] && [ "$out" = "DIVERGENT $f debian/12/amd64 dependencies,image_ref" ] \
    && [ "$(cat "$f")" = "$before" ]; then
  pass "newest null or absent field does not subsume a value"
else
  fail "lost" "code=$code out=$out"
fi

# 24. A divergent tuple keeps every row while a subsumed one beside it
# collapses; the report names only the divergent tuple.
f=$(fixture divmix \
  "$(row debian 12 amd64 2026-01-01T00:00:00Z pass | jq -c '.version_output = "a"')" \
  "$(row alpine 3.22 arm64 2026-01-01T00:00:00Z pass)" \
  "$(row debian 12 amd64 2026-02-01T00:00:00Z pass | jq -c '.version_output = "b"')" \
  "$(row alpine 3.22 arm64 2026-02-01T00:00:00Z pass)")
run --write "$f"
got=$(jq -c '[.tested[] | [.os, .tested_at]]' "$f")
want='[["debian","2026-01-01T00:00:00Z"],["debian","2026-02-01T00:00:00Z"],["alpine","2026-02-01T00:00:00Z"]]'
if [ "$code" -eq 0 ] \
    && [ "$out" = "$(printf 'DIVERGENT %s debian/12/amd64 version_output\n%s: 4 -> 3' "$f" "$f")" ] \
    && [ "$got" = "$want" ]; then
  pass "divergent tuple kept beside a collapsed one"
else
  fail "divergent + uniform" "code=$code out=$out rows=$got"
fi

# 12. With no file args and no version files under tools/, exit 2 clearly.
mkdir -p "$scratch/empty/bin" "$scratch/empty/tools"
cp "$script" "$scratch/empty/bin/dedupe-tested.sh"
out=$("$scratch/empty/bin/dedupe-tested.sh" 2>&1); code=$?
if [ "$code" -eq 2 ] && [ "$out" = "error: no version files under $scratch/empty/tools" ]; then
  pass "no default version files exits 2"
else
  fail "no default files" "code=$code out=$out"
fi

[ "$fails" -eq 0 ] || { echo "$fails case(s) failed"; exit 1; }
