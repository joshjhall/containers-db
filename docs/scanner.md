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

# Real run (will branch, commit, push, and `gh pr create`).
GITHUB_TOKEN=$(gh auth token) \
  cargo run --release -p scanner -- tool rust

# Scan everything that's currently due.
cargo run --release -p scanner -- due
```

`GITHUB_TOKEN` is optional but recommended — unauthenticated GitHub API
calls share a low rate limit.

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
        Ok(AdapterResult { upstream_versions, signals, source_archived })
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

The scanner generates new `versions/<v>.json` files by deep-cloning the
most recent existing version of the same tool and swapping the version
literals. If your tool needs anything else (Tier 2 pinned checksums, a
new `support_matrix` row, etc.), submit it as a manual PR; the scanner
will then carry that change forward to subsequent versions.

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
  cadence check) and `dry_run` (print plan, don't write or open PRs).
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

Only the scanner step sees the secret, as `GH_TOKEN` (`gh` prefers it
over `GITHUB_TOKEN`). Before the scanner runs, that step calls
`gh auth setup-git`, which makes `gh` git's credential helper, so
`git push` reads the same token from the environment at push time.
Checkout uses the default token with `persist-credentials: false`, so
no credential is in `.git/config` while the npm install and `cargo build`
steps run third-party code (#416). Inside the scanner step, the scanner
strips `GH_TOKEN` and `GITHUB_TOKEN` from the env of its `npx ajv`
children, so ajv-cli and its npm dependencies never see either token.
The step runs the prebuilt `target/release/scanner` rather than
`cargo run`, so no build script runs while the token is set. The
remaining holders are the scanner binary itself, `gh`, and `git`.
`GITHUB_TOKEN` is limited to `contents: read`. It only fetches the repo
and makes the adapters' read-only API calls.

A first step fails the run with an `::error::` naming the secret if it
is unset. Without it, the run would only fail at the first push or `gh`
call, after the build.

**Dry runs don't need the secret.** A `workflow_dispatch` with
`dry_run: true` skips the guard and `gh auth setup-git`, because a dry run
writes no files, pushes nothing, and opens no PRs or issues. Scheduled
runs have no inputs, so they always go through the guard.

Each run also writes one line per tool that opened, updated, or was
already covered by a PR to the job's step summary, so a stalled scan PR
is visible from the run page.
