#!/usr/bin/env bash
# Editor config gate for the baseline templates (#560).
#
# The manifest renders `.editorconfig` always, and `.vscode/{settings,extensions}.json`
# / `.zed/settings.json` per IDE selected with `--ide` (default vscode,zed).
# This pins:
#   - `--ide` selects exactly the right editor targets, and refuses an unknown IDE;
#   - every composed editor JSON parses strictly, for every slice x IDE combination;
#   - the editor formatters run with the SAME flags the hooks do: the rendered
#     justfile's `shfmt_flags` and shellcheck severity are read back and compared
#     to the editor files, so a flag bump on one side fails here;
#   - editor files never restate a linter rule (they defer to .shellcheckrc / ruff.toml);
#   - a mutated `shfmt_flags`, shellcheck severity or Python formatter in a
#     scratch template copy makes the parity check fail, so the green above is
#     not vacuous.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# shellcheck source=tests/baseline/lib/harness.sh
source "$SCRIPT_DIR/lib/harness.sh"

TEMPLATES="$REPO_ROOT/baseline/templates"
RENDER="$TEMPLATES/render.sh"
EDITOR_FILES=".editorconfig .vscode/extensions.json .vscode/settings.json .zed/settings.json"

test_suite "repo-baseline editor config"

WORKDIR="$(command mktemp -d)"
trap 'command rm -rf "$WORKDIR"' EXIT

# editor_targets <dir> — the editor files present under <dir>, sorted, one line.
editor_targets() {
    local f found=""
    for f in $EDITOR_FILES; do
        if [ -f "$1/$f" ]; then found="$found $f"; fi
    done
    command printf '%s\n' "${found# }"
}

# mutate <sed-script> <file> — edit a SCRATCH copy in place, portably (BSD sed
# reads `-i`'s next argument as a backup suffix), and fail loudly when the
# script changed nothing, so a mutation test cannot pass on an unapplied edit.
mutate() {
    command sed -e "$1" "$2" >"$2.new"
    if command cmp -s "$2" "$2.new"; then
        command printf 'mutate: %s left %s unchanged\n' "$1" "$2" >&2
        command rm -f "$2.new"
        return 1
    fi
    command mv "$2.new" "$2"
}

# editor_parity <dir> — prints one line per mismatch between the rendered
# justfile's formatter flags and the rendered editor files; empty means they agree.
editor_parity() {
    python3 -I -c '
import configparser, json, os, re, sys
d = sys.argv[1]
out = []
just = open(os.path.join(d, "justfile")).read()
def load(rel):
    p = os.path.join(d, rel)
    return json.load(open(p)) if os.path.exists(p) else None
vs, zed = load(".vscode/settings.json"), load(".zed/settings.json")
ec = configparser.ConfigParser()
ec.read_string("[__root__]\n" + open(os.path.join(d, ".editorconfig")).read())
m = re.search(r"^shfmt_flags := \"([^\"]*)\"", just, re.M)
if m:
    flags = m.group(1).split()
    if vs is not None and vs.get("shellformat.flag", "").split() != flags:
        out.append("vscode shellformat.flag %r != justfile shfmt_flags %r" % (vs.get("shellformat.flag"), flags))
    if zed is not None:
        ext = zed["languages"]["Shell Script"]["formatter"]["external"]
        args = ext["arguments"]
        if ext["command"] != "shfmt" or args[: len(flags)] != flags or args[len(flags):] != ["--filename", "{buffer_path}"]:
            out.append("zed shfmt arguments %r != justfile shfmt_flags %r + filename" % (args, flags))
    i = flags[flags.index("-i") + 1] if "-i" in flags else None
    sh = ec["*.sh"] if ec.has_section("*.sh") else {}
    if sh.get("indent_size") != i:
        out.append(".editorconfig [*.sh] indent_size %r != shfmt -i %r" % (sh.get("indent_size"), i))
    if (sh.get("switch_case_indent") == "true") != ("-ci" in flags):
        out.append(".editorconfig [*.sh] switch_case_indent disagrees with shfmt -ci")
    if zed is not None and str(zed["languages"]["Shell Script"].get("tab_size")) != i:
        out.append("zed Shell Script tab_size != shfmt -i %r" % i)
    sev = re.search(r"shellcheck (--severity=\S+)", just)
    if vs is not None and vs.get("shellcheck.customArgs") != ([sev.group(1)] if sev else None):
        out.append("vscode shellcheck.customArgs %r != justfile lint-sh %r" % (vs.get("shellcheck.customArgs"), sev and sev.group(1)))
elif (vs and "shellformat.flag" in vs) or (zed and "Shell Script" in zed["languages"]):
    out.append("shell editor settings rendered without the shell slice")
if "ruff format" in just:
    if vs is not None and vs.get("[python]", {}).get("editor.defaultFormatter") != "charliermarsh.ruff":
        out.append("vscode [python] formatter is not ruff")
    if zed is not None and zed["languages"]["Python"]["formatter"] != {"language_server": {"name": "ruff"}}:
        out.append("zed Python formatter is not the ruff language server")
# Rules live in .shellcheckrc / ruff.toml only.
for rel in (".vscode/settings.json", ".zed/settings.json"):
    p = os.path.join(d, rel)
    if os.path.exists(p):
        hit = re.findall(r"\"[^\"]*(?:line-length|lineLength|line_length|\.select|\.ignore|\.exclude|\.args\b|disable)[^\"]*\"", open(p).read())
        if hit:
            out.append("%s restates a linter rule: %s" % (rel, ", ".join(hit)))
print("\n".join(out))
' "$1"
}

test_ide_selection() {
    local ide expect out
    for ide in "vscode,zed|.editorconfig .vscode/extensions.json .vscode/settings.json .zed/settings.json" \
        "vscode|.editorconfig .vscode/extensions.json .vscode/settings.json" \
        "zed|.editorconfig .zed/settings.json" \
        "none|.editorconfig" \
        "default|.editorconfig .vscode/extensions.json .vscode/settings.json .zed/settings.json"; do
        expect="${ide#*|}"
        ide="${ide%%|*}"
        out="$WORKDIR/sel-$ide"
        if [ "$ide" = default ]; then
            "$RENDER" --slices shell,python --ci github --out "$out" >/dev/null
        else
            "$RENDER" --slices shell,python --ci github --ide "$ide" --out "$out" >/dev/null
        fi
        assert_equals "$expect" "$(editor_targets "$out")" "--ide $ide writes exactly its editor targets"
    done
}

test_unknown_ide_refused() {
    local out="$WORKDIR/bogus" rc=0
    "$RENDER" --ci github --ide vscode,emacs --out "$out" >/dev/null 2>&1 || rc=$?
    assert_exit 2 "$rc" "An unknown IDE is refused with exit 2"
    assert_true "[ ! -e \"$out\" ]" "A refused IDE writes nothing"
}

test_editor_json_parses() {
    local s ide out f bad=""
    for s in "" shell python shell,python; do
        for ide in vscode zed; do
            out="$WORKDIR/json-${s:-core}-$ide"
            "$RENDER" ${s:+--slices "$s"} --ide "$ide" --out "$out" >/dev/null
            for f in .vscode/extensions.json .vscode/settings.json .zed/settings.json; do
                [ -f "$out/$f" ] || continue
                python3 -I -c 'import json,sys; json.load(open(sys.argv[1]))' "$out/$f" 2>/dev/null ||
                    bad="$bad [${s:-core}/$ide $f]"
            done
        done
    done
    assert_equals "" "$bad" "Every composed editor JSON parses strictly for every slice x IDE"
}

test_formatter_flags_match_hooks() {
    local s out
    for s in "" shell python shell,python; do
        out="$WORKDIR/parity-${s:-core}"
        "$RENDER" ${s:+--slices "$s"} --out "$out" >/dev/null
        assert_equals "" "$(editor_parity "$out")" "Editor formatter flags match the justfile [${s:-core}]"
    done
    assert_contains "$(command cat "$WORKDIR/parity-shell,python/justfile")" 'shfmt_flags := "' \
        "The parity check found shfmt_flags to read (not vacuous)"
}

test_mutated_flags_fail_parity() {
    # Scratch copy, never the tracked template: shfmt now indents 2.
    local copy="$WORKDIR/tmpl-copy" out="$WORKDIR/mutated" got
    command cp -R "$TEMPLATES" "$copy"
    assert_true "mutate 's/^shfmt_flags := \"-i 4 -ci\"\$/shfmt_flags := \"-i 2 -ci\"/' \"$copy/shell/justfile-recipes.just\"" \
        "The shfmt_flags mutation applied to the scratch copy"
    RENDER_MANIFEST="$copy/manifest.tsv" "$copy/render.sh" --slices shell --out "$out" >/dev/null
    got="$(editor_parity "$out")"
    assert_contains "$got" "vscode shellformat.flag" "A drifted shfmt_flags fails the vscode check"
    assert_contains "$got" "zed shfmt arguments" "A drifted shfmt_flags fails the zed check"
    assert_contains "$got" ".editorconfig [*.sh] indent_size" "A drifted shfmt_flags fails the .editorconfig check"
}

test_mutated_severity_and_ruff_fail_parity() {
    # The shellcheck-severity and ruff-formatter branches must also fail when
    # drifted, not only pass on the real templates. Scratch copy again.
    local copy="$WORKDIR/tmpl-copy2" out="$WORKDIR/mutated2" got
    command cp -R "$TEMPLATES" "$copy"
    assert_true "mutate 's/shellcheck --severity=warning/shellcheck --severity=error/' \"$copy/shell/justfile-recipes.just\"" \
        "The severity mutation applied to the scratch copy"
    assert_true "mutate 's/\"charliermarsh.ruff\"/\"ms-python.black-formatter\"/' \"$copy/python/vscode-settings.json.tmpl\"" \
        "The vscode formatter mutation applied to the scratch copy"
    assert_true "mutate 's/\"name\": \"ruff\"/\"name\": \"pylsp\"/' \"$copy/python/zed-settings.json.tmpl\"" \
        "The zed formatter mutation applied to the scratch copy"
    RENDER_MANIFEST="$copy/manifest.tsv" "$copy/render.sh" --slices shell,python --out "$out" >/dev/null
    got="$(editor_parity "$out")"
    assert_contains "$got" "vscode shellcheck.customArgs" "A drifted shellcheck severity fails the vscode check"
    assert_contains "$got" "vscode [python] formatter is not ruff" "A non-ruff vscode Python formatter fails"
    assert_contains "$got" "zed Python formatter is not the ruff language server" "A non-ruff zed Python formatter fails"
}

test_mutated_guards_fail_parity() {
    # The rule-restatement guard and the shell-without-slice branch, each
    # driven to fire on a scratch copy.
    local copy="$WORKDIR/tmpl-copy3" out got t
    t="$(command printf '\t')"
    command cp -R "$TEMPLATES" "$copy"
    assert_true "mutate 's/^  \"editor.formatOnSave\": true,\$/& \"ruff.lineLength\": 100,/' \"$copy/core/editor/vscode-settings-head.json.tmpl\"" \
        "The rule-restatement mutation applied to the scratch copy"
    # Re-own the shell vscode row as core, so it renders without the shell slice.
    assert_true "mutate 's/^\\.vscode\\/settings\\.json${t}shell${t}/.vscode\\/settings.json${t}core${t}/' \"$copy/manifest.tsv\"" \
        "The slice-ownership mutation applied to the scratch manifest"
    out="$WORKDIR/mutated3"
    RENDER_MANIFEST="$copy/manifest.tsv" "$copy/render.sh" --ide vscode --out "$out" >/dev/null
    got="$(editor_parity "$out")"
    assert_contains "$got" "restates a linter rule: \"ruff.lineLength\"" "A restated linter rule fails the guard"
    assert_contains "$got" "shell editor settings rendered without the shell slice" \
        "Shell editor settings without the shell slice fail the guard"
}

test_ide_argument_edges() {
    local out="$WORKDIR/bare" rc=0
    "$RENDER" --ci github --out "$out" --ide >/dev/null 2>&1 || rc=$?
    assert_exit 2 "$rc" "A bare --ide (no value) is a usage error"
    assert_true "[ ! -e \"$out\" ]" "A bare --ide writes nothing"
    out="$WORKDIR/dup"
    "$RENDER" --ci github --ide vscode,vscode --out "$out" >/dev/null
    assert_equals ".editorconfig .vscode/extensions.json .vscode/settings.json" "$(editor_targets "$out")" \
        "A repeated IDE selects it once"
    rc=0
    "$RENDER" --ci github --ide none,emacs --out "$WORKDIR/none-bad" >/dev/null 2>&1 || rc=$?
    assert_exit 2 "$rc" "none does not mask an unknown IDE"
}

run_test test_ide_selection "--ide selects exactly its editor targets (default: both)"
run_test test_unknown_ide_refused "An unknown IDE is refused without writing"
run_test test_editor_json_parses "Composed editor JSON parses for every slice x IDE"
run_test test_formatter_flags_match_hooks "Editor formatter flags equal the hooks' (justfile) flags"
run_test test_mutated_flags_fail_parity "A drifted shfmt_flags fails the parity check"
run_test test_mutated_severity_and_ruff_fail_parity "A drifted severity or non-ruff formatter fails the parity check"
run_test test_mutated_guards_fail_parity "A restated rule or slice-less shell settings fail the parity check"
run_test test_ide_argument_edges "--ide edge inputs: bare flag, duplicates, none+unknown"

generate_report
