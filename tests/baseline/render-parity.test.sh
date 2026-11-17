#!/usr/bin/env bash
# Render-parity gate for the baseline templates (#558).
#
# Renders every slice x CI-platform combination with baseline/templates/render.sh
# and compares each output tree byte-for-byte against the golden copy under
# tests/baseline/expected/<slices>-<ci>/. The goldens were generated from the
# tree imported at the move commit (librarian 0.18.0 + the pygments pin, the
# same bytes joshjhall/containers' stibbons vendored), so any template change
# shows up here as a reviewable golden diff.
#
# Usage: tests/baseline/render-parity.test.sh [--update]
#   --update  regenerate the goldens instead of comparing (review the diff!)

set -uo pipefail

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
render="$repo_root/baseline/templates/render.sh"
expected="$repo_root/tests/baseline/expected"
update=0
[ "${1:-}" = "--update" ] && update=1

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

failures=0
pass() { printf 'PASS %s\n' "$1"; }
fail() {
    printf 'FAIL %s\n' "$1"
    failures=$((failures + 1))
}

combos=0
for slices in core core,shell core,python core,shell,python; do
    for ci in github gitlab; do
        name="${slices//,/+}-$ci"
        out="$scratch/$name"
        combos=$((combos + 1))
        if ! "$render" --slices "$slices" --ci "$ci" --out "$out" >/dev/null; then
            fail "$name: render.sh exited non-zero"
            continue
        fi
        if [ "$update" = 1 ]; then
            rm -rf "${expected:?}/$name"
            mkdir -p "$expected"
            cp -R "$out" "$expected/$name"
            pass "$name: golden updated"
            continue
        fi
        if [ ! -d "$expected/$name" ]; then
            fail "$name: no golden at tests/baseline/expected/$name (run with --update)"
        elif diff_out=$(diff -r "$expected/$name" "$out" 2>&1); then
            pass "$name: render is byte-identical to the golden"
        else
            fail "$name: render differs from the golden"
            printf '%s\n' "$diff_out" | head -20
        fi
    done
done

# Fail closed: a loop that compared nothing must not report a pass.
[ "$combos" -eq 8 ] || fail "expected 8 slice x CI combinations, ran $combos"

if [ "$failures" -ne 0 ]; then
    printf '%d failure(s)\n' "$failures"
    exit 1
fi
printf 'all %d combinations match\n' "$combos"
