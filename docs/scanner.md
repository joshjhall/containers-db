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
