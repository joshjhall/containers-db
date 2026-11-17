# Repo baseline

The repo-baseline standard: the hook layout, always-on tools, CI shape,
version-pinning policy, and docs/test minimums every repo gets, plus the
templates that render it. containers-db is the single source of truth (#558);
consumers pin a containers-db commit SHA until catalog snapshots land.

| Path                             | What it is                                                    |
| -------------------------------- | ------------------------------------------------------------- |
| `core.md`                        | The language-neutral spec                                     |
| `slice-contract.md`              | The headings every language slice must carry                  |
| `languages/<lang>.md`            | One slice per language (`shell`, `python`)                    |
| `templates/`                     | `manifest.tsv`, `render.sh`, and the per-slice template files |
| `platform/`                      | API-only host settings (rulesets, merge settings, labels)     |
| `SOURCE`                         | Provenance of the imported tree                               |
| `Cargo.toml`, `build.rs`, `src/` | The `repo-baseline` crate that embeds `templates/`            |

Render a repo's files with:

```bash
baseline/templates/render.sh --slices shell,python --ci github --out DIR
```

## Consuming the templates from Rust

`baseline/` is also the `repo-baseline` crate. Its `build.rs` embeds every file
under `templates/` into `repo_baseline::FILES` (`(path, bytes)`, sorted), so a
consumer pins a snapshot by commit and renders offline with no checkout:

```toml
[dependencies.repo-baseline]
git = "https://github.com/joshjhall/containers-db"
rev = "<full commit sha>"
```

`repo_baseline::get(path)` looks up one file, and `repo_baseline::TEMPLATES_DIR`
is the build-host path of `templates/` (for tests that run `render.sh` against
the same snapshot). joshjhall/containers' `stibbons setup standards` is the
first consumer.

## Provenance

Imported from librarian 0.18.0 (`plugins/dev-core/skills/repo-baseline/`) plus
the `pygments>=2.20.0` pin that joshjhall/containers' stibbons carried as a
local patch, so the templates are byte-identical to the copy stibbons shipped
at the move.

Tool pins (`templates/*/tools.versions`, `templates/*/tools.sha256`) are still
kept by hand; deriving them from catalog data is a follow-up.

## Gates

| Script                                 | Checks                                                      |
| -------------------------------------- | ----------------------------------------------------------- |
| `tests/baseline/validate-slices.sh`    | Slices conform to the contract; config fences match templates |
| `tests/baseline/validate-templates.sh` | Manifest, fence parity, render behavior, rendered-repo hooks |
| `tests/baseline/render-parity.test.sh` | Every slice × CI render equals its golden (`--update` regenerates) |
| `tests/baseline/validate-platform.sh`  | `platform/` parses; GitHub requires the CI template's `merge-gate`; labels |
| `tests/baseline/validate-platform-mutations.test.sh` | Each single break of the spec or CI template fails `validate-platform.sh` |

`tests/baseline/install-tools.sh` installs every pinned tool through the
templates' own installer. CI runs the gates with `BASELINE_REQUIRE_TOOLS=1`,
which turns a missing-tool skip into a failure.
