#!/usr/bin/env bash
# Collapse repeat tested[] rows left by the evidence re-test loop (#450,
# containers#1319). Repeats differ only in image_digest/ci_run/tested_at, so
# for each (os, os_version, arch) whose rows all agree on result and
# error_class, keep the newest by tested_at. A tuple whose rows disagree is
# left as-is and printed as MIXED, so a regression is never hidden.
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

# Rows are grouped by tuple; the kept rows stay in their original order so the
# diff is deletions only.
# shellcheck disable=SC2016  # jq program, not shell expansion
dedupe='
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
  (.tested // []) as $t
  | [$t | to_entries | group_by(.value | tuple)[]
      | if (map(.value | outcome) | unique | length) == 1
        then max_by(.value.tested_at | when).key
        else .[].key end] as $keep
  | if has("tested")
    then .tested = [$t | to_entries[] | select(.key | IN($keep[])) | .value]
    else . end
'
# shellcheck disable=SC2016
mixed='
  [(.tested // []) | group_by([.os, .os_version, .arch])[]
    | select((map([.result, (.error_class // null)]) | unique | length) > 1)
    | "\(.[0].os)/\(.[0].os_version)/\(.[0].arch)"] | .[]
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

  while IFS= read -r t; do
    [ -n "$t" ] && echo "MIXED $f $t"
  done < <(jq -r "$mixed" "$f")

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
