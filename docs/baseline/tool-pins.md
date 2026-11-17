# Baseline tool pins

The repo-baseline templates pin 14 tools: typos, dprint, taplo, rumdl,
actionlint, gitleaks, conform, lefthook, just, shellcheck, shfmt, bats, ruff and
uv. Their versions and per-arch sha256 values come from the catalog
(`tools/<id>/`) and are **generated** into the template files (#623):

| Generated file                                   | From                                                    |
| ------------------------------------------------ | ------------------------------------------------------- |
| `baseline/templates/{core,shell}/tools.versions` | `tools/<id>/index.json` `default_version`               |
| `baseline/templates/*/tools.sha256`              | each install method's Tier 2 `pinned_checksum` per arch |
| `required-version` in `python/{ruff,uv}.toml.tmpl` | `default_version` of `ruff` / `uv`                    |

Do not edit those files by hand: CI runs `bin/baseline-pins.sh --check` and
fails on any difference.

## Bumping a tool

1. Add `tools/<id>/versions/<new>.json`. Copy the previous version file and
   update `version`, each method's `source_url_template` and its
   `pinned_checksum` (sha256 of that arch's release asset).
1. Set `default_version` in `tools/<id>/index.json` and add the version to
   `available`.
1. Run `bin/baseline-pins.sh`, then
   `tests/baseline/render-parity.test.sh --update`.
1. Commit the catalog, template and golden changes together.

## How per-arch checksums are modeled

The version schema has no per-arch checksum map, and it does not need one. Each
version file carries one `binary-tarball` install method per arch, each with:

- a single `platform.arch` value,
- a literal `source_url_template`,
- `verification: {tier: 2, algorithm: sha256, pinned_checksum}`.

The URL is literal because upstream asset names spell the arch as
`x86_64`/`aarch64` (or `x64`), and the catalog's `{arch}` placeholder produces
`amd64`/`arm64`. When a method covers both arches, as for bats's source tarball,
the generator emits it as `<tool>-any`.

The generator maps arch keys to the installer's `uname -m` spelling: amd64 →
`x86_64`, arm64 → `aarch64`.

These entries do not carry `tested[]` evidence yet. Several assets are raw
binaries, `.gz` or `.zip` files rather than tarballs, so a luggage install of
them is not claimed until it has been exercised.
