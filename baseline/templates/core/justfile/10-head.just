# The hooks, CI, and contributors all call these recipes. A tool's flags appear
# only in this file (core § 1): lefthook decides when and on which files, the
# recipe decides how. File-taking recipes run on the whole tree when called
# with no arguments.

set positional-arguments

default:
    @just --list

# Install the git hooks
install-hooks:
    lefthook install

# Run every linter over the whole tree, no skips
lint:
    just lint-typos
    just fmt-config-check
    just fmt-toml-check
    just lint-md
