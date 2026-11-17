#!/usr/bin/env bash
# Install every tool the baseline pins, using the baseline's own installer.
#
# Renders a shell+python repo into a scratch dir and runs its
# bin/install-tools.sh, so CI installs exactly what the templates ship — the
# same tools.versions / tools.sha256 pins and the same verify-before-install
# path — instead of a second hand-kept tool list (#558).
#
# Usage: tests/baseline/install-tools.sh
#   TOOLS_BIN  install directory (default: $HOME/.local/bin); under GitHub
#              Actions the installer also appends it to $GITHUB_PATH.

set -euo pipefail

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

"$repo_root/baseline/templates/render.sh" \
    --slices shell,python --ci github --out "$scratch/repo" >/dev/null
cd "$scratch/repo"
bash bin/install-tools.sh
