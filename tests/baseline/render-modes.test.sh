#!/usr/bin/env bash
# Render modes gate for the baseline templates (#562).
#
# The manifest's optional 5th column marks a target create-only (`once`) or a
# symlink (`link`). This pins what render.sh does with each:
#   - a `once` target is written when absent and never overwritten, even with
#     --force, and an existing one never blocks a render;
#   - a `link` target is a real symlink with the manifest's link text;
#   - the README stub's "Lint and tests" commands exist in the same render;
#   - a malformed mode row is refused before anything is written.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# shellcheck source=tests/baseline/lib/harness.sh
source "$SCRIPT_DIR/lib/harness.sh"

TEMPLATES="$REPO_ROOT/baseline/templates"
MANIFEST="$TEMPLATES/manifest.tsv"
RENDER="$TEMPLATES/render.sh"

test_suite "repo-baseline render modes"

WORKDIR="$(command mktemp -d)"
trap 'command rm -rf "$WORKDIR"' EXIT

test_once_survives_force() {
    local out="$WORKDIR/once" list
    "$RENDER" --slices shell --ci github --out "$out" >/dev/null
    command printf '# Mine\n' >"$out/README.md"
    command printf 'edited\n' >"$out/justfile"
    list="$("$RENDER" --slices shell --ci github --out "$out" --force)"
    assert_equals "# Mine" "$(command cat "$out/README.md")" "--force leaves an edited once target alone"
    assert_not_contains "$(command cat "$out/justfile")" "edited" "--force still rewrites a tracked target"
    assert_not_contains "$list" "README.md" "A skipped once target is not reported as written"
}

test_once_recreated_when_absent() {
    local out="$WORKDIR/recreate" list
    "$RENDER" --slices shell --ci github --out "$out" >/dev/null
    command rm "$out/CLAUDE.md" "$out/docs/.gitkeep"
    list="$("$RENDER" --slices shell --ci github --out "$out" --force)"
    assert_true "[ -s \"$out/CLAUDE.md\" ] && [ -f \"$out/docs/.gitkeep\" ]" "A deleted once target is re-created"
    assert_contains "$list" "CLAUDE.md" "...and reported as written"
}

test_once_does_not_block() {
    # Adopting the baseline into a repo that already has a README must not
    # need --force (which would clobber tracked files the user also has).
    local out="$WORKDIR/adopt" rc=0
    command mkdir -p "$out"
    command printf '# Existing\n' >"$out/README.md"
    command ln -s elsewhere.md "$out/AGENTS.md"
    "$RENDER" --slices python --ci gitlab --out "$out" >/dev/null || rc=$?
    assert_equals "0" "$rc" "An existing once/link target does not refuse a render without --force"
    assert_equals "# Existing" "$(command cat "$out/README.md")" "...and is left as it was"
    assert_equals "elsewhere.md" "$(command readlink "$out/AGENTS.md")" "...including an existing (dangling) link"
}

test_link_is_symlink() {
    local out="$WORKDIR/link"
    "$RENDER" --ci github --out "$out" >/dev/null
    assert_true "[ -L \"$out/AGENTS.md\" ]" "AGENTS.md is rendered as a symlink, not a copy"
    assert_equals "CLAUDE.md" "$(command readlink "$out/AGENTS.md")" "...whose link text is exactly CLAUDE.md"
    assert_equals "$(command cat "$out/CLAUDE.md")" "$(command cat "$out/AGENTS.md")" "...and resolves to the rendered CLAUDE.md"
}

# fenced_commands <readme> — the command lines in its "Lint and tests" section.
fenced_commands() {
    command awk '/^## Lint and tests/ { s = 1; next } /^## / { s = 0 }
        s && /^```/ { f = !f; next } s && f' "$1"
}

test_readme_commands_exist() {
    local slices ci out cmd n recipes
    if ! command -v just >/dev/null 2>&1; then
        skip_test "just not installed"
        return 0
    fi
    for slices in "" shell python shell,python; do
        for ci in github gitlab; do
            out="$WORKDIR/cmd-${slices:-core}-$ci"
            "$RENDER" --slices "$slices" --ci "$ci" --out "$out" >/dev/null
            recipes="$(cd "$out" && just --summary 2>/dev/null | command tr ' ' '\n')"
            n=0
            while IFS= read -r cmd; do
                n=$((n + 1))
                case "$cmd" in
                    "just "*)
                        command grep -qxF "${cmd#just }" <<<"$recipes" ||
                            _fail "README runs '$cmd' but ${slices:-core}/$ci's justfile has no such recipe"
                        ;;
                    *) assert_true "[ -x \"$out/${cmd%% *}\" ]" "README runs $cmd, which ${slices:-core}/$ci renders executable" ;;
                esac
            done < <(fenced_commands "$out/README.md")
            assert_true "[ \"$n\" -ge 4 ]" "${slices:-core}/$ci README lists its lint and test commands (got $n)"
        done
    done
    assert_contains "$(command cat "$WORKDIR/cmd-shell-github/README.md")" "tests/*.bats" "A shell README describes the shell layout"
    assert_not_contains "$(command cat "$WORKDIR/cmd-shell-github/README.md")" "bin/*.py" "...and not the python one"
    assert_not_contains "$(command cat "$WORKDIR/cmd-core-github/README.md")" "tests/*.bats" "A core README names no slice layout"
}

# refused <name> <row> — append <row> to a copy of the manifest; render.sh must
# exit 2 and write nothing.
refused() {
    local m="$WORKDIR/m-$1" rc=0
    command mkdir -p "$m"
    command cp -R "$TEMPLATES/." "$m/"
    command printf '%s\n' "$2" >>"$m/manifest.tsv"
    RENDER_MANIFEST="$m/manifest.tsv" "$RENDER" --slices shell,python --ci github --out "$WORKDIR/o-$1" >/dev/null 2>&1 || rc=$?
    assert_equals "2" "$rc" "$1: render.sh exits 2"
    assert_true "[ ! -e \"$WORKDIR/o-$1\" ]" "$1: ...and writes nothing"
}

test_bad_mode_rows_refused() {
    local t
    t="$(command printf '\t')"
    refused unknown-mode "x.md${t}core${t}always${t}core/docs/claude.md.tmpl${t}sometimes"
    refused mixed-modes "README.md${t}core${t}always${t}core/docs/claude.md.tmpl"
    refused link-twice "AGENTS.md${t}core${t}always${t}README.md${t}link"
    refused link-dotdot "X.md${t}core${t}always${t}../CLAUDE.md${t}link"
    refused link-absolute "X.md${t}core${t}always${t}/etc/passwd${t}link"
    refused link-unrendered "X.md${t}core${t}always${t}nope.md${t}link"
    refused link-to-link "X.md${t}core${t}always${t}AGENTS.md${t}link"
    refused six-columns "x.md${t}core${t}always${t}core/docs/claude.md.tmpl${t}once${t}extra"
}

test_manifest_declares_docs_minimum() {
    # core.md § 5: README, CLAUDE.md, AGENTS.md as a link to it — all create-only.
    assert_contains "$(command cat "$MANIFEST")" "$(command printf 'AGENTS.md\tcore\talways\tCLAUDE.md\tlink')" "AGENTS.md links to CLAUDE.md"
    assert_equals "" "$(command awk -F '\t' '($1 == "README.md" || $1 == "CLAUDE.md") && $5 != "once"' "$MANIFEST")" \
        "Every README.md / CLAUDE.md row is create-only"
}

run_test test_once_survives_force "A once target survives --force"
run_test test_once_recreated_when_absent "A missing once target is re-created"
run_test test_once_does_not_block "An existing once/link target never blocks a render"
run_test test_link_is_symlink "A link target is a symlink with the manifest's text"
run_test test_readme_commands_exist "README stub commands exist in every render"
run_test test_bad_mode_rows_refused "Malformed mode rows are refused without writing"
run_test test_manifest_declares_docs_minimum "The manifest scaffolds the docs minimum"

generate_report
