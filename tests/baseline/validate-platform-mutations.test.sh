#!/usr/bin/env bash
# Mutation self-test for validate-platform.sh (#561).
#
# A gate that only ever runs on correct inputs is shown to pass, not to work.
# This runs it against scratch copies of the spec and CI template, each broken
# in one way, and requires a non-zero exit every time. The pristine copy must
# pass first, so a gate that always fails cannot satisfy this test either.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# shellcheck source=tests/baseline/lib/harness.sh
source "$SCRIPT_DIR/lib/harness.sh"

GATE="$SCRIPT_DIR/validate-platform.sh"
SRC_PLATFORM="$REPO_ROOT/baseline/platform"
SRC_CI="$REPO_ROOT/baseline/templates/core/ci/github-ci.yml.tmpl"

test_suite "validate-platform.sh mutation self-test"

WORKDIR="$(command mktemp -d)"
trap 'command rm -rf "$WORKDIR"' EXIT

# fresh <dir> — a scratch copy of the spec and CI template at <dir>.
fresh() {
    command rm -rf "$1"
    command mkdir -p "$1"
    command cp -R "$SRC_PLATFORM" "$1/platform"
    command cp "$SRC_CI" "$1/ci.yml"
}

# gate_rc <dir> — prints the gate's exit code against the copy at <dir>.
gate_rc() {
    local rc=0
    PLATFORM_DIR="$1/platform" GITHUB_CI="$1/ci.yml" BASELINE_REQUIRE_TOOLS=1 \
        bash "$GATE" >/dev/null 2>&1 || rc=$?
    command printf '%s' "$rc"
}

# mutant_source <file-relative-to-scratch> — the tracked file it was copied from.
mutant_source() {
    case "$1" in
        ci.yml) command printf '%s' "$SRC_CI" ;;
        platform/*) command printf '%s' "$SRC_PLATFORM/${1#platform/}" ;;
    esac
}

# mutant <name> <file-relative-to-scratch> <sed-expr> <description>
mutant() {
    local d="$WORKDIR/$1" changed=yes rc
    fresh "$d"
    command sed -i -e "$3" "$d/$2"
    # A sed that matched nothing leaves a pristine copy, which passes: catch it
    # here so a renamed fixture cannot turn this into a vacuous "still fails".
    command cmp -s "$d/$2" "$(mutant_source "$2")" && changed=no
    assert_equals "yes" "$changed" "Mutation '$1' changed $2"
    rc="$(gate_rc "$d")"
    assert_true "[ $rc -ne 0 ]" "$4 (exit $rc)"
}

test_pristine_passes() {
    command -v yq >/dev/null 2>&1 || { skip_test "yq not installed"; return 0; }
    fresh "$WORKDIR/pristine"
    assert_equals "0" "$(gate_rc "$WORKDIR/pristine")" "The gate passes on an unmodified copy"
}

test_mutants_fail() {
    command -v yq >/dev/null 2>&1 || { skip_test "yq not installed"; return 0; }
    mutant rename-gate ci.yml 's/^  merge-gate:/  gate:/' \
        "Renaming the CI gate job fails the gate"
    mutant wrong-check platform/github.yml 's/context: merge-gate/context: lint/' \
        "Requiring a check other than the gate job fails the gate"
    mutant gate-not-always ci.yml '/^  merge-gate:/,/^    if:/ s/if: always()/if: success()/' \
        "A gate job that can be skipped fails the gate"
    mutant unquoted-color platform/labels.yml 's/color: "5319E7"/color: 5319E7/' \
        "An unquoted hex color that YAML reads as a float fails the gate"
    mutant short-color platform/labels.yml 's/color: "B60205"/color: "B6020"/' \
        "A 5-digit color fails the gate"
    mutant dup-label platform/labels.yml 's/name: type\/docs,/name: type\/bug,/' \
        "A duplicated label name fails the gate"
    mutant missing-label platform/labels.yml '/status\/blocked/d' \
        "Dropping a workflow label fails the gate"
    mutant gitlab-skip platform/gitlab.yml 's/allow_merge_on_skipped_pipeline: false/allow_merge_on_skipped_pipeline: true/' \
        "Allowing a merge on a skipped GitLab pipeline fails the gate"
    mutant bad-version platform/github.yml 's/^version: 1/version: 2/' \
        "An unknown spec version fails the gate"
}

run_test test_pristine_passes "The gate passes on the real spec"
run_test test_mutants_fail "Each single mutation of the spec or CI template fails the gate"

generate_report
