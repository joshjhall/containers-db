# Binary tarball across glibc + musl

How to catalog a tool that ships one prebuilt Linux tarball and should install
on every base image: debian, ubuntu, rhel (glibc) and alpine (musl). Go is the
worked example (`tools/go/versions/*.json`, #482).

## When this pattern applies

- Upstream publishes a `linux-{arch}` tarball that does not link against the
  host libc, either fully static or with no libc-version-specific symbols. Go's
  release binaries are statically linked, so the same tarball is expected to
  run on musl; the evidence run is what confirms it (see Evidence below).
- If upstream's tarball is glibc-linked (Node.js is), it will not run on
  alpine. Keep the glibc method and give alpine a source build in its own
  method: see [source-build-musl.md](source-build-musl.md), with node as the
  worked example.

## Shape

Two install methods, even when both download the same tarball:

```json
"install_methods": [
  { "name": "go-tarball",
    "platform": { "os": ["debian", "ubuntu", "rhel"], ... }, ... },
  { "name": "go-tarball",
    "platform": { "os": "alpine", ... }, ... }
]
```

The split mirrors rust's `rustup-init` pair. It exists so that a musl-only fix
(a different URL, an extra dependency, a fallback to source-build) edits only
the alpine method and never changes what glibc hosts install. Luggage picks the
first method whose `platform` matches, so the order does not matter for
dispatch. Keep glibc first by convention.

`support_matrix` lists all 18 cells: debian 12/13, ubuntu 24.04/26.04,
alpine 3.22/3.23/3.24, and rhel 9/10, each on amd64 and arm64. The evidence
run generates cells only for tuples the catalog claims, so a missing row means
that cell is never tested.

## Dependencies

- **`ca_certificates`** on both methods: the tarball is fetched over TLS.
- **No `util_linux` on rhel** unless evidence shows a need. Rust needs it for
  `rustup-init`; a plain tarball unpack does not.
- **No C toolchain (cgo: out).** Go builds pure-Go programs without
  `gcc`/`libc_dev`/`musl_dev`, so the catalog follows the minimal-install
  policy and leaves them off. Users who need `CGO_ENABLED=1` install `gcc`
  plus the libc headers themselves. Rust differs: rustc always invokes a
  linker, so its methods carry `gcc` + `libc_dev`/`musl_dev`. Later tarball
  tools should ask the same question: is the toolchain unusable without a C
  compiler, or only some programs?

The decision lives here rather than in `tools/go/index.json`, because the tool
schema has no free-form notes field.

## Keeping new releases in shape

The scanner has no per-tool version template. `render_version`
(`scanner/src/generate.rs`) clones the newest cataloged file on the new
release's line (`template_for`; see [scanner.md](scanner.md) § 4) and
rewrites the version literals, so the shape of each line's newest file is the
shape of every future release on that line, and of the next new line. `scanner/tests/go_catalog_shape_test.rs` pins go's 18
cells and its method split across every cataloged file and a freshly rendered
version. Copy that test when you move another tool to this pattern.

## Evidence

The catalog claims a cell before it has been evidenced. The containers-side
re-vendor and evidence run (containers#1516 for go) confirms each new cell. If
alpine fails there, record the failure mode on the tool's issue and switch the
alpine method to source-build. The glibc method is untouched either way.
