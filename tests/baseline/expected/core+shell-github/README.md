# Project name
<!-- per-repo: the repo's name as the title. -->

<!-- per-repo: what this repo is and why it exists, in a paragraph. A reader
with no context should get from clone to a working run from this file alone. -->

## Layout

- `bin/`: scripts and entry points
- `tests/`: tests; `tests/run.sh` is the single entry point
- `docs/`: longer-form documentation
- Shell: scripts are `bin/*.sh`, suites are `tests/*.bats`

## Usage

<!-- per-repo: install, build, and the main commands. -->

## Lint and tests

Install the pinned tools, then the git hooks:

```bash
bin/install-tools.sh
just install-hooks
```

Run every linter, then the test suite (the same commands CI runs):

```bash
just lint
just test
```
