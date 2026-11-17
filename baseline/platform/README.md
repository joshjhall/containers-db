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

## Placeholders and per-repo values

- `~DEFAULT_BRANCH` (GitHub) and `DEFAULT_BRANCH` (GitLab) stand for the repo's
  default branch. The GitHub form is the ruleset API's own token.
- A value with a `# per-repo:` comment is a starting point: review counts are
  `0` because a new repo often has one maintainer.
- `required_signatures` / `reject_unsigned_commits` are `optional` or
  `required`. The default is `optional`, because required signing blocks every
  contributor who has not set it up.
