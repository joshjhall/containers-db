# shellcheck shell=bash
# The labels the issue workflow selects and parks work by (#561, #633).
# validate-platform.sh requires each one in labels.yml, and
# validate-platform-mutations.test.sh deletes each one in turn, so a label
# added here is both required and mutation-tested.
# shellcheck disable=SC2034 # read by the scripts that source this file
WORKFLOW_LABELS=(
    severity/critical severity/high severity/medium severity/low
    effort/trivial effort/small effort/medium effort/large
    status/in-progress status/pr-pending status/commit-pending status/on-hold status/blocked
    type/bug type/feature
)
