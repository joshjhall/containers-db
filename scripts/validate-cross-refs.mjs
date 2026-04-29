#!/usr/bin/env node
// Cross-reference + post-ajv constraint validator for the containers-db catalog.
//
// Runs after ajv has validated each file against its schema. Catches the
// rules that JSON Schema cannot express across files:
//
//   1. Every Dependency.tool must reference a tool id that exists in the catalog.
//   2. When a Dependency targets a `kind: system_package` entry, `version` (an
//      exact pin) is forbidden — the host package manager owns version
//      selection; the catalog only guards via `version_constraint`.
//   3. When a Dependency narrows `platforms`, every listed platform must
//      appear in the target system_package's `platforms` map.
//   4. Every `version_constraint` is routed through the placeholder
//      comparator below — currently accepts only `*` and refuses everything
//      else loudly so it is impossible to silently ship the placeholder
//      into production.
//
// Real comparator wiring lands in a follow-up issue (joshjhall/containers#3 in
// the issue #6 thread). When that lands, replace the body of
// `placeholderCompare()` with the real implementation and remove the
// hard-coded refusal of non-`*` constraints.
//
// CLI:
//   node scripts/validate-cross-refs.mjs            # validate the whole catalog
//   node scripts/validate-cross-refs.mjs --only PATH # validate a single file
//                                                    (used by the negative
//                                                    fixture loop in CI)
//
// Exit code is non-zero on any cross-ref or comparator violation.

import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative, basename } from "node:path";
import { fileURLToPath } from "node:url";

const REPO_ROOT = process.cwd();

// --- Catalog discovery ------------------------------------------------------

function walk(dir, predicate, out = []) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (entry.name === "node_modules" || entry.name.startsWith(".")) continue;
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      walk(path, predicate, out);
    } else if (predicate(path)) {
      out.push(path);
    }
  }
  return out;
}

function readJson(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

// Build map: tool_id → { kind, system_package_platforms?, source_path }
export function loadCatalog(repoRoot = REPO_ROOT) {
  const catalog = new Map();
  const indexFiles = [
    ...walk(join(repoRoot, "tools"), (p) => basename(p) === "index.json"),
    ...walk(join(repoRoot, "fixtures"), (p) => basename(p) === "index.json"),
  ];
  for (const file of indexFiles) {
    let doc;
    try {
      doc = readJson(file);
    } catch (err) {
      throw new Error(`failed to parse ${file}: ${err.message}`);
    }
    if (!doc.id) continue;
    catalog.set(doc.id, {
      kind: doc.kind,
      system_package_platforms:
        doc.kind === "system_package"
          ? new Set(Object.keys(doc.system_package?.platforms ?? {}))
          : null,
      source: relative(repoRoot, file),
    });
  }
  return catalog;
}

// --- Placeholder comparator -------------------------------------------------

// TODO(comparator): real version-constraint comparator wiring lands in the
// follow-up issue (joshjhall/containers#3). Until then, this placeholder
// REFUSES every constraint that is not literally `*`, so the placeholder
// cannot accidentally ship into production parsing real constraints.
//
// When the real comparator lands:
//   - Replace this function's body with the real implementation.
//   - Delete the `*`-only assertion.
//   - Remove this TODO block.
//   - The CI step that runs this script will continue to gate the catalog.
export function placeholderCompare(constraint, _targetVersionStyle) {
  if (constraint === "*") return { ok: true };
  return {
    ok: false,
    reason:
      `placeholder comparator: real comparator lands in a follow-up issue, refusing constraint ${JSON.stringify(constraint)} so the placeholder isn't accidentally shipped`,
  };
}

// --- Dependency walking -----------------------------------------------------

// All version-shaped files: tools/<id>/versions/*.json,
// fixtures/sample-tool/versions/*.json, fixtures/tier*-example.json.
export function discoverVersionFiles(repoRoot = REPO_ROOT) {
  const out = [];
  for (const dir of [join(repoRoot, "tools"), join(repoRoot, "fixtures")]) {
    out.push(
      ...walk(dir, (p) => {
        const name = basename(p);
        if (p.includes(`${"_negative"}`)) return false;
        if (name === "index.json") return false;
        if (!name.endsWith(".json")) return false;
        // versions/<v>.json or fixtures/tier*-example.json or sample-tool versions
        if (p.includes("/versions/")) return true;
        if (/tier\d+-example\.json$/.test(name)) return true;
        return false;
      }),
    );
  }
  return out;
}

export function* dependenciesOf(versionDoc) {
  for (const dep of versionDoc.requires ?? []) {
    yield { dep, location: "requires[]" };
  }
  for (const [i, method] of (versionDoc.install_methods ?? []).entries()) {
    for (const dep of method.dependencies ?? []) {
      yield { dep, location: `install_methods[${i}].dependencies[]` };
    }
  }
}

export function checkDependency({ dep, location }, fileLabel, catalog, errors) {
  const target = catalog.get(dep.tool);
  if (!target) {
    errors.push(
      `${fileLabel} ${location}: references unknown tool id ${JSON.stringify(dep.tool)}`,
    );
    return;
  }

  if (target.kind === "system_package" && dep.version != null) {
    errors.push(
      `${fileLabel} ${location}: pin (\`version\`) on system_package \`${dep.tool}\` is forbidden — apt selects, the catalog only guards via \`version_constraint\``,
    );
  }

  if (
    target.kind === "system_package" &&
    Array.isArray(dep.platforms) &&
    target.system_package_platforms
  ) {
    for (const distro of dep.platforms) {
      if (!target.system_package_platforms.has(distro)) {
        errors.push(
          `${fileLabel} ${location}: dep on system_package \`${dep.tool}\` narrows to platform \`${distro}\`, but \`${dep.tool}\` does not declare that platform (declared: [${[...target.system_package_platforms].join(", ") || "none"}])`,
        );
      }
    }
  }

  if (dep.version_constraint != null) {
    const result = placeholderCompare(dep.version_constraint, null);
    if (!result.ok) {
      errors.push(`${fileLabel} ${location}: ${result.reason}`);
    }
  }
}

// --- Main -------------------------------------------------------------------

export function parseArgs(argv) {
  let only = null;
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--only") only = argv[++i];
  }
  return { only };
}

export function main() {
  const { only } = parseArgs(process.argv.slice(2));
  const catalog = loadCatalog();

  const targets = only
    ? [join(REPO_ROOT, only)]
    : discoverVersionFiles();

  const errors = [];
  for (const file of targets) {
    if (!statSync(file).isFile()) continue;
    let doc;
    try {
      doc = readJson(file);
    } catch (err) {
      // For a single negative fixture that's intentionally malformed JSON we
      // surface as a violation; for the bulk pass we already trust ajv to
      // have caught syntax errors, so re-raise.
      errors.push(`${relative(REPO_ROOT, file)}: parse error: ${err.message}`);
      continue;
    }
    const label = relative(REPO_ROOT, file);
    // Only version-shaped files carry Dependency arrays.
    const looksLikeVersion =
      doc && typeof doc === "object" && "install_methods" in doc;
    if (!looksLikeVersion) continue;
    for (const entry of dependenciesOf(doc)) {
      checkDependency(entry, label, catalog, errors);
    }
  }

  if (errors.length === 0) {
    console.log(
      `cross-ref OK: ${targets.length} file(s) checked against ${catalog.size} catalog entries`,
    );
    process.exit(0);
  }
  for (const err of errors) console.error(`cross-ref: ${err}`);
  console.error(`cross-ref FAILED: ${errors.length} violation(s)`);
  process.exit(1);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  main();
}
