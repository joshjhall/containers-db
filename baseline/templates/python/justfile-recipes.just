
# Format Python in place. No arguments: every tracked *.py.
fmt-py *files:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "$#" -eq 0 ]; then
        list="$(git ls-files '*.py')"
        while IFS= read -r f; do if [ -n "$f" ]; then set -- "$@" "$f"; fi; done <<<"$list"
    fi
    [ "$#" -eq 0 ] || ruff format -- "$@"

# Lint Python. No arguments: every tracked *.py.
lint-py *files:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "$#" -eq 0 ]; then
        list="$(git ls-files '*.py')"
        while IFS= read -r f; do if [ -n "$f" ]; then set -- "$@" "$f"; fi; done <<<"$list"
    fi
    [ "$#" -eq 0 ] || ruff check -- "$@"

# Check Python formatting without writing. No arguments: every tracked *.py.
fmt-py-check *files:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "$#" -eq 0 ]; then
        list="$(git ls-files '*.py')"
        while IFS= read -r f; do if [ -n "$f" ]; then set -- "$@" "$f"; fi; done <<<"$list"
    fi
    [ "$#" -eq 0 ] || ruff format --check -- "$@"
