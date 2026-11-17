#!/usr/bin/env bash
# Render modes gate for the baseline templates (#562).
#
# The manifest's optional 5th column marks a target create-only (`once`) or a
# symlink (`link`). This pins what render.sh does with each:
#   - a `once` target is written when absent and never overwritten, even with
#     --force, and an existing one never blocks a render;
#   - a symlink at or above a target the render would write (tracked, or an
#     absent create-only one) is refused even with --force (#627);
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
    "$RENDER" --slices shell --ci github --out "$WORKDIR/once-fresh" >/dev/null
    assert_equals "$(command cat "$WORKDIR/once-fresh/justfile")" "$(command cat "$out/justfile")" \
        "--force still rewrites a tracked target to the fresh render"
    assert_not_contains "$list" "README.md" "A skipped once target is not reported as written"
    command rm "$out/AGENTS.md"
    command ln -s README.md "$out/AGENTS.md"
    "$RENDER" --slices shell --ci github --out "$out" --force >/dev/null
    assert_equals "README.md" "$(command readlink "$out/AGENTS.md")" "--force leaves an existing link alone"
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
    # A dangling symlink at a once FILE target is the user's too: writing
    # through it would create a file wherever it points.
    local dangle="$WORKDIR/adopt-dangle"
    command mkdir -p "$dangle"
    command ln -s nowhere.md "$dangle/README.md"
    "$RENDER" --slices python --ci gitlab --out "$dangle" --force >/dev/null
    assert_equals "nowhere.md" "$(command readlink "$dangle/README.md")" "A dangling once-target symlink is kept, even with --force"
    assert_true "[ ! -e \"$dangle/nowhere.md\" ]" "...and nothing is written through it"
    # A dangling symlink at a TRACKED target must refuse without --force, not
    # silently write through to the link's destination.
    local tracked="$WORKDIR/adopt-tracked" rc=0 err
    command mkdir -p "$tracked"
    command ln -s nowhere.just "$tracked/justfile"
    err="$("$RENDER" --slices python --ci gitlab --out "$tracked" 2>&1 >/dev/null)" || rc=$?
    assert_equals "2" "$rc" "A dangling symlink at a tracked target refuses a render without --force"
    assert_contains "$err" "refusing to overwrite" "...by the overwrite check"
    assert_true "[ ! -e \"$tracked/nowhere.just\" ]" "...and nothing is written through it"
}

# refused_through <case> <out> <expected-error> — a --force render into <out>
# must exit 2 with <expected-error> and create no other target (all-or-nothing).
refused_through() {
    local name="$1" out="$2" want="$3" rc=0 err
    err="$("$RENDER" --slices shell,python --ci github --out "$out" --force 2>&1 >/dev/null)" || rc=$?
    assert_equals "2" "$rc" "$name: --force render exits 2"
    assert_contains "$err" "$want" "$name: ...refused by the symlink check"
    assert_true "[ ! -e \"$out/CLAUDE.md\" ] && [ ! -e \"$out/.github\" ]" "$name: ...and writes no other target"
}

test_force_refuses_tracked_symlink() {
    # #627: --force used to truncate and append through a symlink at a
    # tracked target, writing wherever it pointed (outside --out).
    local out="$WORKDIR/thru-real" dest="$WORKDIR/dest-real.just" err rc
    command mkdir -p "$out"
    command printf 'theirs\n' >"$dest"
    command ln -s "$dest" "$out/justfile"
    refused_through resolving "$out" "refusing to write through symlink $out/justfile"
    assert_equals "theirs" "$(command cat "$dest")" "resolving: ...the link's destination is untouched"
    assert_true "[ -L \"$out/justfile\" ]" "resolving: ...and the link is kept"

    out="$WORKDIR/thru-dangle" dest="$WORKDIR/dest-dangle.just"
    command mkdir -p "$out"
    command ln -s "$dest" "$out/justfile"
    refused_through dangling "$out" "refusing to write through symlink $out/justfile"
    assert_true "[ ! -e \"$dest\" ]" "dangling: ...nothing is created at the link's destination"

    # A symlinked directory above a tracked target redirects mkdir -p and the
    # write just the same.
    out="$WORKDIR/thru-parent" dest="$WORKDIR/elsewhere-dir"
    command mkdir -p "$out" "$dest"
    command ln -s "$dest" "$out/bin"
    refused_through parent "$out" "refusing to write through symlink $out/bin"
    assert_equals "" "$(command ls -A "$dest")" "parent: ...nothing is written into the linked directory"

    # The walk must reach a deeper segment too: .github is real, workflows is
    # the link above the tracked .github/workflows/ci.yml.
    out="$WORKDIR/thru-nested" dest="$WORKDIR/elsewhere-wf"
    command mkdir -p "$out/.github" "$dest"
    command ln -s "$dest" "$out/.github/workflows"
    err="$("$RENDER" --slices shell,python --ci github --out "$out" --force 2>&1 >/dev/null)" && rc=0 || rc=$?
    assert_equals "2" "$rc" "nested: --force render exits 2"
    assert_contains "$err" "refusing to write through symlink $out/.github/workflows" "nested: ...refused at the second segment"
    assert_equals "" "$(command ls -A "$dest")" "nested: ...nothing is written into the linked directory"
    assert_true "[ ! -e \"$out/CLAUDE.md\" ]" "nested: ...and writes no other target"

    # An absent create-only target (docs/.gitkeep is `once`) under a symlinked
    # directory would be created through it just the same.
    out="$WORKDIR/thru-once" dest="$WORKDIR/elsewhere-docs"
    command mkdir -p "$out" "$dest"
    command ln -s "$dest" "$out/docs"
    refused_through once-parent "$out" "refusing to write through symlink $out/docs"
    assert_equals "" "$(command ls -A "$dest")" "once-parent: ...nothing is created in the linked directory"
    # ...but one that already exists through that link is skipped, never refused.
    command touch "$dest/.gitkeep"
    "$RENDER" --slices shell,python --ci github --out "$out" --force >/dev/null
    assert_true "[ -f \"$out/CLAUDE.md\" ]" "once-parent: an existing once target through a linked dir does not block"

    # --out itself being a link is the user's choice of destination.
    command mkdir -p "$WORKDIR/real-out"
    command ln -s "$WORKDIR/real-out" "$WORKDIR/out-link"
    "$RENDER" --ci github --out "$WORKDIR/out-link" --force >/dev/null
    assert_true "[ -f \"$WORKDIR/real-out/justfile\" ]" "A symlinked --out is rendered into"
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
            if ! recipes="$(cd "$out" && just --summary 2>&1)"; then
                _fail "just --summary failed in ${slices:-core}/$ci" "$recipes"
                continue
            fi
            recipes="$(command tr ' ' '\n' <<<"$recipes")"
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

# with_rows <name> <row>... — a copy of the templates whose manifest has <row>s
# appended; prints the copy's manifest path.
with_rows() {
    local m="$WORKDIR/m-$1"
    shift
    command mkdir -p "$m"
    command cp -R "$TEMPLATES/." "$m/"
    command printf '%s\n' "$@" >>"$m/manifest.tsv"
    command printf '%s\n' "$m/manifest.tsv"
}

# refused <name> <expected-error> <row>... — render.sh must exit 2, write
# nothing, and fail with <expected-error>: exit 2 alone is every die(), so a
# case whose own guard was deleted would still pass via a later check.
refused() {
    local name="$1" want="$2" manifest rc=0 err
    shift 2
    manifest="$(with_rows "$name" "$@")"
    err="$(RENDER_MANIFEST="$manifest" "$RENDER" --slices shell,python --ci github --out "$WORKDIR/o-$name" 2>&1 >/dev/null)" || rc=$?
    assert_equals "2" "$rc" "$name: render.sh exits 2"
    assert_contains "$err" "$want" "$name: ...refused by its own check"
    assert_true "[ ! -e \"$WORKDIR/o-$name\" ]" "$name: ...and writes nothing"
}

test_bad_mode_rows_refused() {
    local t claude="core/docs/claude.md.tmpl"
    t="$(command printf '\t')"
    refused unknown-mode "unknown mode 'sometimes'" "x.md${t}core${t}always${t}$claude${t}sometimes"
    refused mixed-once-tracked "README.md mixes modes (once vs -)" "README.md${t}core${t}always${t}$claude"
    # Modes must agree across ALL rows, not just selected ones: this conflicting
    # row is python/gitlab and refused() renders shell,python --ci github.
    refused mixed-unselected "README.md mixes modes (once vs -)" "README.md${t}python${t}ci=gitlab${t}$claude"
    refused mixed-tracked-once "justfile mixes modes (- vs once)" "justfile${t}core${t}always${t}$claude${t}once"
    refused link-twice "link target AGENTS.md has more than one row" "AGENTS.md${t}core${t}always${t}README.md${t}link"
    refused link-dotdot "link text must be relative without '..'" "X.md${t}core${t}always${t}../CLAUDE.md${t}link"
    refused link-absolute "link text must be relative without '..'" "X.md${t}core${t}always${t}/etc/passwd${t}link"
    refused link-unrendered "link X.md -> nope.md: target is not a file rendered" "X.md${t}core${t}always${t}nope.md${t}link"
    refused link-to-link "link X.md -> AGENTS.md: target is not a file rendered" "X.md${t}core${t}always${t}AGENTS.md${t}link"
    # Link text is relative to the link's own directory: docs/X.md -> CLAUDE.md
    # names docs/CLAUDE.md, which this render does not write.
    refused link-subdir-unresolved "link docs/X.md -> CLAUDE.md: target is not a file rendered" \
        "docs/X.md${t}core${t}always${t}CLAUDE.md${t}link"
    # The target exists in the manifest but only for a CI platform this render
    # did not select: the link is validated against the selection, not the file.
    refused link-unselected "link X.md -> .gitlab-ci.yml: target is not a file rendered" \
        "X.md${t}core${t}always${t}.gitlab-ci.yml${t}link"
    refused link-dot "link text must be a plain relative path" "X.md${t}core${t}always${t}./CLAUDE.md${t}link"
    refused link-dot-mid "link text must be a plain relative path" "docs/X.md${t}core${t}always${t}./x.md${t}link"
    refused link-empty-segment "link text must be a plain relative path" "X.md${t}core${t}always${t}docs//x.md${t}link"
    refused six-columns "expected 4 or 5 tab-separated columns" "x.md${t}core${t}always${t}$claude${t}once${t}extra"
}

test_link_in_subdirectory() {
    local t manifest out="$WORKDIR/o-subdir"
    t="$(command printf '\t')"
    manifest="$(with_rows subdir "docs/index.md${t}core${t}always${t}core/docs/claude.md.tmpl${t}once" \
        "docs/latest.md${t}core${t}always${t}index.md${t}link")"
    RENDER_MANIFEST="$manifest" "$RENDER" --ci github --out "$out" >/dev/null
    assert_equals "index.md" "$(command readlink "$out/docs/latest.md")" "A link in a subdirectory keeps its relative text"
    assert_true "[ -f \"$out/docs/latest.md\" ]" "...and resolves to the file beside it"
}

test_manifest_declares_docs_minimum() {
    # core.md § 5: README, CLAUDE.md, AGENTS.md as a link to it — all create-only.
    assert_equals "core always CLAUDE.md link" \
        "$(command awk -F '\t' '$1 == "AGENTS.md" { print $2, $3, $4, $5 }' "$MANIFEST")" \
        "AGENTS.md has exactly one row: a core link to CLAUDE.md"
    assert_equals "" "$(command awk -F '\t' '($1 == "README.md" || $1 == "CLAUDE.md") && $5 != "once"' "$MANIFEST")" \
        "Every README.md / CLAUDE.md row is create-only"
}

run_test test_once_survives_force "A once target survives --force"
run_test test_once_recreated_when_absent "A missing once target is re-created"
run_test test_once_does_not_block "An existing once/link target never blocks a render"
run_test test_force_refuses_tracked_symlink "--force never writes through a symlink at or above a target it writes"
run_test test_link_is_symlink "A link target is a symlink with the manifest's text"
run_test test_readme_commands_exist "README stub commands exist in every render"
run_test test_bad_mode_rows_refused "Malformed mode rows are refused by their own check, without writing"
run_test test_link_in_subdirectory "A link's text is relative to its own directory"
run_test test_manifest_declares_docs_minimum "The manifest scaffolds the docs minimum"

generate_report
