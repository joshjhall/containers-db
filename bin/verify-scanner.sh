#!/usr/bin/env bash
# Verify the scanner binary against the sha256 `build` published, before
# `deliver` runs it with SCANNER_TOKEN (#427). Exits non-zero, with a
# workflow `::error::`, on an invalid hash, a mismatch, or a missing binary.
#
# Usage: bin/verify-scanner.sh <sha256> <binary>
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <sha256> <binary>" >&2
  exit 2
fi

sha256=$1
binary=$2

# An empty or malformed hash means the job output is miswired, not a match.
if ! [[ "$sha256" =~ ^[0-9a-f]{64}$ ]]; then
  # The value itself is not echoed: it is not a hash, so it could carry a
  # newline and start a workflow command.
  echo "::error::build did not publish a valid scanner sha256." >&2
  exit 1
fi

# Two spaces between hash and path is the sha256sum line format; --strict
# rejects any line that does not parse.
if ! printf '%s  %s\n' "$sha256" "$binary" \
    | sha256sum --check --strict --quiet -; then
  echo "::error::Scanner binary does not match the sha256 build published; refusing to run it." >&2
  exit 1
fi
