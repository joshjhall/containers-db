#!/usr/bin/env bash
# Single test entry point (core § 6): the pre-push hook and CI both call this.
# Each language slice runs as one stage. A stage whose runner is absent exits
# the 77 sentinel and is reported [SKIP] — never [ok]; under CI=true a skip is
# a FAILURE, because CI installs every tool (core § 1). A stage with no test
# files is reported [none], so an empty corpus never reads as a pass.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1

failed=0
skipped=0

# stage <name> <command...>
stage() {
    local name="$1" rc=0
    shift
    "$@" || rc=$?
    case "$rc" in
        0) echo "[ok]   $name" ;;
        77)
            echo "[SKIP] $name did not run (runner not installed)"
            skipped=$((skipped + 1))
            ;;
        78) echo "[none] $name has no test files" ;;
        *)
            echo "[FAIL] $name (exit $rc)"
            failed=$((failed + 1))
            ;;
    esac
}

# --- shell slice: bats-core over tests/*.bats ---
bats_stage() {
    command -v bats >/dev/null 2>&1 || return 77
    compgen -G 'tests/*.bats' >/dev/null || return 78
    bats tests/
}
stage "shell (bats)" bats_stage

# --- python slice: pytest via uv, pinned by tests/requirements.txt ---
pytest_stage() {
    command -v uv >/dev/null 2>&1 || return 77
    compgen -G 'tests/test_*.py' >/dev/null || return 78
    uv run --no-project --with-requirements tests/requirements.txt \
        python -m pytest tests/
}
stage "python (pytest)" pytest_stage

if [ "$failed" -gt 0 ]; then
    echo "tests: $failed stage(s) failed" >&2
    exit 1
fi
if [ "$skipped" -gt 0 ] && [ "${CI:-}" = "true" ]; then
    echo "tests: $skipped stage(s) skipped under CI — a missing tool is a failure here" >&2
    exit 1
fi
exit 0
