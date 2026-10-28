# Catalog scanner

The scanner walks every tool's `index.json`, decides which are due based
on `activity.scan_cadence_days`, queries upstream sources for new
versions, recomputes the activity block, and opens one PR per tool with
new `versions/<v>.json` files. Failures open a tracking issue instead
of a malformed PR.

The scanner is a Rust binary in the `scanner/` workspace member. It
runs hourly via `.github/workflows/scan.yml` and can be invoked
manually with `workflow_dispatch`.

## Running locally

```bash
# Dry-run a single tool — no files written, no PR opened.
cargo run --release -p scanner -- tool rust --dry-run
cargo run --release -p scanner -- tool go --dry-run

# Real run (will branch, commit, push, and `gh pr create`).
GITHUB_TOKEN=$(gh auth token) \
  cargo run --release -p scanner -- tool rust

# Scan everything that's currently due.
cargo run --release -p scanner -- due

# The CI split: validate and stage (needs ajv-cli), then deliver.
cargo run --release -p scanner -- due --stage /tmp/plan.json
cargo run --release -p scanner -- deliver --plan /tmp/plan.json
```

`GITHUB_TOKEN` is optional but recommended — unauthenticated GitHub API
calls share a low rate limit.

## Runtime and build requirements

**TLS trust comes from the OS store.** The HTTP client is reqwest 0.13
with its `rustls` feature, which verifies certificates through
`rustls-platform-verifier`. On Linux that loads the system roots
(`ca-certificates`) when the client is built, once per run, before any
request. Point `SSL_CERT_FILE` or `SSL_CERT_DIR` at a PEM bundle to use
other roots instead. With no roots at all, `due`, `tool`, and `backfill`
exit with `building the HTTP client failed (TLS roots are loaded from the
OS trust store …)`. `deliver` makes no requests and does not need them.
reqwest 0.13 has no bundled-roots option (0.12's `webpki-roots`), so the
host's store is the only source.

**The crypto provider is `aws-lc-rs`**, reqwest's default for `rustls`.
Building it compiles `aws-lc-sys`, which needs a C toolchain (`cc`) and,
on some targets, `cmake`. `ring` is the alternative, but it would mean
switching to `rustls-no-provider`, adding `rustls` as a direct dependency,
and installing the provider before any client is built. That is more to
keep in step than one build-time requirement, which `ubuntu-latest` CI
runners already meet.

## Activity tier rules

Tier mapping is a deliberate heuristic, encoded in
[`scanner/src/activity.rs`](../scanner/src/activity.rs). First match
wins, top to bottom:

| Tier         | Trigger                                                   | `scan_cadence_days` |
| ------------ | --------------------------------------------------------- | ------------------- |
| `abandoned`  | source repo archived / disabled / EOL                     | 180                 |
| `dormant`    | `last_release` > 5y ago AND `last_commit` > 2y ago        | 90                  |
| `stale`      | `last_release` > 18 months ago                            | 90                  |
| `slow`       | `last_release` > 9 months ago                             | 60                  |
| `very-active`| `releases_last_90d` ≥ 3 AND `last_release` ≤ 30d ago      | 1                   |
| `active`     | `releases_last_90d` ≥ 1 AND `last_release` ≤ 90d ago      | 7                   |
| `maintained` | fallback (signal-missing or quiet-but-not-stale)          | 30                  |

Tune by editing the function and the table together; the unit tests in
`scanner/src/activity.rs` exercise every row.

## Writing a new adapter

Each per-tool upstream source plugs in as an
[`Adapter`](../scanner/src/adapter.rs) implementation. The orchestrator
handles diffing, generation, PR/issue creation, and tier mapping —
adapters only have to fetch upstream truth.

### 1. Implement the trait

```rust
// scanner/src/adapters/<tool>.rs
use async_trait::async_trait;
use crate::adapter::{Adapter, AdapterResult, ToolIndex};

pub struct MyAdapter { /* client, base URL, ... */ }

#[async_trait]
impl Adapter for MyAdapter {
    async fn fetch(&self, index: &ToolIndex) -> anyhow::Result<AdapterResult> {
        // 1. Query upstream
        // 2. Build a Vec<UpstreamVersion>
        // 3. Build ActivitySignals from raw counts
        // 4. Set source_archived from whatever signal your source exposes
        // 5. Pick a default_version if the tool has a policy for one
        Ok(AdapterResult { upstream_versions, signals, source_archived, default_version: None })
    }
}
```

`AdapterResult` carries:

- `upstream_versions: Vec<UpstreamVersion>` — every release the scanner
  should consider. Filter out pre-releases / nightlies in the adapter;
  the orchestrator trusts the list verbatim.
- `signals: ActivitySignals` — raw counts that feed
  [`activity::compute_tier`](../scanner/src/activity.rs). Only fields
  the source can supply need to be set; the rest stay `None`.
- `source_archived: bool` — pivots the tool straight to `abandoned`
  regardless of other signals.
- `default_version: Option<String>` — the version the index's
  `default_version` should name, for a tool with a policy for it (node:
  newest active LTS). `None` leaves the index's value alone. It is written
  only alongside a scan that adds versions, and only if the version is
  cataloged once they are added — a held-back release never becomes the
  default. It also must be all-numeric (no `-rc1`) and never older than
  the current default. A plan could still move it *forward* to any listed
  version; delivery has no upstream feed to check that against, so it is
  accepted risk, visible as a one-line `index.json` change in the PR.

### 2. Register it

```rust
// scanner/src/adapters/mod.rs
pub mod my_tool;
pub fn register_all() -> HashMap<String, Arc<dyn Adapter>> {
    let mut registry = HashMap::new();
    registry.insert("rust".to_string(), Arc::new(rust::RustAdapter::new()));
    registry.insert("my-tool".to_string(), Arc::new(my_tool::MyAdapter::new()));
    registry
}
```

A tool with no registered adapter is silently skipped — adding a tool
to `tools/<id>/index.json` doesn't crash the scanner before its adapter
lands.

Registered adapters:

| Tool   | Adapter                                               | Upstream                                                                       |
| ------ | ----------------------------------------------------- | ------------------------------------------------------------------------------ |
| `go`   | [`adapters/go.rs`](../scanner/src/adapters/go.rs)     | go.dev download feed (versions, `stable`) + release-history page (dates)       |
| `node` | [`adapters/node.rs`](../scanner/src/adapters/node.rs) | nodejs.org `dist/index.json` (versions, dates, artifacts, LTS)                 |
| `rust` | [`adapters/rust.rs`](../scanner/src/adapters/rust.rs) | GitHub REST: repo, releases, security advisories                               |

The scanner treats the newest cataloged `released` as a strict
high-water mark, so an adapter must never guess a missing release
date, and must not emit one release while a same-day sibling is still
undated: once the dated one is cataloged, the sibling's date is no
longer newer and it is rejected on every later scan. The go adapter
holds back all new releases while any release newer than its branch's
newest cataloged patch is undated; an undated gap further back in a
branch's history never blocks.

### 3. Record HTTP fixtures with `wiremock`

Adapters must be hermetic in tests — no live network. See
[`scanner/tests/rust_adapter_test.rs`](../scanner/tests/rust_adapter_test.rs)
for the pattern:

```rust
let server = MockServer::start().await;
// Fixture dates MUST be window-relative, never absolute literals: the
// `releases_last_90d` signal is computed against `Utc::now()`, so a
// hard-coded date silently ages out of the trailing-90-day window and
// turns the test into a date bomb (issue #48). Use the `days_ago` helper
// from `rust_adapter_test.rs` and keep the newest release inside 90 days.
Mock::given(method("GET")).and(path("/repos/o/r/releases"))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!([
        { "tag_name": "1.0.0", "published_at": days_ago(12) }
    ])))
    .mount(&server).await;

let adapter = MyAdapter::with_api_base(server.uri());
let result = adapter.fetch(&fixture_index()).await.unwrap();
```

For each new adapter, snapshot at least: a "found new versions" case, a
"no new versions" case, an "upstream archived" case, and an "upstream
errored" case.

### 4. Adapter-specific version templates

The scanner generates new `versions/<v>.json` files by deep-cloning an
existing version of the same tool and swapping the version literals. The
template is the cataloged version sharing the most leading segments with
the new one, newest on ties
([`generate::template_for`](../scanner/src/generate.rs)): a node 22.x
backport clones the newest 22.x file, go 1.26.x clones the newest 1.26.x,
and a release opening a new line clones the newest file overall. Lines
that ship in parallel can therefore differ in shape (node 26's arm64
`libatomic` dependency is absent from 22.x) without a backport inheriting
the newest line's. A backfill clones the oldest file instead. If your tool
needs anything else (Tier 2 pinned checksums, a
new `support_matrix` row, etc.), submit it as a manual PR; the scanner
will then carry that change forward to subsequent versions. For a tool
whose matrix spans glibc and musl, see
[binary-tarball-glibc-musl.md](binary-tarball-glibc-musl.md), which also
covers pinning the shape with a test. When upstream ships no musl binary,
see [source-build-musl.md](source-build-musl.md).

## Duplicate-PR guard

New versions are computed against `main`, so versions in an open,
unmerged scan PR look new again on every run. Before opening a PR the
scanner lists open PRs whose head starts with `scan/<tool>/` (or
`backfill/<tool>/` for `scanner backfill`) and picks the oldest. PRs
from forks are ignored — their branch name and body are not trusted:

- **Skip** — that PR already covers every new version. The scanner logs
  `open scan PR #N already covers <versions>, skipping`, discards the
  regenerated files, and succeeds (no failure issue).
- **Update in place** — that PR misses some versions. The files are
  regenerated from `main` onto its branch, force-pushed with a lease
  pinned to the head commit seen when listing, and its title and body
  refreshed with `gh pr edit`. No second PR is opened. Manual commits on
  that branch are replaced — carry them in a separate PR.
- **Open new** — no matching open PR; a new `scan/<tool>/<date>` PR opens.

If the open-PR listing is truncated or malformed, the scan fails rather
than guessing.

`scanner due` delivers several tools from one checkout. It records the
starting branch and commit once, returns there after each PR, and checks
it before every tool: if a previous tool left the checkout anywhere else,
the run aborts instead of branching the next tool's PR from the wrong
commit.

The versions an open PR covers are read from the `## New versions`
bullet list in its body (written by `pr_body`), not from the title — a
backfill title names only a range. Hand-edit that list and the guard
sees the edit. The logic lives in
[`scanner/src/dedup.rs`](../scanner/src/dedup.rs).

## Failure handling

Any unhandled error during a scan opens a GitHub issue titled
`scanner: <tool> scan failed <YYYY-MM-DD>` with the truncated error,
labeled `scanner-failure`. Idempotent on title — if an open issue with
the same title exists, the failure path comments on it instead of
creating a duplicate.

A failed scan does not advance `activity.scanned_at`, so the next
scheduled run will retry. For transient errors (upstream rate limit,
API outage), no human action is needed.

## Workflow

`.github/workflows/scan.yml`:

- Cron: hourly. Per-tool gating in scanner code means cheap tools
  aren't actually re-checked every hour.
- `workflow_dispatch` inputs: `tool` (force a single tool, skip the
  cadence check), `dry_run` (print plan, don't write or open PRs), and
  `backfill_from` (with `tool`, run `scanner backfill`).
- Three jobs, `build`, `stage`, and `deliver`; see
  [Where the secret is](#where-the-secret-is-two-phase-scan).
- `concurrency.group = catalog-scanner` serializes runs so a slow scan
  doesn't race the next hourly trigger.

The scanner uses `gh` and `git` from the runner — same code works
locally as long as those CLIs are available.

### `SCANNER_TOKEN` secret

GitHub does not trigger workflows from pushes or PRs made with the
default `GITHUB_TOKEN`, so scan PRs opened that way sat in
`action_required` with no `validate.yml` run (#401). The workflow
therefore requires a `SCANNER_TOKEN` repo secret: a fine-grained PAT or
GitHub App token scoped to this repository only, with:

| Permission      | Access | Why                                         |
| --------------- | ------ | ------------------------------------------- |
| `contents`      | write  | push the `scan/*` branch                    |
| `pull-requests` | write  | open and update scan PRs                    |
| `issues`        | write  | file/comment on `scanner-failure` issues    |

#### Where the secret is (two-phase scan)

The workflow runs in three jobs so that no third-party code runs where
the secret is (#416):

1. **`build` (no secret).** Compiles the scanner with
   `cargo build --release --locked`, uploads the binary, and publishes its
   sha256 as the job output `scanner_sha256`. No npm or
   node runs here, so the Rust cache this job saves was only ever written
   by cargo.
2. **`stage` (no secret).** Downloads the binary, installs ajv-cli
   from the committed lockfile (`npm ci --ignore-scripts`), and runs
   `scanner due|tool|backfill --stage <plan.json>`. It has no Rust
   toolchain or cache. For each tool the
   scanner writes the new files, runs the in-process rules **and** ajv
   (required in this mode), records the files, versions, and activity
   block in the plan, and restores the tree. A tool that fails is
   recorded in the plan's `failures` instead of filing an issue — this
   job has no token to file one with. The plan is uploaded as an artifact.
3. **`deliver` (`SCANNER_TOKEN`).** Checks out the same commit, downloads
   the `scanner` and `plan` artifacts by name into separate directories,
   checks the binary against `build`'s `scanner_sha256` (the job fails before
   `chmod` on a mismatch or a missing hash), and runs
   `scanner deliver --plan <plan.json>`.
   No cargo, npm, node, or ajv runs here. The token is `GH_TOKEN` (`gh`
   prefers it over `GITHUB_TOKEN`); `git push` gets it through
   `credential.helper=!gh auth git-credential`, set by `GIT_CONFIG_*`
   environment variables so nothing is written to `.git/config`.

JavaScript in `stage` never shares a job with the build, so it can
neither replace the binary `deliver` runs nor poison the cargo cache a
later run builds from. `stage` does hold the run's artifact-service token,
though, so `deliver` doesn't rely on artifact names alone to identify the
binary. It trusts the binary through the hash: job outputs are written only
by the job that declares them, so `stage` can't change the sha256 `build`
recorded, and any binary other than the one `build` produced is refused.
The locked dependencies' build scripts and
proc-macros still shape the binary — the same trust any Rust release
build carries. `stage` can still tamper with the plan, so `deliver`
treats the plan as untrusted:

- It refuses to run unless the checkout is at the plan's `base_sha`.
- Each plan may write only one `tools/<id>/index.json` plus one
  `tools/<id>/versions/<v>.json` per announced version, and every file
  must parse as JSON. Every announced version must be new: a plan that
  would overwrite an already-cataloged `versions/<v>.json` is refused. Versions and the backfill floor must be plain
  tokens (letters, digits, `.`, `_`, `-`, `+`).
- **`deliver` writes none of the plan's bytes.** It re-renders every file
  from its own checkout — the tool's template, the plan's releases
  (version, date, channel), activity, and timestamp, through the same
  generator code `stage` used — and refuses the plan unless the staged
  files match exactly. A plan can choose *which* releases to add; install
  URLs, checksums, and every other field come from the checkout. Since the
  bytes are identical, `stage`'s ajv verdict covers what `deliver` writes.
- A plan carries no free text. The branch (`scan/<id>/<date>` or
  `backfill/<id>/<from>`), PR title, and PR body are rendered by
  `deliver` from the plan's typed fields (versions, score, cadence,
  numeric signals), with the upstream link read from the checkout's own
  `index.json`. Unknown fields are rejected.
- A staged failure must name a tool that exists in the checkout, or it
  is refused: no issue is filed, but the run still fails and the other
  plans are still delivered. Its
  error text is capped and fenced so it can't inject markdown into the
  tracking issue (and debug-escaped in the job log, so it can't start a
  `::` workflow command).
- The PR body's upstream link is read from the checkout's `index.json`
  before the plan's files are written over it.
- The in-process rules run again on the written files before any push.
  ajv does not — it is third-party code, and it already ran in `stage`.
  The `stage`-side ajv run is a correctness check, not a security gate:
  a compromised `stage` job can skip it.
- The PR still goes through `validate.yml`, which runs ajv again. That is
  the schema gate that enforces, before anything merges.

The remaining processes that hold the token are the scanner binary,
`gh`, and `git`. Checkout uses the default token with
`persist-credentials: false` in both jobs, and `GITHUB_TOKEN` is limited
to `contents: read`: it only fetches the repo and makes the adapters'
read-only API calls.

The scanner also strips `GH_TOKEN` and `GITHUB_TOKEN` from its `npx ajv`
children. That is defense in depth for local runs, not a boundary: a
child process running as the same user can read its parent's
environment from `/proc`. The job split is the boundary.

`deliver` starts with a step that fails the run with an `::error::`
naming the secret if it is unset. Without it, the run would only fail at
the first push or `gh` call. Failures recorded while staging are filed
as tracking issues by `deliver`, and they fail that job, so a broken
adapter still turns the run red.

**Dry runs don't need the secret.** A `workflow_dispatch` with
`dry_run: true` runs `stage` with `--dry-run`, which prints the plan and
writes nothing, and skips `deliver` entirely. Scheduled runs have no
inputs, so they always run `deliver` and its guard.

Locally, without `--stage`, `scanner due|tool|backfill` still does
everything in one process (ajv only if installed), as in
[Running locally](#running-locally).

Each run also writes one line per tool that opened, updated, or was
already covered by a PR to the job's step summary, so a stalled scan PR
is visible from the run page.
