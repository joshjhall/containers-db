#!/usr/bin/env bash
# Rendered tool pins stay in lockstep with the catalog (#639).
#
# A rendered repo refreshes its pins by re-rendering from a newer containers-db
# snapshot (core § 4 "Pin maintenance"), never by a bot editing one file. This
# test renders the baseline and checks, against the catalog (tools/<id>/):
#
#   - every tools.versions line AND every tools.sha256 entry match the catalog
#     default_version and that version's pinned_checksum, so a version-only bump
#     (the change a regex bot would produce) fails here, naming the tool;
#   - ruff's required-version (read through the rendered bin/ruff-version.sh)
#     and uv's required-version match the catalog;
#   - no other rendered file carries a second copy of a pinned version;
#   - no Renovate or Dependabot manager targets the pin files.
#
# Mutation cases run on a mktemp copy of tools/ + baseline/templates/, never on
# tracked files, and each asserts the failure NAMES what broke.
#
# Usage: tests/baseline/pin-lockstep.test.sh
set -uo pipefail
export LC_ALL=C

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

command -v jq >/dev/null || {
    echo "FAIL jq is required" >&2
    exit 1
}

fails=0
pass() { echo "ok   $1"; }
fail() {
    echo "FAIL $1: $2"
    fails=$((fails + 1))
}

PIN_FILES_RE='(^|/)(tools\.versions|tools\.sha256|ruff\.toml|uv\.toml|ruff-version\.sh)$'

# check_tree <root> <render-dir> — render <root>'s templates for both hosts
# into <render-dir>, then print one "PROBLEM: ..." line per violation. Empty
# output means the render is in lockstep with <root>/tools.
check_tree() {
    local root=$1 out=$2 host
    for host in github gitlab; do
        if ! "$root/baseline/templates/render.sh" --slices shell,python --ci "$host" \
            --out "$out/$host" >"$out/$host.log" 2>&1; then
            echo "PROBLEM: render --ci $host failed: $(tr '\n' ' ' <"$out/$host.log")"
            return
        fi
    done
    local r="$out/gitlab"

    # 1. tools.versions == catalog default_version, per tool.
    local name v want
    while IFS='=' read -r name v; do
        want=$(jq -r '.default_version // empty' "$root/tools/$name/index.json" 2>/dev/null)
        [ "$v" = "$want" ] ||
            echo "PROBLEM: tools.versions pins $name=$v but the catalog default is '${want:-<none>}'"
    done < <(grep -v '^#' "$r/tools.versions" | grep .)

    # 2. ruff/uv required-version == catalog default_version.
    local got
    got=$(bash "$r/bin/ruff-version.sh" "$r/ruff.toml" 2>&1)
    want=$(jq -r .default_version "$root/tools/ruff/index.json")
    [ "$got" = "$want" ] ||
        echo "PROBLEM: ruff required-version (bin/ruff-version.sh) is '$got' but the catalog default is '$want'"
    got=$(sed -n 's/^required-version = "==\([^"]*\)"$/\1/p' "$r/uv.toml")
    want=$(jq -r .default_version "$root/tools/uv/index.json")
    [ "$got" = "$want" ] ||
        echo "PROBLEM: uv required-version is '$got' but the catalog default is '$want'"

    # 3. every tools.sha256 entry == pinned_checksum of the PINNED version, and
    #    every pinned (tool, arch) has an entry. A version bump without its
    #    hashes trips this, since the old hashes belong to the old version.
    local key hex tool arch ver vf sum
    while read -r key hex; do
        tool=${key%-*} arch=${key##*-}
        if [ "$tool" = ruff ] || [ "$tool" = uv ]; then
            ver=$(sed -n 's/^required-version = "==\([^"]*\)"$/\1/p' "$r/$tool.toml")
        else
            ver=$(awk -F= -v k="$tool" '$1 == k { print $2; exit }' "$r/tools.versions")
        fi
        vf="$root/tools/$tool/versions/$ver.json"
        if [ ! -f "$vf" ]; then
            echo "PROBLEM: $key: pinned version '$ver' has no catalog entry, so its sha256 is unvetted"
            continue
        fi
        sum=$(jq -r --arg a "$arch" '
            .install_methods[]
            | ((.platform.arch // []) | if type == "string" then [.] else . end | sort) as $p
            | select(($a == "x86_64" and $p == ["amd64"]) or ($a == "aarch64" and $p == ["arm64"])
                     or ($a == "any" and $p == ["amd64", "arm64"]))
            | .verification.pinned_checksum // empty' "$vf" | head -1)
        # Two empties compare equal, so refuse a malformed hash or an
        # unvetted (tool, arch) before the equality test can fail open.
        if ! [[ "$hex" =~ ^[0-9a-f]{64}$ ]]; then
            echo "PROBLEM: $key sha256 '$hex' is not a 64-hex digest"
            continue
        fi
        if [ -z "$sum" ]; then
            echo "PROBLEM: $key has no vetted catalog checksum for $tool $ver"
            continue
        fi
        [ "$hex" = "$sum" ] ||
            echo "PROBLEM: $key sha256 $hex does not match the catalog checksum for $tool $ver ('${sum:-<none>}')"
    done < <(grep -v '^#' "$r/tools.sha256" | grep .)

    #    The other direction: every arch the catalog vets for a pinned tool has
    #    a tools.sha256 entry. install-tools.sh refuses a download with none.
    local keys
    while IFS='=' read -r name v; do
        vf="$root/tools/$name/versions/$v.json"
        [ -f "$vf" ] || continue # already reported above
        keys=$(jq -r '.install_methods[]
            | select(.verification.pinned_checksum)
            | ((.platform.arch // []) | if type == "string" then [.] else . end | sort)
            | if . == ["amd64"] then "x86_64" elif . == ["arm64"] then "aarch64"
              elif . == ["amd64", "arm64"] then "any" else empty end' "$vf")
        for arch in $keys; do
            grep -q "^$name-$arch " "$r/tools.sha256" ||
                echo "PROBLEM: no sha256 entry for $name-$arch in tools.sha256; install-tools.sh would refuse it"
        done
    done < <({
        grep -v '^#' "$r/tools.versions" | grep .
        echo "ruff=$(sed -n 's/^required-version = "==\([^"]*\)"$/\1/p' "$r/ruff.toml")"
        echo "uv=$(sed -n 's/^required-version = "==\([^"]*\)"$/\1/p' "$r/uv.toml")"
    })

    # 4. no second copy of a pin: a line naming a tool next to its pinned
    #    version, in any rendered file other than the pin sources. Comment
    #    lines are skipped: prose may cite the version a quirk was seen in.
    local f rel re
    while IFS='=' read -r name v; do
        re="${name}[^[:space:]]{0,2}[@=:v -]{1,3}\"?${v//./\\.}([^0-9.]|$)"
        while IFS= read -r f; do
            rel=${f#"$out"/}
            [[ "$rel" =~ $PIN_FILES_RE ]] && continue
            # Not `grep -q`: an early exit SIGPIPEs the upstream grep, and
            # pipefail would turn a real match into a skip.
            grep -vE '^[[:space:]]*(#|//|<!--)' "$f" | grep -E -- "$re" >/dev/null || continue
            echo "PROBLEM: $rel hardcodes $name $v; read it from the pin file instead"
        done < <(grep -rlE -- "$re" "$out/github" "$out/gitlab")
    done < <({
        grep -v '^#' "$r/tools.versions" | grep .
        echo "ruff=$(jq -r .default_version "$root/tools/ruff/index.json")"
        echo "uv=$(jq -r .default_version "$root/tools/uv/index.json")"
    })

    # 5. no bot manager may target a pin file: it could only bump one of them.
    #    Fail closed: a missing or unparsable config is a PROBLEM, not a pass.
    local cfg managers
    for cfg in "$r/renovate.json" "$out/github/.github/dependabot.yml"; do
        [ -s "$cfg" ] || echo "PROBLEM: ${cfg#"$out"/} is missing or empty; cannot check its managers"
    done
    if ! managers=$(jq -r '.customManagers // [] | length' "$r/renovate.json" 2>&1) ||
        [ -z "$managers" ]; then
        echo "PROBLEM: renovate.json does not parse: $managers"
    elif [ "$managers" != 0 ]; then
        echo "PROBLEM: renovate.json has customManagers; a hosted-Renovate regex bump changes tools.versions without tools.sha256"
    fi
    local hits
    hits=$(grep -nE 'tools\.(versions|sha256)|required-version|ruff\.toml|uv\.toml' \
        "$r/renovate.json" "$out/github/.github/dependabot.yml" 2>/dev/null)
    [ -z "$hits" ] ||
        echo "PROBLEM: a Renovate/Dependabot entry targets a pin file: $(tr '\n' ' ' <<<"$hits")"
}

# fresh — a scratch copy of the real tree; sets $tree.
n=0
fresh() {
    n=$((n + 1))
    tree="$scratch/t$n"
    mkdir -p "$tree/baseline"
    cp -R "$repo_root/tools" "$tree/tools"
    cp -R "$repo_root/baseline/templates" "$tree/baseline/templates"
}

# expect_problem NAME NEEDLE — run check_tree on $tree; pass only if it reports
# a PROBLEM line containing NEEDLE (so a case cannot pass by breaking elsewhere).
expect_problem() {
    local probs o
    o=$(mktemp -d "$tree.out.XXXX")
    probs=$(check_tree "$tree" "$o")
    if grep -F -- "$2" <<<"$probs" | grep -q '^PROBLEM'; then
        pass "$1"
    else
        fail "$1" "no PROBLEM containing '$2'; got: ${probs:-<none>}"
    fi
}

# 1. The real tree is in lockstep.
mkdir -p "$scratch/real"
probs=$(check_tree "$repo_root" "$scratch/real")
if [ -z "$probs" ] && [ -s "$scratch/real/gitlab/tools.versions" ]; then
    pass "rendered pins match the catalog on both hosts"
else
    fail "real tree" "${probs:-render produced no tools.versions}"
fi

# 2. A version-only bump (what a regex bot produces) is caught, naming the tool,
#    by BOTH the versions and the sha256 check.
fresh
sed -i 's/^typos=.*/typos=1.99.0/' "$tree/baseline/templates/core/tools.versions"
expect_problem "version-only bump: tools.versions vs catalog" "pins typos=1.99.0"
expect_problem "version-only bump: sha256 has no vetted entry" "typos-x86_64: pinned version '1.99.0'"

# 3. A version bump that ALSO lands in the catalog but keeps the old hashes is
#    caught: the hashes belong to the old artifact.
fresh
old=$(jq -r .default_version "$tree/tools/dprint/index.json")
jq '.install_methods[].verification.pinned_checksum |= ("f" * 64)' \
    "$tree/tools/dprint/versions/$old.json" >"$tree/tools/dprint/versions/9.9.9.json"
jq '.default_version = "9.9.9"' "$tree/tools/dprint/index.json" >"$tree/x" && mv "$tree/x" "$tree/tools/dprint/index.json"
sed -i 's/^dprint=.*/dprint=9.9.9/' "$tree/baseline/templates/core/tools.versions"
# Guard the setup itself: an empty or schema-drifted 9.9.9.json would make the
# lookup return nothing and still "mismatch", passing for the wrong reason.
fhash=$(printf 'f%.0s' $(seq 64))
if jq -e --arg h "$fhash" '[.install_methods[].verification.pinned_checksum] | length >= 2 and all(. == $h)' \
    "$tree/tools/dprint/versions/9.9.9.json" >/dev/null; then
    pass "mutation 3 setup: 9.9.9.json carries the fabricated hashes"
else
    fail "mutation 3 setup" "9.9.9.json was not built as expected"
fi
# The needle names the catalog's fabricated hash, so only the stale-hash path
# (committed old hash vs vetted new one) can produce it.
expect_problem "catalog+version bump with stale sha256 (x86_64)" "dprint-x86_64 sha256"
expect_problem "...naming the vetted 9.9.9 hash (x86_64)" "catalog checksum for dprint 9.9.9 ('$fhash')"
expect_problem "catalog+version bump with stale sha256 (aarch64)" "dprint-aarch64 sha256"

# 4. A hand-edited ruff required-version is caught through bin/ruff-version.sh.
fresh
sed -i 's/^required-version = "==[^"]*"/required-version = "==0.0.1"/' "$tree/baseline/templates/python/ruff.toml.tmpl"
expect_problem "ruff required-version skew" "ruff required-version (bin/ruff-version.sh) is '0.0.1'"

# 5. A hand-edited uv required-version is caught.
fresh
sed -i 's/^required-version = "==[^"]*"/required-version = "==0.0.1"/' "$tree/baseline/templates/python/uv.toml.tmpl"
expect_problem "uv required-version skew" "uv required-version is '0.0.1'"

# 6. A Renovate customManager on the pin files is refused.
fresh
jq '.customManagers = [{"customType": "regex", "managerFilePatterns": ["/tools\\.versions$/"],
     "matchStrings": ["(?<depName>[a-z]+)=(?<currentValue>.+)"], "datasourceTemplate": "github-releases"}]' \
    "$tree/baseline/templates/core/platform/renovate.json.tmpl" >"$tree/x" &&
    mv "$tree/x" "$tree/baseline/templates/core/platform/renovate.json.tmpl"
expect_problem "renovate customManagers" "renovate.json has customManagers"

# 7. A second copy of a pin in another rendered file is caught.
fresh
printf '\n[default]\nlocale = "en-us" # typos v%s\n' \
    "$(awk -F= '$1 == "typos" { print $2 }' "$tree/baseline/templates/core/tools.versions")" \
    >>"$tree/baseline/templates/core/typos.toml.tmpl"
expect_problem "hardcoded duplicate pin" "_typos.toml hardcodes typos"

# 8. A deleted sha256 line (version kept) is caught: install-tools.sh would
#    refuse the download, so the gate must refuse the render first.
fresh
sed -i '/^shfmt-aarch64 /d' "$tree/baseline/templates/shell/tools.sha256"
expect_problem "missing sha256 entry" "no sha256 entry for shfmt-aarch64"

# 8b. The same holds for ruff/uv, whose version comes from required-version
#     rather than tools.versions.
fresh
sed -i '/^uv-x86_64 /d' "$tree/baseline/templates/python/tools.sha256"
expect_problem "missing sha256 entry (uv, via required-version)" "no sha256 entry for uv-x86_64"

# 8c. A key-only sha256 line (empty hash) is refused, not compared as
#     empty == empty.
fresh
sed -i 's/^\(shfmt-x86_64\) .*/\1/' "$tree/baseline/templates/shell/tools.sha256"
expect_problem "empty sha256 hash" "shfmt-x86_64 sha256 '' is not a 64-hex digest"

# 8d. A sha256 entry for an arch the catalog never vetted is refused.
fresh
printf 'shfmt-any %s\n' "$(printf 'a%.0s' $(seq 64))" >>"$tree/baseline/templates/shell/tools.sha256"
expect_problem "sha256 for an unvetted arch" "shfmt-any has no vetted catalog checksum"

# 8e. A pinned tool with no catalog entry is reported, not skipped.
fresh
echo 'nosuchtool=1.0.0' >>"$tree/baseline/templates/core/tools.versions"
expect_problem "tool missing from catalog" "pins nosuchtool=1.0.0 but the catalog default is '<none>'"

# 9. A Dependabot entry pointing at a pin file is caught.
fresh
printf '    # watches tools.versions\n' >>"$tree/baseline/templates/core/platform/dependabot-actions.yml.tmpl"
expect_problem "dependabot targets a pin file" "Renovate/Dependabot entry targets a pin file"

# 10. A missing renovate.json fails closed rather than passing the guard.
fresh
: >"$tree/baseline/templates/core/platform/renovate.json.tmpl"
expect_problem "empty renovate.json fails closed" "renovate.json is missing or empty"

if [ "$fails" -gt 0 ]; then
    echo "$fails case(s) failed"
    exit 1
fi
echo "all pin-lockstep cases passed"
