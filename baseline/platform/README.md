# Platform settings spec

Settings a new repo needs on its host that no file in the repo can carry: branch
protection, merge settings, and labels. The rendered templates
(`../templates/`) cover the platform **files** (Dependabot, `CODEOWNERS`, issue
and PR/MR templates). This directory covers what only the host's API can set.

`stibbons setup platform` reads these files from a containers-db snapshot and
applies them. They are versioned with the templates, so a snapshot's settings
always match its CI template.

| File         | Host   | What it sets                                                              |
| ------------ | ------ | ------------------------------------------------------------------------- |
| `github.yml` | GitHub | Squash-only merges, delete branch on merge, a default-branch ruleset      |
| `gitlab.yml` | GitLab | Merge settings, pipeline-must-succeed, the protected default branch       |
| `labels.yml` | Both   | The `type/*`, `severity/*`, `effort/*` and `status/*` label vocabulary     |

Every file carries `version: 1`. A consumer refuses a version it does not know.

## The required check

The GitHub ruleset requires exactly one check, `merge-gate`: the gate job in
`templates/core/ci/github-ci.yml.tmpl` (core § 3). The GitLab template has no
gate job, because a GitLab pipeline already fails on any failed job, so
`gitlab.yml` requires `only_allow_merge_if_pipeline_succeeds` instead.
`tests/baseline/validate-platform.sh` reads the job name from the CI template,
so renaming the job without updating the spec fails the gate.
`tests/baseline/validate-platform-mutations.test.sh` breaks scratch copies one
way at a time and requires the gate to fail on each.

## Check source and bypass

A required check named only by its context is satisfied by a check or status of
that name from **any** app or token with checks/statuses write access. So
`github.yml` binds `merge-gate` to an app with `integration_id`. `15368` is
GitHub Actions, which runs the CI template. The field is optional. Drop it, or
set the right app id, if a repo's CI posts the check from another app. When it
is present, the gate requires an integer.

`bypass_actors` is `[]`: nobody, admins included, bypasses the ruleset. It is
stated in the spec rather than left to the consumer's default, so a per-repo
bypass is a reviewed edit to this list, not a side effect of how the ruleset was
created.

## Placeholders and per-repo values

- `~DEFAULT_BRANCH` (GitHub) and `DEFAULT_BRANCH` (GitLab) stand for the repo's
  default branch. The GitHub form is the ruleset API's own token.
- A value with a `# per-repo:` comment is a starting point: review counts are
  `0` because a new repo often has one maintainer.
- `required_signatures` / `reject_unsigned_commits` are `optional` or
  `required`. The default is `optional`, because required signing blocks every
  contributor who has not set it up.
