// Unit tests for scripts/validate-cross-refs.mjs.
//
// Exercises the helpers directly (loadCatalog, placeholderCompare,
// dependenciesOf, checkDependency) plus the --only CLI surface via
// child_process — main() calls process.exit() and cannot be invoked
// in-process without contaminating the test runner.
//
// IMPORTANT: when joshjhall/containers-db#7 lands and the placeholder
// comparator is replaced with the real `containers-common::version`
// parser, the placeholderCompare suite below MUST be updated or deleted.
// The cases here are intentionally pinned to the placeholder so a
// regression that silently relaxes its rejection rule fails unit tests,
// not just the catalog walk.
//
// No new dependencies — node:test and node:assert are stdlib in Node 20.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  rmSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname, resolve, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

import {
  loadCatalog,
  placeholderCompare,
  dependenciesOf,
  checkDependency,
} from "./validate-cross-refs.mjs";

const __filename = fileURLToPath(import.meta.url);
const SCRIPT_DIR = dirname(__filename);
const REPO_ROOT = resolve(SCRIPT_DIR, "..");
const SCRIPT_PATH = join(SCRIPT_DIR, "validate-cross-refs.mjs");

// --- placeholderCompare -----------------------------------------------------

test("placeholderCompare accepts '*'", () => {
  const result = placeholderCompare("*", null);
  assert.deepEqual(result, { ok: true });
});

test("placeholderCompare rejects every common comparator", () => {
  for (const constraint of [">=1", "~1.2.0", "1.x", "", "1.0.0"]) {
    const result = placeholderCompare(constraint, null);
    assert.equal(result.ok, false, `expected rejection for ${JSON.stringify(constraint)}`);
    assert.match(
      result.reason,
      /placeholder comparator/,
      `reason must mention placeholder so the upgrade-window guard stays loud (constraint: ${JSON.stringify(constraint)})`,
    );
  }
});

// --- loadCatalog ------------------------------------------------------------

function makeFixtureRoot() {
  const root = mkdtempSync(join(tmpdir(), "xref-test-"));
  mkdirSync(join(root, "tools"), { recursive: true });
  mkdirSync(join(root, "fixtures"), { recursive: true });
  return root;
}

function writeJson(path, doc) {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, JSON.stringify(doc, null, 2));
}

test("loadCatalog ingests valid tools and system_packages, skips no-id entries", (t) => {
  const root = makeFixtureRoot();
  t.after(() => rmSync(root, { recursive: true, force: true }));

  writeJson(join(root, "tools/foo/index.json"), {
    schemaVersion: 1,
    id: "foo",
    kind: "cli",
  });
  writeJson(join(root, "tools/bar/index.json"), {
    schemaVersion: 1,
    id: "bar",
    kind: "system_package",
    system_package: {
      platforms: {
        debian: { name: "bar" },
        alpine: { name: "bar" },
      },
    },
  });
  // Index without `id` — must be silently skipped (real-world shape:
  // a stray index.json that hasn't been filled in yet).
  writeJson(join(root, "tools/orphan/index.json"), {
    schemaVersion: 1,
    kind: "cli",
  });

  const catalog = loadCatalog(root);

  assert.equal(catalog.size, 2, "missing-id entries must be skipped");
  assert.equal(catalog.get("foo").kind, "cli");
  assert.equal(catalog.get("foo").system_package_platforms, null);

  const bar = catalog.get("bar");
  assert.equal(bar.kind, "system_package");
  assert.ok(bar.system_package_platforms instanceof Set);
  assert.deepEqual(
    [...bar.system_package_platforms].sort(),
    ["alpine", "debian"],
  );
});

test("loadCatalog throws with file path when an index.json is malformed", (t) => {
  const root = makeFixtureRoot();
  t.after(() => rmSync(root, { recursive: true, force: true }));

  const brokenPath = join(root, "tools/broken/index.json");
  mkdirSync(dirname(brokenPath), { recursive: true });
  writeFileSync(brokenPath, "{ not valid json");

  assert.throws(
    () => loadCatalog(root),
    (err) => {
      assert.match(err.message, /failed to parse/);
      assert.ok(
        err.message.includes(brokenPath),
        `error must include the offending path; got: ${err.message}`,
      );
      return true;
    },
  );
});

// --- dependenciesOf ---------------------------------------------------------

test("dependenciesOf yields requires[] then install_methods[].dependencies[]", () => {
  const doc = {
    requires: [{ tool: "rust", version_constraint: ">=1.7.0" }],
    install_methods: [
      { name: "tarball", dependencies: [{ tool: "gcc" }, { tool: "libc_dev" }] },
      { name: "apt", dependencies: [] },
    ],
  };
  const yielded = [...dependenciesOf(doc)];
  assert.equal(yielded.length, 3);
  assert.equal(yielded[0].location, "requires[]");
  assert.equal(yielded[0].dep.tool, "rust");
  assert.equal(yielded[1].location, "install_methods[0].dependencies[]");
  assert.equal(yielded[2].dep.tool, "libc_dev");
});

test("dependenciesOf handles missing arrays without throwing", () => {
  assert.deepEqual([...dependenciesOf({})], []);
});

// --- checkDependency --------------------------------------------------------

function makeCatalog() {
  return new Map([
    ["sample_cli", { kind: "cli", system_package_platforms: null, source: "x" }],
    [
      "gcc",
      {
        kind: "system_package",
        system_package_platforms: new Set(["debian", "ubuntu", "alpine", "rhel"]),
        source: "x",
      },
    ],
    [
      "musl_dev",
      {
        kind: "system_package",
        system_package_platforms: new Set(["alpine"]),
        source: "x",
      },
    ],
  ]);
}

test("checkDependency flags unknown tool ids", () => {
  const errors = [];
  checkDependency(
    { dep: { tool: "ghost" }, location: "requires[]" },
    "test.json",
    makeCatalog(),
    errors,
  );
  assert.equal(errors.length, 1);
  assert.match(errors[0], /references unknown tool id/);
  assert.match(errors[0], /"ghost"/);
});

test("checkDependency forbids version pin on a system_package", () => {
  const errors = [];
  checkDependency(
    {
      dep: { tool: "gcc", version: "13.2.0" },
      location: "install_methods[0].dependencies[]",
    },
    "test.json",
    makeCatalog(),
    errors,
  );
  assert.equal(errors.length, 1);
  assert.match(errors[0], /pin .* on system_package `gcc` is forbidden/);
});

test("checkDependency flags platform narrowing to an undeclared distro", () => {
  const errors = [];
  checkDependency(
    {
      dep: { tool: "musl_dev", platforms: ["debian"] },
      location: "install_methods[0].dependencies[]",
    },
    "test.json",
    makeCatalog(),
    errors,
  );
  assert.equal(errors.length, 1);
  assert.match(errors[0], /narrows to platform `debian`/);
  assert.match(errors[0], /declared: \[alpine\]/);
});

test("checkDependency passes a well-formed dep through silently", () => {
  const errors = [];
  checkDependency(
    {
      dep: { tool: "sample_cli", version_constraint: "*" },
      location: "requires[]",
    },
    "test.json",
    makeCatalog(),
    errors,
  );
  assert.deepEqual(errors, []);
});

// --- --only CLI mode --------------------------------------------------------

function runScript(args, opts = {}) {
  return spawnSync("node", [SCRIPT_PATH, ...args], {
    cwd: opts.cwd ?? REPO_ROOT,
    encoding: "utf8",
  });
}

test("--only on a known-good fixture exits 0", () => {
  const result = runScript([
    "--only",
    "fixtures/sample-tool/versions/1.0.0.json",
  ]);
  assert.equal(result.status, 0, `stderr: ${result.stderr}`);
  assert.match(result.stdout, /cross-ref OK/);
});

test("--only on a known-bad fixture exits 1 with a diagnostic", () => {
  const result = runScript([
    "--only",
    "fixtures/_negative/dep-pin-on-system-package.json",
  ]);
  assert.equal(result.status, 1);
  assert.match(result.stderr, /pin .* on system_package `gcc` is forbidden/);
});

test("--only on malformed JSON exits 1 and surfaces the parse error", (t) => {
  const tmp = mkdtempSync(join(tmpdir(), "xref-malformed-"));
  t.after(() => rmSync(tmp, { recursive: true, force: true }));

  const malformedPath = join(tmp, "broken.json");
  writeFileSync(malformedPath, "{ not valid json");

  // The script joins --only against its REPO_ROOT (cwd). path.join keeps
  // absolute path segments intact only on Windows; on POSIX it concatenates,
  // so pass a path relative to REPO_ROOT instead.
  const relMalformed = relative(REPO_ROOT, malformedPath);
  const result = runScript(["--only", relMalformed]);
  assert.equal(result.status, 1, `stdout: ${result.stdout}\nstderr: ${result.stderr}`);
  assert.match(result.stderr, /parse error/);
});
