#!/usr/bin/env bash
# Collapse repeat tested[] rows left by the evidence re-test loop (#450,
# containers#1319). For each (os, os_version, arch) whose rows all agree on
# result and error_class, keep the newest by tested_at. A tuple whose rows
# disagree is left as-is and printed as MIXED, so a regression is never hidden.
#
# A uniform tuple collapses only when the newest row subsumes every older one:
# ignoring the volatile fields (image_digest, ci_run, tested_at,
# duration_seconds), each field an older row sets to a non-null value must be
# equal on the newest row. An older row's absent or null field is subsumed, so
# a pre-dependencies row gives way to one that records them. Anything else
# (a different version_output, or a value the newest row lacks) would drop real
# evidence, so the tuple is left as-is and printed as DIVERGENT with the fields
# that differ.
#
# This is a one-off cleanup, not a gate: ingest's append-on-new-digest policy
# is intended, and a rebuilt image is new evidence.
#
# Usage: bin/dedupe-tested.sh [--check|--write] [file...]
#   --check  (default) report per-file counts; exit 1 if any file would change
#   --write  rewrite the files in place
#   file...  version files; default tools/*/versions/*.json
set -euo pipefail

mode="check"
case "${1:-}" in
  --check) shift ;;
  --write) mode="write"; shift ;;
  -*) echo "usage: $0 [--check|--write] [file...]" >&2; exit 2 ;;
esac
# The mode flag goes first; a later one would otherwise be read as a path.
for a in "$@"; do
  case "$a" in
    -*) echo "usage: $0 [--check|--write] [file...]" >&2; exit 2 ;;
  esac
done

if [ "$#" -eq 0 ]; then
  repo_root=$(cd "$(dirname "$0")/.." && pwd)
  shopt -s nullglob
  set -- "$repo_root"/tools/*/versions/*.json
  shopt -u nullglob
  if [ "$#" -eq 0 ]; then
    echo "error: no version files under $repo_root/tools" >&2
    exit 2
  fi
fi

# Shared by the collapse and the report.
# shellcheck disable=SC2016  # jq program, not shell expansion
defs='
  def tuple: [.os, .os_version, .arch];
  def outcome: [.result, (.error_class // null)];
  # RFC3339Nano drops trailing zeros, so `...:53Z` and `...:53.5Z` do not
  # order as strings. Sort on [seconds, fraction right-padded to 9 digits].
  # The zone must be Z: an offset cannot be ordered this way, so fail loudly.
  def when: (if type == "string" then
      capture("^(?<s>[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2})(?:\\.(?<f>[0-9]{1,9}))?Z$")
      else null end)
    // error("tested_at not a UTC RFC3339 timestamp: \(.)")
    | [.s, ((.f // "") + "000000000")[0:9]];
  # Outcome is compared separately; the rest of these differ on every re-test.
  def stable: del(.result, .error_class,
    .image_digest, .ci_run, .tested_at, .duration_seconds);
  # The fields this row sets that $new does not carry identically.
  def lost($new): ($new | stable) as $n
    | [stable | to_entries[] | select(.value != null and .value != $n[.key]) | .key];
  # Of a uniform group of {key, value} entries: the newest entry, and every
  # field some older row would lose to it.
  def newest: max_by(.value.tested_at | when);
  def divergent: newest as $new | map(.value | lost($new.value)) | add | unique;
'
# Rows are grouped by tuple; the kept rows stay in their original order so the
# diff is deletions only.
# shellcheck disable=SC2016
dedupe="$defs"'
  (.tested // []) as $t
  | [$t | to_entries | group_by(.value | tuple)[]
      | if (map(.value | outcome) | unique | length) == 1
        then (if divergent == [] then newest.key else .[].key end)
        else .[].key end] as $keep
  | if has("tested")
    then .tested = [$t | to_entries[] | select(.key | IN($keep[])) | .value]
    else . end
'
# One line per tuple left as-is: "MIXED os/ver/arch" or
# "DIVERGENT os/ver/arch field,...".
# shellcheck disable=SC2016
report="$defs"'
  (.tested // []) | to_entries | group_by(.value | tuple)[]
  | (.[0].value | "\(.os)/\(.os_version)/\(.arch)") as $t
  | if (map(.value | outcome) | unique | length) > 1 then "MIXED \($t)"
    else divergent as $d
      | if $d == [] then empty else "DIVERGENT \($t) \($d | join(","))" end
    end
'
# The set of (tuple, result) facts must survive the collapse unchanged.
triples='[(.tested // [])[] | [.os, .os_version, .arch, .result]] | unique'

tmp=""
trap 'rm -f "$tmp"' EXIT

# Pass 1 computes and guards every file before pass 2 writes any, so a guard
# refusal never leaves the catalog half rewritten.
pending=()
for f in "$@"; do
  rows=$(jq '(.tested // []) | length' "$f")
  out=$(jq --indent 2 "$dedupe" "$f")
  kept=$(jq '(.tested // []) | length' <<<"$out")

  while read -r kind t; do
    [ -n "$kind" ] && echo "$kind $f $t"
  done < <(jq -r "$report" "$f")

  [ "$kept" -eq "$rows" ] && continue
  echo "$f: $rows -> $kept"

  if [ "$(jq -c "$triples" "$f")" != "$(jq -c "$triples" <<<"$out")" ]; then
    echo "error: $f: (tuple, result) set changed; refusing to write" >&2
    exit 1
  fi
  pending+=("$f")
done

[ "${#pending[@]}" -eq 0 ] && exit 0
[ "$mode" = check ] && exit 1

for f in "${pending[@]}"; do
  # Seed the temp file with `cp -p` so it already carries the original's
  # mode (portable, unlike `chmod --reference`), then rename it into place
  # so an interrupted write never truncates the version file.
  tmp=$(mktemp "$f.XXXXXX")
  cp -p "$f" "$tmp"
  jq --indent 2 "$dedupe" "$f" >"$tmp"
  # Keep the trailing blank line the version files carry.
  if [ "$(tail -c 2 "$f" | wc -l)" -eq 2 ]; then printf '\n' >>"$tmp"; fi
  mv "$tmp" "$f"
  tmp=""
done
