#!/usr/bin/env bash
# Tests for bin/baseline-pins.sh (#623). Every mutation runs on a mktemp copy
# of tools/ and baseline/templates/, never on tracked files. Each case asserts
# the exit code AND that the output names the thing that drifted or broke, so a
# case cannot pass by failing for an unrelated reason.
#
# Usage: tests/baseline-pins.test.sh [path/to/baseline-pins.sh]
set -uo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)
script=${1:-$repo_root/bin/baseline-pins.sh}

scratch_root=$(mktemp -d)
trap 'rm -rf "$scratch_root"' EXIT

fails=0
pass() { echo "ok   $1"; }
fail() { echo "FAIL $1: $2"; fails=$((fails + 1)); }

n=0
# fresh — a new scratch tree; sets $tree.
fresh() {
    n=$((n + 1))
    tree="$scratch_root/t$n"
    mkdir -p "$tree/baseline"
    cp -R "$repo_root/tools" "$tree/tools"
    cp -R "$repo_root/baseline/templates" "$tree/baseline/templates"
}

# run ARGS... — runs the script against $tree; sets out and code.
run() { out=$(BASELINE_PINS_ROOT="$tree" "$script" "$@" 2>&1); code=$?; }

# expect NAME WANT_CODE NEEDLE — assert exit code and a fixed-string match.
expect() {
    if [ "$code" != "$2" ]; then
        fail "$1" "exit $code, want $2; output: $out"
    elif ! grep -qF -- "$3" <<<"$out"; then
        fail "$1" "output lacks '$3'; output: $out"
    else
        pass "$1"
    fi
}

# set_json FILE JQ — rewrite a catalog file in the scratch tree.
set_json() {
    local tmp="$1.tmp"
    jq "$2" "$1" >"$tmp" && mv "$tmp" "$1"
}

T=baseline/templates

# 1. The real tree is in sync.
fresh
run --check
expect "committed pins match the catalog" 0 "7 files match the catalog"

# 2. Hand-editing a hash in tools.sha256 is caught, naming the line.
fresh
sed -i 's/^gitleaks-x86_64 [0-9a-f]*/gitleaks-x86_64 0000000000000000000000000000000000000000000000000000000000000000/' "$tree/$T/core/tools.sha256"
run --check
expect "hand-edited sha256 is drift" 1 "-gitleaks-x86_64 0000"

# 3. Hand-editing a version in tools.versions is caught.
fresh
sed -i 's/^shfmt=.*/shfmt=9.9.9/' "$tree/$T/shell/tools.versions"
run --check
expect "hand-edited tools.versions is drift" 1 "-shfmt=9.9.9"

# 4. Hand-editing ruff's required-version is caught.
fresh
sed -i 's/^required-version = .*/required-version = "==0.0.1"/' "$tree/$T/python/ruff.toml.tmpl"
run --check
expect "hand-edited ruff required-version is drift" 1 '-required-version = "==0.0.1"'

# 5. Changing a catalog checksum without regenerating is caught.
fresh
v=$(jq -r .default_version "$tree/tools/just/index.json")
set_json "$tree/tools/just/versions/$v.json" \
    '(.install_methods[] | select(.platform.arch == ["arm64"]) | .verification.pinned_checksum) = ("a" * 64)'
run --check
expect "catalog checksum change without regen is drift" 1 "+just-aarch64 aaaa"

# 6. Regenerating after that catalog change puts the new hash in place, and
#    --check then passes (the write path, end to end).
run
if [ "$code" = 0 ] && grep -qx "just-aarch64 $(printf 'a%.0s' $(seq 64))" "$tree/$T/core/tools.sha256"; then
    run --check
    expect "regenerate writes the catalog hash" 0 "7 files match the catalog"
else
    fail "regenerate writes the catalog hash" "exit $code; output: $out"
fi

# 7. A catalog version bump flows into uv's required-version.
fresh
old=$(jq -r .default_version "$tree/tools/uv/index.json")
cp "$tree/tools/uv/versions/$old.json" "$tree/tools/uv/versions/9.8.7.json"
set_json "$tree/tools/uv/versions/9.8.7.json" '.version = "9.8.7"'
set_json "$tree/tools/uv/index.json" '.default_version = "9.8.7"'
run
if [ "$code" = 0 ] && grep -qx 'required-version = "==9.8.7"' "$tree/$T/python/uv.toml.tmpl"; then
    pass "version bump reaches uv required-version"
else
    fail "version bump reaches uv required-version" "exit $code; $(grep required-version "$tree/$T/python/uv.toml.tmpl")"
fi

# 8. default_version pointing at a missing versions file fails, writes nothing.
fresh
set_json "$tree/tools/typos/index.json" '.default_version = "0.0.0"'
before=$(cat "$tree/$T/core/tools.versions")
run
expect "missing versions file fails" 1 "typos: default_version 0.0.0 has no tools/typos/versions/0.0.0.json"
if [ "$(cat "$tree/$T/core/tools.versions")" != "$before" ]; then
    fail "failed run writes nothing" "core/tools.versions changed"
else
    pass "failed run writes nothing"
fi

# 9. A non-Tier-2 method is refused.
fresh
v=$(jq -r .default_version "$tree/tools/taplo/index.json")
set_json "$tree/tools/taplo/versions/$v.json" \
    '.install_methods[0].verification = {tier: 3, algorithm: "sha256", checksum_url_template: "https://example.invalid/x"}'
run --check
expect "tier-3 method is refused" 1 "taplo $v: non-tier-2-sha256 method"

# 10. Two methods for the same arch are refused.
fresh
v=$(jq -r .default_version "$tree/tools/rumdl/index.json")
set_json "$tree/tools/rumdl/versions/$v.json" '.install_methods[1].platform.arch = ["amd64"]'
run --check
expect "duplicate arch is refused" 1 "rumdl $v: duplicate arch: rumdl-x86_64"

# 11. A missing catalog entry is refused.
fresh
rm -rf "$tree/tools/lefthook"
run --check
expect "missing catalog entry is refused" 1 "lefthook: no catalog entry"

# tree_sum — one checksum over the whole scratch templates tree.
tree_sum() { (cd "$tree/$T" && find . -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum); }

# 13. Drift in EACH wholly generated file is caught (a file missing from
#     FILES, or a renderer for it gone wrong, would otherwise pass). The two
#     python .toml.tmpl files are hand-authored except their required-version
#     line, so only that line is drift there: ruff is case 4, uv is below.
for f in core/tools.versions shell/tools.versions core/tools.sha256 \
    shell/tools.sha256 python/tools.sha256; do
    fresh
    printf 'hand-edit-marker\n' >>"$tree/$T/$f"
    run --check
    expect "drift caught in $f" 1 "committed baseline/templates/$f"
done
fresh
sed -i 's/^required-version = .*/required-version = "==0.0.2"/' "$tree/$T/python/uv.toml.tmpl"
run --check
expect "hand-edited uv required-version is drift" 1 '-required-version = "==0.0.2"'

# 14. --check never modifies the tree, even on drift.
fresh
sed -i 's/^typos=.*/typos=0.0.1/' "$tree/$T/core/tools.versions"
before=$(tree_sum)
run --check
if [ "$code" = 1 ] && [ "$(tree_sum)" = "$before" ]; then
    pass "--check leaves the tree untouched"
else
    fail "--check leaves the tree untouched" "exit $code or tree changed"
fi

# 15. A core tool with no default_version fails and writes NOTHING. This is
#     the case where a die inside a printf argument used to be swallowed.
fresh
set_json "$tree/tools/typos/index.json" 'del(.default_version)'
before=$(tree_sum)
run
expect "missing default_version fails" 1 "typos: tools/typos/index.json has no default_version"
if [ "$(tree_sum)" = "$before" ]; then
    pass "missing default_version writes nothing"
else
    fail "missing default_version writes nothing" "templates tree changed"
fi

# 16. A default_version that could escape a path or a sed replacement is refused.
fresh
set_json "$tree/tools/shfmt/index.json" '.default_version = "../../x"'
run --check
expect "path-like default_version is refused" 1 "shfmt: default_version '../../x' is not a plain version string"
fresh
set_json "$tree/tools/ruff/index.json" '.default_version = "1.0/&"'
run --check
expect "sed-metachar default_version is refused" 1 "ruff: default_version '1.0/&' is not a plain version string"

# 17. A malformed pinned_checksum is refused (it would land in the
#     integrity file install-tools.sh trusts).
fresh
v=$(jq -r .default_version "$tree/tools/dprint/index.json")
set_json "$tree/tools/dprint/versions/$v.json" '.install_methods[0].verification.pinned_checksum = "ABCDEF"'
run --check
expect "malformed pinned_checksum is refused" 1 "dprint $v: pinned_checksum is not 64 lowercase hex"

# 18. An arch outside amd64/arm64 is refused.
fresh
v=$(jq -r .default_version "$tree/tools/typos/index.json")
set_json "$tree/tools/typos/versions/$v.json" '.install_methods[0].platform.arch = ["riscv64"]'
run --check
expect "unknown arch is refused" 1 "typos $v: unknown arch riscv64"

# 19. A string-valued arch is normalised like a one-element array.
fresh
v=$(jq -r .default_version "$tree/tools/typos/index.json")
set_json "$tree/tools/typos/versions/$v.json" '(.install_methods[] | select(.platform.arch == ["arm64"]) | .platform.arch) = "arm64"'
run --check
expect "string arch is accepted" 0 "7 files match the catalog"

# 20. A tool with no install methods is refused.
fresh
v=$(jq -r .default_version "$tree/tools/conform/index.json")
set_json "$tree/tools/conform/versions/$v.json" '.install_methods = []'
run --check
expect "empty install_methods is refused" 1 "conform $v: no install methods"

# 21. A ruff template without exactly one required-version line is refused.
fresh
sed -i '/^required-version/d' "$tree/$T/python/ruff.toml.tmpl"
run --check
expect "missing required-version line is refused" 1 "found 0"
fresh
line=$(grep "^required-version" "$tree/$T/python/uv.toml.tmpl")
printf "%s\n" "$line" >>"$tree/$T/python/uv.toml.tmpl"
run --check
expect "duplicated required-version line is refused" 1 "found 2"

# 12. An unknown argument is refused.
fresh
run --bogus
expect "unknown argument is refused" 1 "unknown argument '--bogus'"

echo
if [ "$fails" -gt 0 ]; then
    echo "$fails case(s) failed"
    exit 1
fi
echo "all cases passed"
