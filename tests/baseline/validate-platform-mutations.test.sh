#!/usr/bin/env bash
# Mutation self-test for validate-platform.sh (#561).
#
# A gate that only ever runs on correct inputs is shown to pass, not to work.
# This runs it against scratch copies of the spec and CI template, each broken
# in one way, and requires two things of every mutant:
#   - the gate exits non-zero, and
#   - the failure it reports is the assertion that mutant targets, so a crash
#     or an unrelated check cannot stand in for the one under test.
# The pristine copy must pass first, so a gate that always fails cannot satisfy
# this test, and a mutation that changes nothing is itself a failure.

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

# run_gate <dir> — runs the gate against the copy at <dir>; sets GATE_RC and
# GATE_OUT (colour off, so failure messages match as plain text).
run_gate() {
    GATE_RC=0
    GATE_OUT="$(PLATFORM_DIR="$1/platform" GITHUB_CI="$1/ci.yml" BASELINE_REQUIRE_TOOLS=1 NO_COLOR=1 \
        bash "$GATE" 2>&1)" || GATE_RC=$?
}

# mutant_source <file-relative-to-scratch> — the tracked file it was copied from.
mutant_source() {
    case "$1" in
        ci.yml) command printf '%s' "$SRC_CI" ;;
        platform/*) command printf '%s' "$SRC_PLATFORM/${1#platform/}" ;;
    esac
}

# failed_messages — the gate's failing assertion messages, one per line: the
# lines its harness prints under a FAIL, minus the indented detail lines.
failed_messages() {
    command printf '%s\n' "$GATE_OUT" | command sed -n 's/^      \([^ ].*\)$/\1/p'
}

# mutant <name> <file-relative-to-scratch> <sed-expr> <expected gate message>
mutant() {
    local d="$WORKDIR/$1" changed=yes msgs
    fresh "$d"
    command sed -i -e "$3" "$d/$2"
    # A sed that matched nothing leaves a pristine copy, which passes: catch it
    # here so a renamed fixture cannot turn this into a vacuous result.
    command cmp -s "$d/$2" "$(mutant_source "$2")" && changed=no
    assert_equals "yes" "$changed" "Mutation '$1' changed $2"
    run_gate "$d"
    msgs="$(failed_messages)"
    assert_true "[ $GATE_RC -ne 0 ]" "Mutation '$1' makes the gate exit non-zero (exit $GATE_RC)"
    if ! command grep -qxF -- "$4" <<<"$msgs"; then
        _fail "Mutation '$1' fails the gate on its target assertion" \
            "Expected: $4" "Reported: ${msgs:-(no failing assertion)}"
    fi
}

test_pristine_passes() {
    command -v yq >/dev/null 2>&1 || { skip_test "yq not installed"; return 0; }
    fresh "$WORKDIR/pristine"
    run_gate "$WORKDIR/pristine"
    assert_equals "0" "$GATE_RC" "The gate passes on an unmodified copy"
    assert_equals "" "$(failed_messages)" "...and reports no failing assertion"
}

test_github_mutants() {
    command -v yq >/dev/null 2>&1 || { skip_test "yq not installed"; return 0; }
    mutant rename-gate ci.yml 's/^  merge-gate:/  gate:/' \
        "The GitHub CI template's gate job (needs every other job) is merge-gate"
    mutant wrong-check platform/github.yml 's/context: merge-gate/context: lint/' \
        "github.yml requires exactly the CI template's gate job, nothing else"
    mutant gate-skippable ci.yml '/^  merge-gate:/,/^    if:/ s/if: always()/if: success()/' \
        "The gate job runs even when a stage fails (a skipped required check counts as passed)"
    mutant merge-commits platform/github.yml 's/allow_merge_commit: false/allow_merge_commit: true/' \
        "github.yml is squash-only and deletes the branch on merge"
    mutant no-squash platform/github.yml 's/allow_squash_merge: true/allow_squash_merge: false/' \
        "github.yml is squash-only and deletes the branch on merge"
    mutant rebase-merges platform/github.yml 's/allow_rebase_merge: false/allow_rebase_merge: true/' \
        "github.yml is squash-only and deletes the branch on merge"
    mutant keep-branch platform/github.yml 's/delete_branch_on_merge: true/delete_branch_on_merge: false/' \
        "github.yml is squash-only and deletes the branch on merge"
    mutant any-branch platform/github.yml 's/include: \["~DEFAULT_BRANCH"\]/include: ["~ALL"]/' \
        "The ruleset targets the default branch only"
    mutant force-push platform/github.yml 's/non_fast_forward: true/non_fast_forward: false/' \
        "The ruleset blocks force-push and deletion"
    mutant deletable-branch platform/github.yml 's/deletion: true/deletion: false/' \
        "The ruleset blocks force-push and deletion"
    mutant bad-signatures platform/github.yml 's/^  required_signatures: optional/  required_signatures: sometimes/' \
        "required_signatures is optional or required"
    mutant bad-version platform/github.yml 's/^version: 1/version: 2/' \
        "github.yml parses and declares version: 1"
}

test_gitlab_mutants() {
    command -v yq >/dev/null 2>&1 || { skip_test "yq not installed"; return 0; }
    mutant pipeline-optional platform/gitlab.yml 's/only_allow_merge_if_pipeline_succeeds: true/only_allow_merge_if_pipeline_succeeds: false/' \
        "gitlab.yml requires a succeeded (not skipped) pipeline: GitLab's form of merge-gate"
    mutant skipped-pipeline platform/gitlab.yml 's/allow_merge_on_skipped_pipeline: false/allow_merge_on_skipped_pipeline: true/' \
        "gitlab.yml requires a succeeded (not skipped) pipeline: GitLab's form of merge-gate"
    mutant direct-push platform/gitlab.yml 's/push_access_level: no_access/push_access_level: developer/' \
        "The protected default branch takes no direct push and no force-push"
    mutant gitlab-force-push platform/gitlab.yml 's/allow_force_push: false/allow_force_push: true/' \
        "The protected default branch takes no direct push and no force-push"
    mutant gitlab-bad-version platform/gitlab.yml 's/^version: 1/version: 2/' \
        "gitlab.yml parses and declares version: 1"
    mutant bad-unsigned platform/gitlab.yml 's/reject_unsigned_commits: optional/reject_unsigned_commits: maybe/' \
        "reject_unsigned_commits is optional or required"
}

test_label_mutants() {
    command -v yq >/dev/null 2>&1 || { skip_test "yq not installed"; return 0; }
    mutant unquoted-color platform/labels.yml 's/color: "5319E7"/color: 5319E7/' \
        "Every label's name, color, and description are strings (quote hex colors)"
    mutant short-color platform/labels.yml 's/color: "B60205"/color: "B6020"/' \
        "Every label has a prefix/name, a 6-hex uppercase color, and a description"
    mutant empty-description platform/labels.yml 's/description: Documentation-only change/description: ""/' \
        "Every label has a prefix/name, a 6-hex uppercase color, and a description"
    mutant bad-prefix platform/labels.yml 's/name: type\/docs,/name: kind\/docs,/' \
        "Every label has a prefix/name, a 6-hex uppercase color, and a description"
    mutant dup-label platform/labels.yml 's/name: type\/docs,/name: type\/bug,/' \
        "No label name repeats"
    mutant missing-label platform/labels.yml '/status\/blocked/d' \
        "labels.yml carries every label the issue workflow selects and parks by"
    mutant labels-bad-version platform/labels.yml 's/^version: 1/version: 2/' \
        "labels.yml parses and declares version: 1"
    mutant no-labels platform/labels.yml '/^  - {/d' \
        "labels.yml lists at least one label"
}

run_test test_pristine_passes "The gate passes on the real spec"
run_test test_github_mutants "Each GitHub spec or CI template mutation fails its own assertion"
run_test test_gitlab_mutants "Each GitLab spec mutation fails its own assertion"
run_test test_label_mutants "Each label vocabulary mutation fails its own assertion"

generate_report
