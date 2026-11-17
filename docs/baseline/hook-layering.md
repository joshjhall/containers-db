# Baseline hook layering: lefthook → just → tool

- **Status:** Accepted (2026-11-17)
- **Issue:** [#559](https://github.com/joshjhall/containers-db/issues/559),
  part of the repo-baseline epic
  [#557](https://github.com/joshjhall/containers-db/issues/557)
- **Follow-up:** template rewrite and enforcement gate in [#620](https://github.com/joshjhall/containers-db/issues/620),
  blocked by [#558](https://github.com/joshjhall/containers-db/issues/558)

The baseline spec now lives in this repo at
[`baseline/core.md`](../../baseline/core.md) (#558), whose § 1 *Hook layout*
links here. This record stays authoritative until #620 applies it to the
templates and folds it into `core.md` § 1, at which point this file becomes a
pointer.

## Problem

In the baseline templates, `lefthook.yml` calls each tool directly
(`shellcheck --severity=warning {staged_files}`, `shfmt -w -i 4 -ci …`), and the
`justfile` `lint` recipe lists the same tools again. CI calls `just`. That
leaves two hand-kept lists of tools and flags. A hook can run one set of flags
while `just lint` and CI run another, so "green locally" stops meaning "green
in CI".

## Decision

**Every hook command is a `just` recipe, and `just` is the only place a tool's
flags appear.**

```text
lefthook (when + which files)  →  just recipe (how)  →  tool
CI                             →  just recipe (how)  →  tool
```

The hook decides *when* a check runs and *which files* it sees. The recipe
decides *how* the tool is invoked. CI and contributors call the same recipe, so
there is one place to change a flag and one place it can diverge.

## Answers to the open questions

### 1. Staged-file scope

A recipe takes an optional file list. An empty list means the whole tree, which
the recipe selects with its own `git ls-files` pattern. Files are passed as
positional arguments and expanded as `"$@"`, never word-split, so paths with
spaces survive.

```just
set positional-arguments

# Format shell scripts in place. No arguments: every tracked *.sh file.
fmt-sh *files:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "$#" -eq 0 ]; then
        mapfile -t f < <(git ls-files '*.sh')
        set -- "${f[@]}"
    fi
    [ "$#" -eq 0 ] || shfmt -w -i 4 -ci "$@"
```

The hook passes `{staged_files}` (`just fmt-sh {staged_files}`). `just lint`
and CI call the same recipe with no arguments.

### 2. Skip guards

**They stay in lefthook's `skip:`.** The baseline rule is "hooks may skip, CI
may not". Keeping the guard in the hook and keeping recipes guard-free makes
that rule structural: CI calls recipes directly, so it has no skip path to take.
A recipe never tests whether its tool is installed. If the tool is missing, the
recipe fails.

`just` itself is a **required** tool and is never guarded. A hook whose `just`
is missing fails loudly, the same way the spec treats in-repo gates. Guarding
`just` would silently disable every hook at once.

### 3. `stage_fixed` formatters

**Works unchanged.** lefthook re-stages the matched files after the command
exits, whatever process ran in between. This was checked in a scratch repo:
`just fmt-sh {staged_files}` with `stage_fixed: true` reformatted a staged
script whose name contains a space. The commit contained the formatted content
and left a clean working tree.

Fixer recipes write in place (`fmt-*`). Checker recipes only report
(`*-check`, `lint-*`) and must not touch the index, so they never get
`stage_fixed`.

### 4. Glob filtering

`glob:` and `exclude:` stay in the hook, and the recipe receives files that are
already filtered. One pair can still drift: the hook's `glob:` and the
recipe's whole-tree `git ls-files` pattern. The enforcement gate (below) must
check that they agree.

### 5. Cost

Measured with `hyperfine -N`, 20 runs, on one shell file:

| Invocation               | Mean    |
| ------------------------ | ------- |
| `shfmt -d -i 4 -ci b.sh` | 0.98 ms |
| `just fmt-sh b.sh`       | 17.6 ms |

The extra `just` process adds about 16.6 ms per hook command. Pre-commit runs
with `parallel: true`, so a typical five-command stage gains well under 100 ms
of wall time. **Accepted.**

## Before and after (shell slice)

Before, with flags in both files:

```yaml
pre-commit:
  commands:
    shfmt:
      glob: "*.sh"
      skip:
        - run: "! command -v shfmt >/dev/null 2>&1"
      run: shfmt -w -i 4 -ci {staged_files}
      stage_fixed: true
```

After, with flags only in the justfile:

```yaml
pre-commit:
  commands:
    shfmt:
      glob: "*.sh"
      skip:
        - run: "! command -v shfmt >/dev/null 2>&1"
      run: just fmt-sh {staged_files}
      stage_fixed: true
```

## Enforcement gate (specified here, built in the follow-up)

- **Fail** when any lefthook `run:` value does not start with `just `.
  `skip:` entries may name the tool, because naming it there is how the guard
  works.
- **Fail** when a hook's `glob:` disagrees with the whole-tree pattern of the
  recipe it calls (question 4).
- The gate is an in-repo script, so it is never skip-guarded.
