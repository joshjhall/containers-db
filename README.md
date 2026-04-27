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

## Related

- Design notes:
  [`luggage-tooldb-design.md`](https://github.com/joshjhall/containers/blob/main/.claude/memory/luggage-tooldb-design.md)
  in the parent repo.
- Foundational issue: joshjhall/containers#400.
- Pilot dataset (rust): joshjhall/containers#401.
- Tier-conditional verification spec: joshjhall/containers#402.
- Luggage crate (consumer): joshjhall/containers#403.
- Daily scanner: joshjhall/containers#406.
