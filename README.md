# containers-db

Tool catalog consumed by [`luggage`](https://github.com/joshjhall/containers)
(and, in time, by stibbons and igor) to drive container feature installation.
This repo is **not user-facing**: humans interact with stibbons; stibbons
asks luggage; luggage reads this catalog.

Splitting the catalog out of the main repo means:

- Daily auto-scanner commits (one tool/version per file) don't pollute
  main-repo history.
- Consumers pin a snapshot SHA, so a bad scanner run can't break yesterday's
  builds.
- PRs stay small and reviewable — typically one tool or one version per file.

## Layout

```text
.
├── schema/
│   ├── tool.schema.json       # JSON Schema for tools/<id>/index.json
│   └── version.schema.json    # JSON Schema for tools/<id>/versions/<v>.json
├── fixtures/
│   ├── sample-tool/           # minimal fixture exercising the schemas
│   └── _negative/             # broken fixtures that MUST fail validation
├── tools/                     # populated tool by tool — pilot is rust (#401)
│   └── <id>/
│       ├── index.json
│       ├── versions/<v>.json
│       └── recipes/<name>.json
├── snapshots/                 # frozen catalog snapshots for client pinning (planned)
└── .github/workflows/
    └── validate.yml           # ajv-cli validation on every push/PR
```

`tools/`, `snapshots/`, and `recipes/` are scaffolded by the schemas but
populated by follow-up issues (#401 onward).

## Schema versioning

Both schemas declare `schemaVersion: 1` as a const. Every breaking change
bumps the integer **and** cuts a new repo tag (`v0.1.0`, `v0.2.0`, …).
Consumers compare `schemaVersion` on read and refuse to load unknown
versions. Non-breaking additions (new optional fields) keep the version.

## 7-tier activity model

The `activity.score` field on every tool index is one of seven tiers.
Stibbons uses this for recommendation gating:

| Tier          | Meaning                                          | Stibbons behavior |
| ------------- | ------------------------------------------------ | ----------------- |
| `very-active` | multiple releases per quarter, daily commits     | Recommend         |
| `active`      | regular releases, weekly commits                 | Recommend         |
| `maintained`  | occasional releases, security fixes flow         | Recommend         |
| `slow`        | months between releases but still alive          | Warn              |
| `stale`       | over a year without a release                    | Warn              |
| `dormant`     | upstream silent, no clear successor              | Refuse            |
| `abandoned`   | upstream archived or explicitly EOL              | Refuse            |

The cutoff is **`maintained`**: anything `maintained` or better is
recommended; `slow`/`stale` warn; `dormant`/`abandoned` are refused entirely.
Scan cadence decays with activity — very-active tools are rescanned daily,
maintained ones every 30 days, stale ones every 90.

## Three-field support model

Every tool version expresses support across three intentionally separate
fields. Don't conflate them:

| Field                                       | Meaning                          | Example use                       |
| ------------------------------------------- | -------------------------------- | --------------------------------- |
| `support_matrix[]` (claim)                  | "we say this runs here"          | resolver gating                   |
| `tested[]` (evidence)                       | "CI proved it ran here"          | release notes, audit              |
| `available[].last_known_good_for` (fossil)  | "last version we tested here"    | finding compatible old versions   |

The claim/evidence split exists because shipping schedules force claims to
move ahead of CI runs; without separate fields, every claim would either
rot into a lie or block on CI.

## Dependency model

Dependencies in this catalog are **flat references**: every dependency
points at another catalog tool by id. There are no string-typed package
arrays — even OS packages like `gcc` and `ca-certificates` are
first-class catalog entries with `kind: "system_package"`. That gives
the future luggage resolver one shape to walk and one place to attach
activity decay, advisory gating, and remediation menus.

### Two levels of dependency

A version file has two distinct dependency arrays. Both share the
`Dependency` shape (`schema/version.schema.json#/$defs/Dependency`):

| Field                                  | Level   | Meaning                                                                          |
| -------------------------------------- | ------- | -------------------------------------------------------------------------------- |
| `requires[]`                           | version | Compatibility expectations that must hold across **all** install methods         |
| `install_methods[].dependencies[]`     | method  | Physical install chain for **this specific** install method on **this** platform |

Use `requires[]` for facts like "this version of cargo_X needs rust >=
1.8.5" — solver input that captures the diamond case where two install
methods of two different tools both pull in the same dep at conflicting
constraints. Use `install_methods[].dependencies[]` for the actual
prerequisites the platform needs before luggage runs the method's
fetch/extract/invoke steps (typically the system_package entries the
host package manager will install).

### `kind: "system_package"` entries

A system_package catalog entry is a tracking record only. It carries:

- `system_package.platforms` — a map of distro id → per-distro package
  name (e.g., `libc_dev` maps `debian: libc6-dev`, `rhel: glibc-devel`).
  luggage looks this up at install time when a Dependency targets the
  entry.
- `activity` — same scoring as any other tool, so a stale or compromised
  system package decays the recommendation tier of everything that
  depends on it.
- `validation_tiers` — typically Tier 1 because distro archives are
  GPG-signed by their respective archive keyrings.

system_packages do **not** carry `default_version` or `available[]` —
the host package manager (apt/apk/yum) owns version selection. The
schema enforces this with a conditional `if/then/else` block on
`tool.schema.json`.

### Constraint is a guard, not a selector

The catalog never **picks** a system_package version. apt does. The
catalog only **refuses** when a known-bad version is in the resolver's
view. That is why exact `version` pins on a Dependency targeting a
`kind: "system_package"` are forbidden — only `version_constraint`
expressions are allowed, and they act as guards (e.g. "refuse if
glibc < 2.31"). This rule cannot be expressed in pure JSON Schema
across files and is enforced by `scripts/validate-cross-refs.mjs`.

Constraint expressions are parsed by `containers-common::version`, the
shared Rust parser/comparator that backs every consumer of the catalog
(stibbons, luggage, and this repo's CI validator). One library, one
grammar — see `validator/` for the CI integration.

### Deliberate handoff to apt

Transitive system dependencies — what depends on glibc, what gets
pulled in by `apt install gcc` — are left to the host package manager.
The catalog tracks the direct edge ("rust depends on libc_dev"); apt
pulls in libstdc++, gcc-runtime, and the rest. We rely on the distro
security feeds (DSA / USN / RHSA) to cover transitive system libs;
wiring those feeds into the resolver's advisory-gating policy is a
separate piece of work, tracked as a follow-up.

### Alternatives are advisory only

A tool's `alternatives[]` array points at related tools (similar
capabilities, partial overlap, succession). The resolver **never
auto-substitutes** — alternatives surface only in remediation menus
and `stibbons info` output. Auto-substitution would silently change
what gets installed, which is the opposite of what a reproducible
catalog is for.

## Snapshot pinning

`snapshots/YYYY-MM-DD.json` (planned) freezes the catalog at a point in
time. Consumers (luggage, stibbons) pin a snapshot SHA in their config so
HEAD changes here can't break their builds. Day-to-day catalog updates are
visible only to consumers that explicitly opt in to floating refs.

## Validate locally

The CI workflow (`.github/workflows/validate.yml`) runs the same commands.
To reproduce locally:

```bash
# Compile the schemas (catches schema-internal mistakes)
npx --yes ajv-cli@5 compile --spec=draft2020 -c ajv-formats \
  -s schema/tool.schema.json
npx --yes ajv-cli@5 compile --spec=draft2020 -c ajv-formats \
  -s schema/version.schema.json

# Validate fixtures
npx --yes ajv-cli@5 validate --spec=draft2020 -c ajv-formats \
  -s schema/tool.schema.json -d fixtures/sample-tool/index.json
npx --yes ajv-cli@5 validate --spec=draft2020 -c ajv-formats \
  -s schema/version.schema.json -d "fixtures/sample-tool/versions/*.json"
```

The schemas declare JSON Schema 2020-12; that dialect is required for
`unevaluatedProperties: false` composition (see issue #402, which layers
per-tier conditional validation onto `install_methods[].verification`).

The reserved placeholder vocabulary for `install_methods[].source_url_template`
and `install_methods[].verification.checksum_url_template` (`{version}`,
`{arch}`, `{os}`, `{os_version}`, `{libc}`, `{rustup_target}`,
`{distro_family}`) is documented inline on the `source_url_template`
description in `schema/version.schema.json`. Resolution is the consumer's
job (luggage); ajv does not enforce the vocabulary.

ajv covers shape; the Rust validator under `validator/` covers the
cross-file rules JSON Schema cannot express (unknown tool references,
pin-on-system_package, platform narrowing, version/constraint parsing,
mutual exclusion, and `requires[]` intersection across the catalog).
Run it with:

```bash
cargo run --release --manifest-path validator/Cargo.toml --bin validate-catalog
```

To re-run rule checks against a single file (the form CI uses for
negative fixtures):

```bash
cargo run --release --manifest-path validator/Cargo.toml \
  --bin validate-catalog -- --only fixtures/_negative/dep-pin-on-system-package.json
```

## Related

- Design notes:
  [`luggage-tooldb-design.md`](https://github.com/joshjhall/containers/blob/main/.claude/memory/luggage-tooldb-design.md)
  in the parent repo.
- Foundational issue: joshjhall/containers#400.
- Pilot dataset (rust): joshjhall/containers#401.
- Tier-conditional verification spec: joshjhall/containers#402.
- Luggage crate (consumer): joshjhall/containers#403.
- Daily scanner: joshjhall/containers#406.

## License

Dual-licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <https://opensource.org/licenses/MIT>)

at your option. This is the standard dual-license used across the Rust
ecosystem; it matches the license of [octarine](https://github.com/joshjhall/octarine),
the foundation crate v5 is built on.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
