#!/usr/bin/env bash
# Render the repo-baseline tool pins from catalog data (#623).
#
# The catalog (tools/<id>/) is the single source of every baseline tool pin.
# For each tool this reads index.json `.default_version`, then that version's
# versions/<v>.json, and takes each install method's `platform.arch` and Tier 2
# `pinned_checksum`. From those it writes:
#
#   baseline/templates/core/tools.versions     name=version, core slice
#   baseline/templates/shell/tools.versions    name=version, shell slice
#   baseline/templates/{core,shell,python}/tools.sha256
#                                              <tool>-<arch> <sha256>
#   baseline/templates/python/{ruff,uv}.toml.tmpl
#                                              only the required-version line
#
# Arch keys use the installer's uname spelling: amd64 -> x86_64,
# arm64 -> aarch64, and a single method covering both -> any (bats).
#
# Usage:
#   bin/baseline-pins.sh           regenerate the files in place
#   bin/baseline-pins.sh --check   exit 1 with a diff if any committed file
#                                  differs from what the catalog renders
#   BASELINE_PINS_ROOT=<dir>       operate on another tree (tests use this)
#
# Every failure is loud: a missing default_version or versions file, a method
# that is not Tier 2 sha256, a duplicate or unknown arch, or an empty render
# exits non-zero before anything is written.
set -euo pipefail
# Byte-order sorting: en_US collation ignores `-`, so a locale-dependent sort
# could order <tool>-<arch> keys differently on a dev box than in CI and make
# --check report drift that is not there.
export LC_ALL=C

ROOT=${BASELINE_PINS_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
T="$ROOT/baseline/templates"

CORE_TOOLS="typos dprint taplo rumdl actionlint gitleaks conform lefthook just"
SHELL_TOOLS="shellcheck shfmt bats"
# ruff and uv carry their pin as required-version in their own config, which
# the python installer reads; they have no tools.versions line.
PYTHON_TOOLS="ruff uv"

die() {
    echo "ERROR: baseline-pins: $*" >&2
    exit 1
}

mode="write"
case "${1:-}" in
    "") ;;
    --check) mode=check ;;
    *) die "unknown argument '$1' (expected --check or nothing)" ;;
esac
command -v jq >/dev/null || die "jq is required"

# version_of <tool> — the catalog default_version, validated non-empty and
# restricted to version characters: it lands in a file path, a sed replacement
# and the pin files, so `/`, `&`, `..` or whitespace must never get through.
#
# Call it ONLY as a plain assignment (`v=$(version_of t)`): a die inside a
# command substitution used as a command ARGUMENT exits just the subshell and
# its status is discarded, so the caller would carry on with an empty value.
version_of() {
    local idx="$ROOT/tools/$1/index.json" v
    [ -f "$idx" ] || die "$1: no catalog entry at tools/$1/index.json"
    v=$(jq -r '.default_version // empty' "$idx")
    [ -n "$v" ] || die "$1: tools/$1/index.json has no default_version"
    [[ "$v" =~ ^[0-9A-Za-z][0-9A-Za-z.+_-]*$ && "$v" != *..* ]] ||
        die "$1: default_version '$v' is not a plain version string"
    printf '%s\n' "$v"
}

# versions_lines <tool...> — "name=version" per tool, failing loud.
versions_lines() {
    local t v
    for t in "$@"; do
        v=$(version_of "$t")
        printf '%s=%s\n' "$t" "$v"
    done
}

# sha_lines <tool> — "<tool>-<arch> <hex>" lines, sorted, from the version file.
sha_lines() {
    local tool=$1 v f out
    v=$(version_of "$tool")
    f="$ROOT/tools/$tool/versions/$v.json"
    [ -f "$f" ] || die "$tool: default_version $v has no tools/$tool/versions/$v.json"
    out=$(jq -r --arg t "$tool" '
        .install_methods[]
        | if (.verification.tier != 2 or .verification.algorithm != "sha256")
          then "BAD non-tier-2-sha256 method \(.name)"
          elif ((.verification.pinned_checksum // "") | test("^[0-9a-f]{64}$") | not)
          then "BAD pinned_checksum is not 64 lowercase hex in method \(.name)"
          else
            ((.platform.arch // []) | if type == "string" then [.] else . end | sort) as $a
            | (if $a == ["amd64"] then "x86_64"
               elif $a == ["arm64"] then "aarch64"
               elif $a == ["amd64", "arm64"] then "any"
               else "BAD unknown arch \($a | join(",")) in method \(.name)" end) as $k
            | if ($k | startswith("BAD")) then $k
              else "\($t)-\($k) \(.verification.pinned_checksum)" end
          end' "$f")
    if grep -q '^BAD' <<<"$out"; then
        die "$tool $v: $(grep -m1 '^BAD' <<<"$out" | cut -c5-)"
    fi
    [ -n "$out" ] || die "$tool $v: no install methods"
    if [ "$(cut -d' ' -f1 <<<"$out" | sort | uniq -d)" != "" ]; then
        die "$tool $v: duplicate arch: $(cut -d' ' -f1 <<<"$out" | sort | uniq -d | tr '\n' ' ')"
    fi
    sort <<<"$out"
}

# --- renderers: each prints one file to stdout ---

render_core_versions() {
    cat <<'EOF'
# Pinned tool versions — the single source bin/install-tools.sh reads (core § 4).
# One `name=version` per line. GENERATED by containers-db bin/baseline-pins.sh
# from the catalog (tools/<id>/): bump the catalog entry, then regenerate.
EOF
    # shellcheck disable=SC2086 # word-split tool list on purpose
    versions_lines $CORE_TOOLS
}

render_shell_versions() {
    echo "# shell slice"
    # shellcheck disable=SC2086
    versions_lines $SHELL_TOOLS
}

render_core_sha() {
    cat <<'EOF'
# sha256 of every artifact bin/install-tools.sh downloads, keyed <tool>-<arch>.
# install-tools.sh verifies each download against this file BEFORE extracting
# or installing it, and fails on a mismatch or a missing entry. A version pin
# alone does not stop a re-tagged or compromised release.
# GENERATED by containers-db bin/baseline-pins.sh from the catalog's Tier 2
# pinned_checksum values: change the catalog entry, then regenerate.
EOF
    local t
    for t in $CORE_TOOLS; do sha_lines "$t"; done | sort
}

render_slice_sha() { # <slice> <tools...>
    echo "# $1 slice"
    shift
    local t
    for t in "$@"; do sha_lines "$t"; done | sort
}

# render_required_version <tool> — the .tmpl with its required-version line
# replaced; fails if the line is not there exactly once.
render_required_version() {
    local tool=$1 tmpl="$T/python/$1.toml.tmpl" v n
    v=$(version_of "$tool")
    [ -f "$tmpl" ] || die "$tool: missing $tmpl"
    n=$(grep -c '^required-version = "==' "$tmpl" || true)
    [ "$n" = 1 ] || die "$tool: expected exactly one required-version line in $tmpl, found $n"
    sed "s/^required-version = \"==[^\"]*\"/required-version = \"==$v\"/" "$tmpl"
}

# --- render everything into a scratch tree first, so a failure writes nothing ---

OUT=$(mktemp -d)
trap 'rm -rf "$OUT"' EXIT
mkdir -p "$OUT/core" "$OUT/shell" "$OUT/python"

# Each renderer runs in its own command so `set -e` sees a die inside it.
render_core_versions >"$OUT/core/tools.versions"
render_shell_versions >"$OUT/shell/tools.versions"
render_core_sha >"$OUT/core/tools.sha256"
# shellcheck disable=SC2086 # word-split tool lists on purpose
render_slice_sha shell $SHELL_TOOLS >"$OUT/shell/tools.sha256"
# shellcheck disable=SC2086
render_slice_sha python $PYTHON_TOOLS >"$OUT/python/tools.sha256"
for t in $PYTHON_TOOLS; do
    render_required_version "$t" >"$OUT/python/$t.toml.tmpl"
done

FILES="core/tools.versions shell/tools.versions core/tools.sha256 shell/tools.sha256
python/tools.sha256 python/ruff.toml.tmpl python/uv.toml.tmpl"

for f in $FILES; do
    [ -s "$OUT/$f" ] || die "rendered $f is empty"
done

if [ "$mode" = check ]; then
    drift=0
    for f in $FILES; do
        if ! diff -u --label "committed baseline/templates/$f" --label "catalog-rendered $f" \
            "$T/$f" "$OUT/$f"; then
            drift=1
        fi
    done
    if [ "$drift" = 1 ]; then
        echo "ERROR: baseline pin files differ from the catalog. Edit tools/<id>/ and run bin/baseline-pins.sh." >&2
        exit 1
    fi
    echo "baseline pins OK: $(wc -w <<<"$FILES") files match the catalog"
else
    for f in $FILES; do
        cp "$OUT/$f" "$T/$f"
    done
    echo "baseline pins written: $(wc -w <<<"$FILES") files from the catalog"
fi
