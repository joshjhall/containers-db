#!/usr/bin/env bash
# Gate: a rendered lefthook.yml calls only `just` recipes (#620).
#
#   hook-just-gate.sh <lefthook.yml> <justfile>
#
# The baseline rule (baseline/core.md § 1): lefthook decides when a check runs
# and on which files; the justfile is the only place a tool and its flags
# appear. This checks a RENDERED pair, so it sees the composed result. Fails
# (exit 1, one `FAIL:` line per offender) when:
#
#   1. a segment of a hook `run:` (split on && || ; | & and newlines) does not
#      start with `just `, or carries a command substitution or redirect
#      (`$(`, a backtick, `<`, `>`) — `skip:` entries are exempt, naming the
#      tool there is how the guard works;
#   2. a hook's `skip:` guards `just` itself (just is required, never skipped);
#   3. a hook calls a recipe the justfile does not define;
#   4. a hook with a `glob:` calls a file-taking recipe whose whole-tree
#      `git ls-files` patterns differ from the glob after brace expansion;
#   5. a file-taking recipe has no `git ls-files` line, or more than one.
#
# Exit 2 on bad usage or a missing yq/just. Needs yq (mikefarah v4) and just.
set -euo pipefail

usage() {
    printf 'usage: hook-just-gate.sh <lefthook.yml> <justfile>\n' >&2
    exit 2
}
[ "$#" -eq 2 ] || usage
hooks="$1" justfile="$2"
[ -f "$hooks" ] || usage
[ -f "$justfile" ] || usage
for t in yq just; do
    command -v "$t" >/dev/null 2>&1 || {
        printf 'hook-just-gate.sh: %s not installed\n' "$t" >&2
        exit 2
    }
done

tab="$(printf '\t')"
fails=0
fail() {
    printf 'FAIL: %s\n' "$1"
    fails=$((fails + 1))
}

# One line per fact: `<stage>.<command>\t<kind>\t<value>`, kind in glob|skip|run.
# A run: body's newlines become `;` so a multi-line script splits into segments.
hook_facts="$(yq -r '
  to_entries[] | select(.value | tag == "!!map") | .key as $s
  | (.value.commands // {}) | to_entries[] | .key as $c
  | ( ([(.value.glob // [])] | flatten | .[] | $s + "." + $c + "\tglob\t" + .),
      ((.value.skip // [])[] | select(tag == "!!map") | $s + "." + $c + "\tskip\t" + (.run // "")),
      ($s + "." + $c + "\trun\t" + ((.value.run // "") | sub("\n"; " ; "))) )
' "$hooks")"

# One line per fact: `<recipe>\tparams\t<n variadic params>` and
# `<recipe>\tlsfiles\t<body line naming git ls-files>`.
recipe_facts="$(cd "$(dirname "$justfile")" && just --justfile "$(basename "$justfile")" --dump --dump-format json |
    yq -p json -r '
  .recipes | to_entries[] | .key as $r
  | ( ($r + "\tparams\t" + ([.value.parameters[] | select(.kind != "singular")] | length | tostring)),
      (.value.body[] | [.[] | select(tag == "!!str")] | join("")
        | select(test("git ls-files")) | $r + "\tlsfiles\t" + .) )
')"

[ -n "$hook_facts" ] || {
    fail "$hooks defines no hook commands"
    exit 1
}

# recipe_defined <name> / recipe_takes_files <name>
# Compare the name field exactly: a substring match would let `just md`
# through because `lint-md` exists.
recipe_params() { printf '%s\n' "$recipe_facts" | awk -F"$tab" -v r="$1" '$1 == r && $2 == "params" { print $3 }'; }
recipe_defined() { [ -n "$(recipe_params "$1")" ]; }
recipe_takes_files() { [ "$(recipe_params "$1")" != 0 ]; }

# recipe_patterns <name> — the quoted pathspecs of its git ls-files line(s),
# one per line, sorted.
recipe_patterns() {
    printf '%s\n' "$recipe_facts" | awk -F"$tab" -v r="$1" '$1 == r && $2 == "lsfiles" { print $3 }' |
        sed -n "s/.*git ls-files//p" | grep -oE "'[^']*'" | tr -d "'" | LC_ALL=C sort
}

# expand_glob <glob> — one-level brace expansion (`*.{a,b}` → `*.a` `*.b`).
expand_glob() {
    case "$1" in
        *'{'*'}'*)
            local pre="${1%%\{*}" rest="${1#*\{}" alts post a
            alts="${rest%%\}*}"
            post="${rest#*\}}"
            local IFS=,
            for a in $alts; do printf '%s%s%s\n' "$pre" "$a" "$post"; done
            ;;
        *) printf '%s\n' "$1" ;;
    esac
}

# Rule 5: every file-taking recipe names exactly one git ls-files line.
while IFS="$tab" read -r name kind value; do
    [ "$kind" = params ] && [ "$value" != 0 ] || continue
    n="$(printf '%s\n' "$recipe_facts" | awk -F"$tab" -v r="$name" '$1 == r && $2 == "lsfiles"' | wc -l | tr -d ' ')"
    [ "$n" = 1 ] || fail "recipe '$name' takes files but has $n git ls-files lines (want exactly 1)"
done <<<"$recipe_facts"

commands="$(printf '%s\n' "$hook_facts" | cut -f1 | awk '!seen[$0]++')"
while IFS= read -r cmd; do
    globs="$(printf '%s\n' "$hook_facts" | awk -F"$tab" -v c="$cmd" '$1 == c && $2 == "glob" { print $3 }')"
    skips="$(printf '%s\n' "$hook_facts" | awk -F"$tab" -v c="$cmd" '$1 == c && $2 == "skip" { print $3 }')"
    run="$(printf '%s\n' "$hook_facts" | awk -F"$tab" -v c="$cmd" '$1 == c && $2 == "run" { print $3 }')"

    # Rule 2.
    if printf '%s\n' "$skips" | grep -qE '(^|[^[:alnum:]_-])just([^[:alnum:]_-]|$)'; then
        fail "$cmd: skip: guards just (just is required, never skipped)"
    fi

    # Rule 1 + 3, per segment.
    segments="$(printf '%s\n' "$run" | sed -e 's/&&/\n/g' -e 's/||/\n/g' -e 's/[;|&]/\n/g' |
        sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' | grep -v '^$' || true)"
    if [ -z "$segments" ]; then
        fail "$cmd: empty run:"
        continue
    fi
    recipes=""
    while IFS= read -r seg; do
        case "$seg" in
            *'$('* | *'`'* | *'<'* | *'>'*)
                fail "$cmd: run: segment hides a command or redirect behind just: $seg"
                continue
                ;;
            'just '*) ;;
            *)
                fail "$cmd: run: segment does not call just: $seg"
                continue
                ;;
        esac
        recipe="$(printf '%s\n' "$seg" | awk '{ print $2 }')"
        if recipe_defined "$recipe"; then
            recipes="$recipes $recipe"
        else
            fail "$cmd: calls recipe '$recipe', which the justfile does not define"
        fi
    done <<<"$segments"

    # Rule 4: only a hook that filters by glob AND hands files to the recipe.
    [ -n "$globs" ] || continue
    want="$(while IFS= read -r g; do expand_glob "$g"; done <<<"$globs" | LC_ALL=C sort)"
    for recipe in $recipes; do
        recipe_takes_files "$recipe" || continue
        got="$(recipe_patterns "$recipe")"
        [ "$want" = "$got" ] ||
            fail "$cmd: glob ($(printf '%s' "$want" | tr '\n' ' ')) != recipe '$recipe' git ls-files ($(printf '%s' "$got" | tr '\n' ' '))"
    done
done <<<"$commands"

if [ "$fails" -gt 0 ]; then
    printf '%s: %d problem(s)\n' "$hooks" "$fails"
    exit 1
fi
printf 'ok: %s calls only just recipes that agree with their globs\n' "$hooks"
