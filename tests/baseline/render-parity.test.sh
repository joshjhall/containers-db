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

# modes <dir> — "<octal mode> <relative path>" per file and "link <path> -> <text>"
# per symlink, sorted. diff -r compares bytes only (and follows links), so this
# is what catches a lost exec bit or a link rendered as a copy (#562).
modes() {
    (cd "$1" && {
        find . -type f -exec stat -c '%a %n' {} +
        find . -type l -exec sh -c 'for l; do printf "link %s -> %s\n" "$l" "$(readlink "$l")"; done' sh {} +
    } | sort)
}

combos=0
names=""
# Default renders select both IDEs (vscode,zed); two extra combos pin the
# single-IDE selections (#560).
for combo in core core,shell core,python core,shell,python core,shell,python:vscode core,shell,python:zed; do
    slices="${combo%%:*}"
    ide=""
    case "$combo" in *:*) ide="${combo#*:}" ;; esac
    for ci in github gitlab; do
        [ -z "$ide" ] || [ "$ci" = github ] || continue
        name="${slices//,/+}-$ci${ide:+-$ide}"
        out="$scratch/$name"
        combos=$((combos + 1))
        names="$names$name"$'\n'
        if ! "$render" --slices "$slices" --ci "$ci" ${ide:+--ide "$ide"} --out "$out" >/dev/null; then
            fail "$name: render.sh exited non-zero"
            continue
        fi
        # Fail closed: a render that wrote nothing would trivially match an
        # empty golden, so an empty tree is a failure, never a pass.
        if [ -z "$(find "$out" -type f -print -quit)" ]; then
            fail "$name: render wrote no files"
            continue
        fi
        if [ "$update" = 1 ]; then
            rm -rf "${expected:?}/$name"
            mkdir -p "$expected"
            cp -RP "$out" "$expected/$name"
            pass "$name: golden updated"
            continue
        fi
        if [ ! -d "$expected/$name" ]; then
            fail "$name: no golden at tests/baseline/expected/$name (run with --update)"
        elif ! diff_out=$(diff -r --no-dereference "$expected/$name" "$out" 2>&1); then
            fail "$name: render differs from the golden"
            printf '%s\n' "$diff_out" | head -20
        elif ! mode_out=$(diff <(modes "$expected/$name") <(modes "$out") 2>&1); then
            fail "$name: file modes differ from the golden"
            printf '%s\n' "$mode_out" | head -20
        else
            pass "$name: render is byte- and mode-identical to the golden"
        fi
    done
done

# A golden no combination produces is stale: it would never be compared, so a
# renamed or dropped combination would silently stop being checked.
if [ "$update" = 0 ] && [ -d "$expected" ]; then
    for dir in "$expected"/*/; do
        [ -d "$dir" ] || continue
        golden=$(basename "$dir")
        case "$names" in
            *"$golden"$'\n'*) ;;
            *) fail "$golden: stale golden; no rendered combination produces it" ;;
        esac
    done
fi

if [ "$failures" -ne 0 ]; then
    printf '%d failure(s)\n' "$failures"
    exit 1
fi
printf 'all %d combinations match\n' "$combos"
