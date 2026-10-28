# Source build for missing musl binaries

How to catalog alpine for a tool whose upstream publishes only glibc-linked
prebuilt tarballs: build it from the upstream source tarball with
`method_kind: source-build`. Node.js is the worked example
(`tools/node/versions/*.json`, #483). This is the fallback that
[binary-tarball-glibc-musl.md](binary-tarball-glibc-musl.md) points to when one
tarball cannot serve both libcs.

## When this pattern applies

- Upstream's Linux tarball links glibc, so it will not run on alpine. Node's
  `linux-{x64,arm64}.tar.xz` are glibc builds, and nodejs.org publishes no musl
  build.
- Upstream publishes a source tarball covered by the **same** checksum manifest
  as its binaries, so the alpine method verifies exactly as the glibc one does.
  Node's `node-v{version}.tar.xz` is listed in the same `SHASUMS256.txt`.
- The build is a plain `configure` / `make` / `make install`. The schema can
  express that as-is; luggage's `source_build.rs` runs it in a scratch
  directory and removes the source tree afterwards.

Rejected alternatives for node, and the general reasons behind them:

- **Third-party musl builds** (unofficial-builds.nodejs.org). These are weaker
  provenance than the upstream manifest, and coverage is uneven: arm64 musl
  exists only from node 26.
- **Distro package** (`apk add nodejs`). It can't pin a version, which breaks
  the catalog's contract.

## Shape

The glibc methods stay as they are, and alpine gets one method of its own:

```json
{ "name": "node-source",
  "method_kind": "source-build",
  "platform": { "os": "alpine", "arch": ["amd64", "arm64"] },
  "source_url_template": "https://nodejs.org/dist/v{version}/node-v{version}.tar.xz",
  "strip_components": 1,
  "prefix": "/usr/local",
  "build_steps": [
    { "command": "./configure", "args": ["--prefix=/usr/local"] },
    { "command": "make", "args": ["-j{nproc}"] },
    { "command": "make", "args": ["install"] }
  ],
  "verification": { "...": "identical to the glibc method" } }
```

- `strip_components: 1` flattens `node-v{version}/` so the extracted directory
  is the build root.
- `{nproc}` is the only placeholder `build_steps` accepts. It leaves build
  parallelism to luggage's host.
- `--prefix` must match `prefix`, so that whatever `make install` writes is
  what the catalog says is installed.

Node keeps its glibc tarball split per arch (`node-tarball-amd64`,
`node-tarball-arm64`) because the dependencies differ by arch (below). The
source build needs no such split.

## Dependencies: build vs runtime

Declare each package with the `purpose` that matches its role:

| Tool | Package (alpine) | `purpose` | Why |
|---|---|---|---|
| `gxx` | `g++` | build | node is C++ |
| `make` | `make` | build | drives the build |
| `linux_headers` | `linux-headers` | build | libuv's syscall headers |
| `python3` | `python3` | build | gyp, called by `./configure` |
| `libstdcxx` | `libstdc++` | runtime | `node` links `libstdc++.so.6` |
| `libgcc` | `libgcc` | runtime | `node` links `libgcc_s.so.1` (22+) |

The runtime set is measured, not guessed. `scanelf --needed` on each spike
build listed exactly these libraries, and each binary ran `node` and `npm` in a
fresh alpine that had only the runtime packages installed.

`libatomic` is **not** a dependency of the musl build. The official glibc arm64
tarball links `libatomic.so.1` from node **25.0.0** on, so `node-tarball-arm64`
declares it for 25.0.0 and later only. The musl binary built from source does
not link it. The boundary was measured with `readelf -d` NEEDED: every
cataloged 25.x/26.x tarball links it, and 15 sampled 18.x–24.x tarballs,
including each major's edges, do not. #481 said "26+"; 25.x links it too.

**Luggage gap.** Luggage does not read `purpose` yet: every dependency is
installed by its system-package stage and stays in the final image, including
the toolchain. The catalog still scopes them correctly, so the cleanup can be
added in luggage without a catalog change. Tracked in containers#1524.

## The python floor

A source build inherits upstream's build-time constraints, and node has a sharp
one. Each release's `./configure` checks the host python against a
hard-coded `acceptable_pythons` list and exits when the host python is not on
it. Alpine's `python3` is 3.12 on 3.22/3.23 and 3.14 on 3.24.

A release whose list doesn't include a given alpine's python cannot build
there. Those cells are `status: unsupported` with a `reason`, never omitted,
and luggage's resolver turns that into an `UnsupportedPlatform` error that
carries the reason:

| Group | Releases | alpine 3.22/3.23 | alpine 3.24 |
|---|---|---|---|
| list caps below 3.12 | 59 (18.0.0–18.19.1, 19, 20.0.0–20.10.0, 21.0.0–21.1.0) | unsupported | unsupported |
| accepts 3.12, not 3.14 | 105 | supported | unsupported |
| accepts both | 56 (20.20.1+, 22.22.1+, 24.13.1+, 25, 26) | supported | supported |

The groups come from reading `acceptable_pythons` out of each release's
`configure` at its source tag. Building the oldest release that passes
(18.20.8) confirmed that python, not the compiler, is the floor.

**Keeping it current.** The scanner clones the newest cataloged file on the
new release's own line (22.23.5 from the newest 22.x, see
[scanner.md](scanner.md) § 4), so new releases inherit that line's alpine
statuses. That stays correct while node keeps
accepting the python of each cataloged alpine. When the matrix gains an alpine
release with a newer python, re-check `acceptable_pythons` for every cataloged
version, because older releases will likely drop to `unsupported` for that
cell. `scanner/tests/node_catalog_shape_test.rs` requires every unsupported
alpine row to carry a reason. It does not re-derive the gate.

Keep the reason text version-free ("this release's ./configure …"). The
scanner's `render_version` rewrites only strings that are **entirely** the
version, so a version embedded in a sentence would carry over to the cloned
file and name the wrong release. The shape test rejects a reason that contains
its own file's version.

## Cost model

These numbers come from a native arm64 alpine 3.22 host with 10 cores. Each
build runs on every install, because there is no prebuilt to cache:

| Release | Wall time | Peak RSS | Installed |
|---|---|---|---|
| 22.23.4 | 23m50s | 2.75 GB | 202 MB |
| 18.20.8 | 29m26s\* | 1.35 GB | 161 MB |
| 26.9.0 | 59m22s\* | 2.69 GB | 219 MB |

\* These two ran concurrently and shared the cores, so their times are
inflated. 22.23.4's time, measured alone, is the clean figure.

Plan for roughly 25 minutes of CPU-saturated build and about 3 GB of RAM per
image. A CI runner with fewer cores will take proportionally longer, and one
with under 4 GB of memory may fail at `-j{nproc}`, since peak memory scales with
parallelism. amd64 and alpine 3.24 were not built in the spike. The evidence
run confirms those cells (see below).

## Evidence

As with the tarball pattern, the catalog claims a cell before it is evidenced.
The containers-side re-vendor and evidence run confirms each supported alpine
cell. A cell that fails there gets its failure recorded on the tool's issue and
moves to `unsupported` with that reason.

## Applying this to another tool

1. Confirm upstream signs or checksums its source tarball in the same manifest
   as its binaries. If not, the alpine method drops a verification tier, so
   record that decision explicitly.
1. Spike the build in a throwaway alpine container. Record the minimal build
   dependency set, the runtime libraries (`scanelf --needed`), wall time and
   peak RSS.
1. Look for build-time version gates like node's python list, and turn them
   into `unsupported` rows with reasons.
1. Add `system_package` entries for any new build or runtime packages. Keep
   them alpine-only unless another distro is verified, as `musl_dev` does.
1. Pin the shape with a copy of `scanner/tests/node_catalog_shape_test.rs`.
