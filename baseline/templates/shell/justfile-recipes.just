
shfmt_flags := "-i 4 -ci"

# Lint shell scripts. No arguments: every tracked *.sh.
lint-sh *files:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "$#" -eq 0 ]; then
        list="$(git ls-files '*.sh')"
        while IFS= read -r f; do if [ -n "$f" ]; then set -- "$@" "$f"; fi; done <<<"$list"
    fi
    [ "$#" -eq 0 ] || shellcheck --severity=warning -- "$@"

# Format shell scripts in place. No arguments: every tracked *.sh.
fmt-sh *files:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "$#" -eq 0 ]; then
        list="$(git ls-files '*.sh')"
        while IFS= read -r f; do if [ -n "$f" ]; then set -- "$@" "$f"; fi; done <<<"$list"
    fi
    [ "$#" -eq 0 ] || shfmt -w {{ shfmt_flags }} -- "$@"

# Check shell formatting without writing. No arguments: every tracked *.sh.
fmt-sh-check *files:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "$#" -eq 0 ]; then
        list="$(git ls-files '*.sh')"
        while IFS= read -r f; do if [ -n "$f" ]; then set -- "$@" "$f"; fi; done <<<"$list"
    fi
    [ "$#" -eq 0 ] || shfmt -d {{ shfmt_flags }} -- "$@"
