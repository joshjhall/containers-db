
# Lint GitHub workflows. No arguments: every tracked workflow.
lint-actions *files:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "$#" -eq 0 ]; then
        list="$(git ls-files '.github/workflows/*.yml' '.github/workflows/*.yaml')"
        while IFS= read -r f; do if [ -n "$f" ]; then set -- "$@" "$f"; fi; done <<<"$list"
    fi
    [ "$#" -eq 0 ] || actionlint -- "$@"
