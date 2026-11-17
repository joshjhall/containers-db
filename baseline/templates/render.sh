#!/usr/bin/env bash
# Render the repo-baseline scaffold for a chosen set of slices and CI platforms.
#
#   render.sh --slices shell,python --ci github,gitlab --out DIR [--force]
#
# Reads manifest.tsv beside this script: every row whose slice is selected
# (`core` always is) and whose `when` holds is written to DIR/<target>; rows
# sharing a target are concatenated in manifest order. No templating language,
# no network — the same bytes a plain `cat` loop would produce, which is the
# contract a non-LLM renderer (stibbons) implements independently. Format and
# rationale: README.md.
#
# An optional 5th column, `mode`, marks a target create-only (`once`: written
# only when absent, even under --force) or a symlink (`link`: `source` is the
# link text, naming another target of the same render; also create-only).
#
# Refuses (exit 2) an unknown slice or CI platform, a manifest row with an
# unknown `when` or `mode` or a missing source, an absolute or `..` target, a
# target whose rows disagree on `mode`, a bad link, an existing tracked target
# unless --force, and, even with --force, a symlink at or above a tracked
# target inside --out. Nothing is written until every check passes. Prints
# each target it wrote. bash-3.2 clean.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MANIFEST="${RENDER_MANIFEST:-$HERE/manifest.tsv}"
TEMPLATES="$(cd "$(dirname "$MANIFEST")" && pwd)"

die() {
    printf 'render.sh: %s\n' "$1" >&2
    exit 2
}

usage() {
    printf 'usage: render.sh --slices a,b --ci github,gitlab --out DIR [--force]\n' >&2
    exit 2
}

slices="" ci="" out="" force=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --slices)
            [ "$#" -ge 2 ] || usage
            slices="$2"
            shift 2
            ;;
        --ci)
            [ "$#" -ge 2 ] || usage
            ci="$2"
            shift 2
            ;;
        --out)
            [ "$#" -ge 2 ] || usage
            out="$2"
            shift 2
            ;;
        --force)
            force=1
            shift
            ;;
        *) usage ;;
    esac
done
[ -n "$out" ] || usage
[ -f "$MANIFEST" ] || die "manifest not found: $MANIFEST"

# Known slices are the ones the manifest names — adding a slice is adding rows.
known_slices=" "
while IFS="$(printf '\t')" read -r target slice when source mode || [ -n "${target:-}" ]; do
    case "$target" in '' | '#'*) continue ;; esac
    case "$known_slices" in *" $slice "*) ;; *) known_slices="$known_slices$slice " ;; esac
done <"$MANIFEST"
[ "$known_slices" != " " ] || die "manifest has no rows: $MANIFEST"

selected=" core "
for s in $(printf '%s' "$slices" | tr ',' ' '); do
    case "$known_slices" in *" $s "*) ;; *) die "unknown slice '$s' (known:$known_slices)" ;; esac
    case "$selected" in *" $s "*) ;; *) selected="$selected$s " ;; esac
done

platforms=" "
for p in $(printf '%s' "$ci" | tr ',' ' '); do
    case "$p" in
        github | gitlab) platforms="$platforms$p " ;;
        *) die "unknown CI platform '$p' (known: github gitlab)" ;;
    esac
done

# Pass 1: validate every row and collect the selected ones, in order.
plan="$(mktemp)"
modes="$(mktemp)"
trap 'rm -f "$plan" "$modes"' EXIT
n=0
while IFS="$(printf '\t')" read -r target slice when source mode || [ -n "${target:-}" ]; do
    n=$((n + 1))
    case "$target" in '' | '#'*) continue ;; esac
    [ -n "$slice" ] && [ -n "$when" ] && [ -n "$source" ] ||
        die "manifest line $n: expected 4 or 5 tab-separated columns"
    case "$mode" in *"$(printf '\t')"*) die "manifest line $n: expected 4 or 5 tab-separated columns" ;; esac
    case "$target" in
        /* | ../* | */../* | *'/..' | '..') die "manifest line $n: target must be a relative path inside the repo: $target" ;;
    esac
    case "$mode" in
        '' | once) [ -f "$TEMPLATES/$source" ] || die "manifest line $n: source not found: $source" ;;
        link)
            case "$source" in
                /* | .. | ../* | */../* | *'/..') die "manifest line $n: link text must be relative without '..': $source" ;;
                . | ./* | */./* | *'/.' | *//*) die "manifest line $n: link text must be a plain relative path (no '.' or empty segments): $source" ;;
            esac
            ;;
        *) die "manifest line $n: unknown mode '$mode' (known: once link)" ;;
    esac
    # A target's rows must agree on its mode, selected or not.
    prev="$(T="$target" awk -F '\t' '$1 == ENVIRON["T"] { print ($3 == "" ? "-" : $3); exit }' "$modes")"
    if [ -n "$prev" ]; then
        [ "$prev" = "${mode:--}" ] || die "manifest line $n: $target mixes modes ($prev vs ${mode:--})"
        [ "$mode" != link ] || die "manifest line $n: link target $target has more than one row"
    else
        printf '%s\t\t%s\n' "$target" "$mode" >>"$modes"
    fi
    case "$when" in
        always) keep=1 ;;
        ci=github | ci=gitlab)
            case "$platforms" in *" ${when#ci=} "*) keep=1 ;; *) keep=0 ;; esac
            ;;
        *) die "manifest line $n: unknown when '$when' (known: always ci=github ci=gitlab)" ;;
    esac
    case "$selected" in *" $slice "*) ;; *) keep=0 ;; esac
    [ "$keep" -eq 1 ] && printf '%s\t%s\t%s\n' "$target" "$source" "$mode" >>"$plan"
done <"$MANIFEST"

# mode_of <target> — "" (tracked), once, or link.
mode_of() { T="$1" awk -F '\t' '$1 == ENVIRON["T"] { print $3; exit }' "$plan"; }
# link_text <target> — a link row's source column.
link_text() { T="$1" awk -F '\t' '$1 == ENVIRON["T"] { print $2; exit }' "$plan"; }

targets="$(cut -f1 "$plan" | awk '!seen[$0]++')"
for t in $targets; do
    [ "$(mode_of "$t")" = link ] || continue
    text="$(link_text "$t")"
    case "$t" in */*) dest="$(dirname "$t")/$text" ;; *) dest="$text" ;; esac
    printf '%s\n' "$targets" | grep -qxF "$dest" && [ "$(mode_of "$dest")" != link ] ||
        die "link $t -> $text: target is not a file rendered by this selection"
done
if [ "$force" -eq 0 ]; then
    for t in $targets; do
        [ -z "$(mode_of "$t")" ] || continue
        if [ -e "$out/$t" ] || [ -L "$out/$t" ]; then die "refusing to overwrite $out/$t (pass --force)"; fi
    done
fi
# A tracked target is truncated and appended to, and both redirections (and
# mkdir -p) follow symlinks. So a symlink at the target, or at any directory
# between --out and it, would send the write wherever the link points, which
# can be outside --out. Refuse it, even with --force. --out itself may be a link.
for t in $targets; do
    [ -z "$(mode_of "$t")" ] || continue
    p="" rest="$t"
    while [ -n "$rest" ]; do
        seg="${rest%%/*}"
        case "$rest" in */*) rest="${rest#*/}" ;; *) rest="" ;; esac
        p="${p:+$p/}$seg"
        if [ -L "$out/$p" ]; then die "refusing to write through symlink $out/$p"; fi
    done
done

# Pass 2: write. A create-only target (once, link) that already exists — even
# as a dangling symlink — is the user's now and is left alone. Each written
# file target is truncated once, then its rows are appended; links go last.
written=""
for t in $targets; do
    m="$(mode_of "$t")"
    if [ -n "$m" ] && { [ -e "$out/$t" ] || [ -L "$out/$t" ]; }; then continue; fi
    written="$written$t
"
    [ "$m" = link ] && continue
    mkdir -p "$out/$(dirname "$t")"
    : >"$out/$t"
done
while IFS="$(printf '\t')" read -r t source m; do
    [ "$m" != link ] || continue
    printf '%s' "$written" | grep -qxF "$t" || continue
    cat "$TEMPLATES/$source" >>"$out/$t"
done <"$plan"
for t in $targets; do
    printf '%s' "$written" | grep -qxF "$t" || continue
    if [ "$(mode_of "$t")" = link ]; then
        mkdir -p "$out/$(dirname "$t")"
        ln -s "$(link_text "$t")" "$out/$t"
    else
        case "$t" in *.sh) chmod 0755 "$out/$t" ;; esac
    fi
    printf '%s\n' "$t"
done
