#!/usr/bin/env bash
# Tests for tests/baseline/hook-just-gate.sh (#620).
#
# Renders a shell+python+github+gitlab scaffold once, then mutates scratch
# copies of its lefthook.yml / justfile — never a tracked file — and asserts the
# gate's exit code AND the offender it names, so a gate that fails for some
# other reason does not count as catching the mutation.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
GATE="$SCRIPT_DIR/hook-just-gate.sh"
RENDER="$REPO_ROOT/baseline/templates/render.sh"

# shellcheck source=tests/baseline/lib/harness.sh
source "$SCRIPT_DIR/lib/harness.sh"

test_suite "hook-just gate (lefthook → just → tool)"

WORKDIR="$(command mktemp -d)"
trap 'command rm -rf "$WORKDIR"' EXIT
"$RENDER" --slices shell,python --ci github,gitlab --out "$WORKDIR/base" >/dev/null

have_tools() {
    local t
    for t in yq just; do
        command -v "$t" >/dev/null 2>&1 || {
            skip_test "$t not installed"
            return 1
        }
    done
}

# mutate <name> <file: lefthook.yml|justfile> <python-replace-old> <new>
# Copies the clean render to $WORKDIR/<name> and replaces one exact string in
# one file; a no-op replacement is a test bug, so it fails loudly.
mutate() {
    local dir="$WORKDIR/$1"
    command cp -R "$WORKDIR/base" "$dir"
    python3 -I -c '
import sys
path, old, new = sys.argv[1:4]
s = open(path).read()
if old not in s:
    sys.exit("mutation target not found: " + old)
open(path, "w").write(s.replace(old, new, 1))
' "$dir/$2" "$3" "$4"
}

# gate_on <name> — runs the gate on $WORKDIR/<name>; sets RC and OUT.
gate_on() {
    RC=0
    OUT="$("$GATE" "$WORKDIR/$1/lefthook.yml" "$WORKDIR/$1/justfile" 2>&1)" || RC=$?
}

test_clean_render_passes() {
    have_tools || return 0
    gate_on base
    assert_equals "0" "$RC" "The clean render passes the gate"
    assert_contains "$OUT" "ok:" "...and says so"
}

test_every_render_passes() {
    have_tools || return 0
    local s c out
    for s in "" shell python shell,python; do
        for c in github gitlab; do
            out="$WORKDIR/all-${s:-core}-$c"
            "$RENDER" ${s:+--slices "$s"} --ci "$c" --out "$out" >/dev/null
            assert_true "\"$GATE\" \"$out/lefthook.yml\" \"$out/justfile\" >/dev/null" \
                "The ${s:-core} × $c render passes the gate"
        done
    done
}

test_direct_tool_fails() {
    have_tools || return 0
    mutate direct lefthook.yml "run: just fmt-sh {staged_files}" "run: shfmt -w -i 4 -ci {staged_files}"
    gate_on direct
    assert_equals "1" "$RC" "A hook calling shfmt directly fails the gate (AC3)"
    assert_contains "$OUT" "pre-commit.shfmt: run: segment does not call just: shfmt -w" \
        "...naming the shfmt hook and its direct call"
}

test_glob_mismatch_fails() {
    have_tools || return 0
    mutate glob lefthook.yml 'glob: "*.toml"' 'glob: "*.{toml,ini}"'
    gate_on glob
    assert_equals "1" "$RC" "A hook glob that disagrees with its recipe's git ls-files fails (AC3)"
    assert_contains "$OUT" "pre-commit.taplo: glob (*.ini *.toml) != recipe 'fmt-toml' git ls-files (*.toml)" \
        "...naming the hook, the glob, and the recipe pattern"
}

test_recipe_pattern_mismatch_fails() {
    have_tools || return 0
    mutate pattern justfile "git ls-files '*.py')" "git ls-files '*.py' '*.pyi')"
    gate_on pattern
    assert_equals "1" "$RC" "A recipe pattern that drifts from its hook glob fails"
    assert_contains "$OUT" "recipe 'fmt-py' git ls-files (*.py *.pyi)" "...naming the drifted recipe"
}

test_skip_guarded_just_fails() {
    have_tools || return 0
    mutate guard lefthook.yml 'run: "! command -v shfmt >/dev/null 2>&1"' 'run: "! command -v just >/dev/null 2>&1"'
    gate_on guard
    assert_equals "1" "$RC" "A skip: guard on just fails the gate (AC4)"
    assert_contains "$OUT" "pre-commit.shfmt: skip: guards just" "...naming the guarded hook"
}

test_tool_in_skip_is_allowed() {
    have_tools || return 0
    # Precondition: the render really guards hooks by naming the tool in skip:,
    # so a pass below proves those entries are exempt rather than absent.
    assert_contains "$(command cat "$WORKDIR/base/lefthook.yml")" 'run: "! command -v shfmt >/dev/null 2>&1"' \
        "The render names shfmt in a skip: guard"
    # A skip: entry that names a tool in a way rule 1 would reject if it scanned
    # skip: bodies: no leading `just `, several segments, a pipe.
    mutate skiptool lefthook.yml 'run: "! command -v shfmt >/dev/null 2>&1"' \
        'run: "shfmt --version | grep -q 3 && ! true"'
    gate_on skiptool
    assert_equals "0" "$RC" "A tool named in skip: does not trip rule 1"
    assert_not_contains "$OUT" "shfmt --version" "...and the skip: body is never reported"
}

test_unknown_recipe_fails() {
    have_tools || return 0
    mutate unknown lefthook.yml "run: just lint-typos" "run: just lint-spelling"
    gate_on unknown
    assert_equals "1" "$RC" "A hook calling an undefined recipe fails the gate"
    assert_contains "$OUT" "pre-push.typos: calls recipe 'lint-spelling', which the justfile does not define" \
        "...naming the hook and the missing recipe"
}

test_multi_segment_direct_tool_fails() {
    have_tools || return 0
    mutate multi lefthook.yml "run: just lint-py && just fmt-py-check" "run: just lint-py && ruff format --check ."
    gate_on multi
    assert_equals "1" "$RC" "A direct tool in a later && segment fails the gate"
    assert_contains "$OUT" "pre-push.ruff: run: segment does not call just: ruff format --check ." \
        "...naming the offending segment, not the clean one"
    assert_not_contains "$OUT" "just lint-py" "The clean first segment is not reported"
}

test_multiline_run_fails() {
    have_tools || return 0
    # Three lines, offender LAST: every newline must split, not just the first.
    mutate multiline lefthook.yml "run: just lint-conflicts {staged_files}" \
        "run: |
        just lint-conflicts {staged_files}
        just lint-large-files {staged_files}
        grep -l x {staged_files}"
    gate_on multiline
    assert_equals "1" "$RC" "A direct tool on the third line of a block run: fails the gate"
    assert_contains "$OUT" "pre-commit.check-merge-conflict: run: segment does not call just: grep -l x" \
        "...naming the third line"
}

test_other_separators_fail() {
    have_tools || return 0
    local sep tool
    for sep in ";" "|" "||"; do
        tool="tee /tmp/x"
        mutate "sep$sep" lefthook.yml "run: just lint-typos" "run: just lint-typos $sep $tool"
        gate_on "sep$sep"
        assert_equals "1" "$RC" "A direct tool after '$sep' fails the gate"
        assert_contains "$OUT" "pre-push.typos: run: segment does not call just: $tool" \
            "...naming the segment after '$sep'"
    done
}

test_recipe_with_two_patterns_fails() {
    have_tools || return 0
    mutate twopat justfile "        list=\"\$(git ls-files '*.md')\"" \
        "        list=\"\$(git ls-files '*.md')\"
        extra=\"\$(git ls-files '*.markdown')\""
    gate_on twopat
    assert_equals "1" "$RC" "A file-taking recipe with two git ls-files lines fails the gate"
    assert_contains "$OUT" "recipe 'lint-md' takes files but has 2 git ls-files lines" "...naming the recipe and count"
}

test_unbraced_glob_mismatch_fails() {
    have_tools || return 0
    mutate unbraced lefthook.yml 'glob: "*.md"' 'glob: "docs/*.md"'
    gate_on unbraced
    assert_equals "1" "$RC" "A glob without braces that disagrees with its recipe fails the gate"
    assert_contains "$OUT" "pre-commit.rumdl: glob (docs/*.md) != recipe 'lint-md' git ls-files (*.md)" \
        "...naming both sides"
}

test_recipe_without_pattern_fails() {
    have_tools || return 0
    mutate nopattern justfile "        list=\"\$(git ls-files '*.md')\"" "        list=\"\$(find . -name '*.md')\""
    gate_on nopattern
    assert_equals "1" "$RC" "A file-taking recipe with no git ls-files line fails the gate"
    assert_contains "$OUT" "recipe 'lint-md' takes files but has 0 git ls-files lines" \
        "...naming the recipe"
}

test_substring_recipe_name_fails() {
    have_tools || return 0
    # `md` is a suffix of the defined `lint-md`: only an exact name match fails it.
    mutate substr lefthook.yml "run: just lint-typos" "run: just md"
    gate_on substr
    assert_equals "1" "$RC" "A hook calling a recipe that is only a substring of a defined one fails"
    assert_contains "$OUT" "pre-push.typos: calls recipe 'md', which the justfile does not define" \
        "...naming the undefined recipe"
}

test_hidden_command_fails() {
    have_tools || return 0
    local body
    for body in 'just lint-typos $(tee /tmp/x)' 'just lint-typos `tee /tmp/x`' 'just lint-typos > /tmp/x'; do
        mutate "hide-${#body}" lefthook.yml "run: just lint-typos" "run: $body"
        gate_on "hide-${#body}"
        assert_equals "1" "$RC" "A segment hiding a command behind just fails: $body"
        assert_contains "$OUT" "pre-push.typos: run: segment hides a command or redirect behind just: $body" \
            "...naming the segment"
    done
    mutate bg lefthook.yml "run: just lint-typos" "run: just lint-typos & tee /tmp/x"
    gate_on bg
    assert_equals "1" "$RC" "A direct tool after a single & fails the gate"
    assert_contains "$OUT" "pre-push.typos: run: segment does not call just: tee /tmp/x" "...naming the backgrounded segment"
}

test_empty_run_fails() {
    have_tools || return 0
    mutate emptyrun lefthook.yml "      run: just lint-typos" "      run: \"\""
    gate_on emptyrun
    assert_equals "1" "$RC" "A hook with an empty run: fails the gate"
    assert_contains "$OUT" "pre-push.typos: empty run:" "...naming the hook"
}

test_no_hook_commands_fails() {
    have_tools || return 0
    command mkdir -p "$WORKDIR/nohooks"
    command printf 'pre-commit:\n  parallel: true\n' >"$WORKDIR/nohooks/lefthook.yml"
    command cp "$WORKDIR/base/justfile" "$WORKDIR/nohooks/justfile"
    gate_on nohooks
    assert_equals "1" "$RC" "A lefthook.yml with no hook commands fails the gate (never a vacuous pass)"
    assert_contains "$OUT" "defines no hook commands" "...and says why"
}

test_multi_recipe_glob_mismatch_fails() {
    have_tools || return 0
    # Two file-taking recipes behind one glob: only the drifted one is named.
    mutate tworec lefthook.yml "run: just lint-py && just fmt-py-check" "run: just lint-py && just lint-sh"
    gate_on tworec
    assert_equals "1" "$RC" "A two-recipe hook whose second recipe disagrees with the glob fails"
    assert_contains "$OUT" "pre-push.ruff: glob (*.py) != recipe 'lint-sh' git ls-files (*.sh)" \
        "...naming the recipe that disagrees"
    assert_not_contains "$OUT" "recipe 'lint-py'" "...and not the one that agrees"
}

test_no_glob_hook_is_not_compared() {
    have_tools || return 0
    # A hook with no glob may call a file-taking recipe: rule 4 does not apply.
    mutate noglob lefthook.yml "run: just lint-typos" "run: just lint-md"
    gate_on noglob
    assert_equals "0" "$RC" "A no-glob hook calling a file-taking recipe passes"
}

test_patternless_recipe_keeps_checking() {
    have_tools || return 0
    # rule 4 asks a recipe with NO quoted pathspec for its patterns; that must
    # not abort the gate before it reaches a direct tool call in a later hook.
    mutate nopat2 justfile "        list=\"\$(git ls-files '*.md')\"" "        list=\"\$(find . -name '*.md')\""
    python3 -I -c '
import sys
p = sys.argv[1]
s = open(p).read()
old = "run: just lint-typos"
assert old in s
open(p, "w").write(s.replace(old, "run: typos", 1))
' "$WORKDIR/nopat2/lefthook.yml"
    gate_on nopat2
    assert_equals "1" "$RC" "A pattern-less recipe behind a glob still fails the gate"
    assert_contains "$OUT" "pre-commit.rumdl: glob (*.md) != recipe 'lint-md' git ls-files ()" \
        "...with the rule-4 diagnostic for that hook"
    assert_contains "$OUT" "pre-push.typos: run: segment does not call just: typos" \
        "...and still reports the direct tool in a LATER hook"
    assert_contains "$OUT" "problem(s)" "...and prints the summary, so it ran to the end"
}

test_missing_justfile_exits_2() {
    local rc=0
    "$GATE" "$WORKDIR/base/lefthook.yml" "$WORKDIR/nope.just" >/dev/null 2>&1 || rc=$?
    assert_equals "2" "$rc" "A missing justfile exits 2"
}

test_bad_usage_exits_2() {
    local rc=0
    "$GATE" >/dev/null 2>&1 || rc=$?
    assert_equals "2" "$rc" "No arguments exits 2"
    rc=0
    "$GATE" "$WORKDIR/nope.yml" "$WORKDIR/base/justfile" >/dev/null 2>&1 || rc=$?
    assert_equals "2" "$rc" "A missing lefthook.yml exits 2"
}

run_test test_clean_render_passes "The clean full render passes"
run_test test_every_render_passes "Every slice × CI render passes"
run_test test_direct_tool_fails "AC3: a hook calling shfmt directly fails"
run_test test_glob_mismatch_fails "AC3: a hook glob/recipe pattern mismatch fails"
run_test test_recipe_pattern_mismatch_fails "A recipe pattern drifting from its glob fails"
run_test test_skip_guarded_just_fails "AC4: a skip-guarded just fails"
run_test test_tool_in_skip_is_allowed "A tool named in skip: is exempt"
run_test test_unknown_recipe_fails "A hook calling an undefined recipe fails"
run_test test_multi_segment_direct_tool_fails "A direct tool in one && segment fails"
run_test test_multiline_run_fails "A direct tool on the third line of a block run: fails"
run_test test_other_separators_fail "A direct tool after ;, | or || fails"
run_test test_recipe_with_two_patterns_fails "A file-taking recipe with two git ls-files lines fails"
run_test test_unbraced_glob_mismatch_fails "An unbraced glob mismatch fails"
run_test test_recipe_without_pattern_fails "A file-taking recipe without git ls-files fails"
run_test test_substring_recipe_name_fails "A recipe name that is only a substring of a defined one fails"
run_test test_hidden_command_fails "A command or redirect hidden behind just, or after &, fails"
run_test test_empty_run_fails "An empty run: fails"
run_test test_no_hook_commands_fails "A lefthook.yml with no commands fails"
run_test test_multi_recipe_glob_mismatch_fails "A two-recipe hook names only the recipe that disagrees with the glob"
run_test test_no_glob_hook_is_not_compared "A no-glob hook is not compared under rule 4"
run_test test_patternless_recipe_keeps_checking "A pattern-less recipe does not abort the gate before later hooks"
run_test test_missing_justfile_exits_2 "A missing justfile exits 2"
run_test test_bad_usage_exits_2 "Bad usage exits 2"

generate_report
