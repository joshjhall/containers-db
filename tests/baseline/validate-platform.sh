#!/usr/bin/env bash
# Gate for the platform settings spec (#561).
#
# baseline/platform/ holds what only a host's API can set (rulesets, merge
# settings, labels); stibbons applies it. This gate pins:
#
#   - every spec file parses and declares `version: 1`;
#   - the GitHub ruleset requires exactly the CI template's gate job, read from
#     the template, so renaming the job without the spec fails here;
#   - the required check is bound to an app when `integration_id` is set, and
#     the ruleset names no bypass actors;
#   - GitLab, whose template has no gate job, requires pipeline success instead;
#   - the label vocabulary is well-formed and carries every label the issue
#     workflow selects and parks work by.
#
# PLATFORM_DIR and GITHUB_CI override the inputs, so a mutation check can point
# the gate at a scratch copy instead of editing a tracked file.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# shellcheck source=tests/baseline/lib/harness.sh
source "$SCRIPT_DIR/lib/harness.sh"
# shellcheck source=tests/baseline/lib/workflow-labels.sh
source "$SCRIPT_DIR/lib/workflow-labels.sh"

PLATFORM_DIR="${PLATFORM_DIR:-$REPO_ROOT/baseline/platform}"
GITHUB_CI="${GITHUB_CI:-$REPO_ROOT/baseline/templates/core/ci/github-ci.yml.tmpl}"

test_suite "repo-baseline platform settings spec"

have_yq() {
    command -v yq >/dev/null 2>&1 && return 0
    skip_test "yq not installed"
    return 1
}

# assert_one_of <value> <message> <allowed>... — plain comparison, so a spec
# value is never spliced into a command string that assert_true evaluates.
assert_one_of() {
    local value="$1" message="$2" ok
    shift 2
    for ok in "$@"; do
        [ "$value" = "$ok" ] && return 0
    done
    _fail "$message" "Actual:   '$value'" "Allowed:  $*"
}

test_spec_files_parse() {
    have_yq || return 0
    local f
    for f in github.yml gitlab.yml labels.yml; do
        assert_true "[ -s \"$PLATFORM_DIR/$f\" ]" "$f exists and is not empty"
        assert_equals "1" "$(yq '.version' "$PLATFORM_DIR/$f" 2>/dev/null)" "$f parses and declares version: 1"
    done
}

test_github_requires_the_ci_gate_job() {
    have_yq || return 0
    local gate required
    # The gate is the job whose `needs` is exactly every other job, compared as
    # a set. `needs` may be a list, a single string (`needs: lint`), or absent,
    # so normalise it to a list first: a string's yq `length` is its character
    # count, which a count-based check would silently mistake for a job count.
    # shellcheck disable=SC2016 # $all is a yq variable, not a shell one
    gate="$(yq '(.jobs | keys) as $all | .jobs | to_entries | .[]
        | select((.value.needs | with(select(tag == "!!null"); . = []) | with(select(tag == "!!str"); . = [.])
            | sort | join(",")) == (($all - [.key]) | sort | join(",")))
        | .key' "$GITHUB_CI")"
    required="$(yq '.ruleset.rules.required_status_checks.required_status_checks[].context' "$PLATFORM_DIR/github.yml")"
    assert_equals "merge-gate" "$gate" "The GitHub CI template's gate job (needs exactly every other job) is merge-gate"
    assert_equals "$gate" "$required" "github.yml requires exactly the CI template's gate job, nothing else"
    assert_equals "always()" "$(yq ".jobs.\"$gate\".if" "$GITHUB_CI")" \
        "The gate job runs even when a stage fails (a skipped required check counts as passed)"
}

test_github_check_source() {
    have_yq || return 0
    local g="$PLATFORM_DIR/github.yml"
    # Tag, not value: a quoted "15368" is a string GitHub's API rejects or, worse,
    # a consumer passes through unchecked. Absent (!!null) is allowed.
    assert_equals "" \
        "$(yq '.ruleset.rules.required_status_checks.required_status_checks[]
            | select((.integration_id | tag) != "!!null" and (.integration_id | tag) != "!!int") | .context' "$g")" \
        "merge-gate's integration_id, when set, is an integer app id"
    assert_equals "!!seq 0" "$(yq '(.ruleset.bypass_actors | tag) + " " + (.ruleset.bypass_actors | length | tostring)' "$g")" \
        "The ruleset declares no bypass actors"
}

test_github_merge_settings() {
    have_yq || return 0
    local g="$PLATFORM_DIR/github.yml"
    assert_equals "true false false true" \
        "$(yq '[.repository.allow_squash_merge, .repository.allow_merge_commit, .repository.allow_rebase_merge, .repository.delete_branch_on_merge] | join(" ")' "$g")" \
        "github.yml is squash-only and deletes the branch on merge"
    assert_equals "~DEFAULT_BRANCH" "$(yq '.ruleset.conditions.ref_name.include | join(" ")' "$g")" \
        "The ruleset targets the default branch only"
    assert_equals "true true" "$(yq '[.ruleset.rules.non_fast_forward, .ruleset.rules.deletion] | join(" ")' "$g")" \
        "The ruleset blocks force-push and deletion"
    assert_one_of "$(yq '.ruleset.required_signatures' "$g")" \
        "required_signatures is optional or required" optional required
}

test_gitlab_requires_pipeline_success() {
    have_yq || return 0
    local g="$PLATFORM_DIR/gitlab.yml"
    assert_equals "true false" \
        "$(yq '[.project.only_allow_merge_if_pipeline_succeeds, .project.allow_merge_on_skipped_pipeline] | join(" ")' "$g")" \
        "gitlab.yml requires a succeeded (not skipped) pipeline: GitLab's form of merge-gate"
    assert_equals "no_access false" \
        "$(yq '[.protected_branch.push_access_level, .protected_branch.allow_force_push] | join(" ")' "$g")" \
        "The protected default branch takes no direct push and no force-push"
    assert_one_of "$(yq '.approval_rule.reject_unsigned_commits' "$g")" \
        "reject_unsigned_commits is optional or required" optional required
}

test_labels_well_formed() {
    have_yq || return 0
    local l="$PLATFORM_DIR/labels.yml" bad dups count
    count="$(yq '.labels | length' "$l")"
    case "$count" in
        '' | 0 | *[!0-9]*) _fail "labels.yml lists at least one label" "Count:    '$count'" ;;
    esac
    # An unquoted color such as 0E8A16 is a YAML float, and `test` on a float
    # errors out. Check the tag first and fail on any yq error, so a parse
    # failure can never read as "no bad labels".
    if bad="$(yq '.labels[] | select((.name | tag) != "!!str" or (.color | tag) != "!!str"
        or (.description | tag) != "!!str") | .name' "$l" 2>&1)"; then
        assert_equals "" "$bad" "Every label's name, color, and description are strings (quote hex colors)"
    else
        _fail "yq could not check label types" "Output:   $bad"
    fi
    if bad="$(yq '.labels[] | select((.name | tag) == "!!str" and (.color | tag) == "!!str")
        | select((.name | test("^(type|severity|effort|status)/[a-z][a-z-]*$") | not)
        or (.color | test("^[0-9A-F]{6}$") | not)
        or ((.description // "") == "")) | .name' "$l" 2>&1)"; then
        assert_equals "" "$bad" "Every label has a prefix/name, a 6-hex uppercase color, and a description"
    else
        _fail "yq could not check label values" "Output:   $bad"
    fi
    dups="$(yq '.labels[].name' "$l" | LC_ALL=C command sort | command uniq -d)"
    assert_equals "" "$dups" "No label name repeats"
}

test_labels_cover_the_workflow() {
    have_yq || return 0
    local names want missing=""
    names="$(yq '.labels[].name' "$PLATFORM_DIR/labels.yml")"
    for want in "${WORKFLOW_LABELS[@]}"; do
        command grep -qxF "$want" <<<"$names" || missing="$missing $want"
    done
    assert_equals "" "$missing" "labels.yml carries every label the issue workflow selects and parks by"
}

run_test test_spec_files_parse "Every spec file parses and is versioned"
run_test test_github_requires_the_ci_gate_job "GitHub requires exactly the CI template's merge-gate job"
run_test test_github_check_source "GitHub binds the required check to an app and allows no bypass"
run_test test_github_merge_settings "GitHub merge settings and ruleset protections"
run_test test_gitlab_requires_pipeline_success "GitLab requires pipeline success and protects the default branch"
run_test test_labels_well_formed "The label vocabulary is well-formed"
run_test test_labels_cover_the_workflow "The label vocabulary covers the issue workflow"

generate_report
