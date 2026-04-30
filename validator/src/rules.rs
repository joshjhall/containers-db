//! Cross-file semantic rules for the containers-db tool catalog.
//!
//! Runs after ajv has validated each file's shape. Catches the
//! invariants JSON Schema cannot express across files:
//!
//!   1. Every `Dependency.tool` must reference a tool id that exists in
//!      the catalog.
//!   2. When a `Dependency` targets a `kind: system_package` entry,
//!      `version` (an exact pin) is forbidden — apt selects, the
//!      catalog only guards via `version_constraint`.
//!   3. When a `Dependency` narrows `platforms`, every listed platform
//!      must appear in the target system_package's `platforms` map.
//!   4. Every `version` and `version_constraint` parses through
//!      `containers_common::version` against the target tool's
//!      `version_style`.
//!   5. `version` (pin) and `version_constraint` (range) are mutually
//!      exclusive on the same `Dependency` edge. ajv catches this via
//!      `oneOf`; we restate it for a clearer message.
//!   6. Every `requires[]` edge that targets the same tool must
//!      collectively intersect to a non-empty constraint. If two
//!      consumers ask for disjoint ranges, the catalog has no
//!      satisfiable starting state.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use containers_common::version::{Constraint, Version, VersionStyle};
use serde::Deserialize;
use walkdir::WalkDir;

use crate::catalog::Catalog;

/// One rule violation, ready for `Display`.
#[derive(Debug)]
pub struct Diagnostic {
    /// Repo-relative path of the file the violation was found in, or
    /// `[catalog-wide]` for cross-file violations (rule 6).
    pub origin: String,
    /// JSON Pointer into that file (e.g. `/install_methods/0/dependencies/0`),
    /// or empty for cross-file diagnostics.
    pub pointer: String,
    pub message: String,
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.pointer.is_empty() {
            write!(f, "{}: {}", self.origin, self.message)
        } else {
            write!(f, "{}:{}: {}", self.origin, self.pointer, self.message)
        }
    }
}

/// Run all rules against the catalog plus the version-shaped files
/// at `targets`. If `targets` is empty, every version-shaped file under
/// `tools/` and `fixtures/` (excluding `_negative/`) is walked.
pub fn check_all(
    repo_root: &Path,
    catalog: &Catalog,
    only: Option<&Path>,
) -> Result<Vec<Diagnostic>, RulesError> {
    let mut diagnostics = Vec::new();

    let files = match only {
        Some(p) => vec![p.to_path_buf()],
        None => discover_version_files(repo_root)?,
    };

    // Aggregator for rule 6 — populated by both full-catalog and
    // --only walks. In --only mode the aggregation only sees the
    // current file's requires[] edges, so the intersection check is
    // self-conflict only (e.g. one file with two disjoint requires
    // edges on the same tool). Cross-file conflicts are still caught
    // by the full-catalog pass that CI runs first.
    let mut requires_index: BTreeMap<String, Vec<RequiresEdge>> = BTreeMap::new();

    for file in &files {
        let bytes = match std::fs::read(file) {
            Ok(b) => b,
            Err(e) => {
                return Err(RulesError::Read {
                    path: file.clone(),
                    source: e,
                });
            }
        };
        let label = relative_path(repo_root, file);

        let doc: VersionDoc = match serde_json::from_slice(&bytes) {
            Ok(d) => d,
            Err(e) => {
                diagnostics.push(Diagnostic {
                    origin: label,
                    pointer: String::new(),
                    message: format!("parse error: {e}"),
                });
                continue;
            }
        };

        // Only files that look like a version doc carry Dependency arrays.
        // (Tool indexes and tier examples without install_methods are
        // skipped silently — same shape as the JS predecessor.)
        if doc.install_methods.is_none() && doc.requires.is_none() {
            continue;
        }

        let label = relative_path(repo_root, file);

        for (idx, dep) in doc.requires.as_deref().unwrap_or_default().iter().enumerate() {
            let pointer = format!("/requires/{idx}");
            check_dep(dep, &pointer, &label, catalog, &mut diagnostics);
            // Stash for rule 6.
            if let Some(c) = &dep.version_constraint {
                requires_index
                    .entry(dep.tool.clone())
                    .or_default()
                    .push(RequiresEdge {
                        constraint_str: c.clone(),
                        origin: label.clone(),
                        pointer: pointer.clone(),
                    });
            }
        }

        for (mi, method) in doc
            .install_methods
            .as_deref()
            .unwrap_or_default()
            .iter()
            .enumerate()
        {
            for (di, dep) in method.dependencies.iter().enumerate() {
                let pointer = format!("/install_methods/{mi}/dependencies/{di}");
                check_dep(dep, &pointer, &label, catalog, &mut diagnostics);
            }
        }
    }

    check_requires_intersection(catalog, &requires_index, &mut diagnostics);

    Ok(diagnostics)
}

/// Files the rule walker considers: every `versions/<v>.json` plus the
/// `fixtures/tier*-example.json` set. Skips `_negative/`,
/// `node_modules/`, and Rust `target/`.
fn discover_version_files(repo_root: &Path) -> Result<Vec<PathBuf>, RulesError> {
    let mut out = Vec::new();
    for root in [repo_root.join("tools"), repo_root.join("fixtures")] {
        if !root.exists() {
            continue;
        }
        for entry in WalkDir::new(&root).into_iter().filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !(name == "_negative" || name == "node_modules" || name == "target")
        }) {
            let entry = entry.map_err(|e| RulesError::Walk(e.to_string()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "index.json" {
                continue;
            }
            if !name.ends_with(".json") {
                continue;
            }

            // versions/<v>.json — under any directory called `versions`
            let in_versions = path
                .components()
                .any(|c| c.as_os_str() == "versions");
            // fixtures/tier{N}-example.json
            let is_tier_example = name.starts_with("tier") && name.ends_with("-example.json");
            if in_versions || is_tier_example {
                out.push(path.to_path_buf());
            }
        }
    }
    Ok(out)
}

fn relative_path(repo_root: &Path, path: &Path) -> String {
    path.strip_prefix(repo_root)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[derive(Debug, Deserialize)]
struct VersionDoc {
    #[serde(default)]
    requires: Option<Vec<RawDependency>>,
    #[serde(default)]
    install_methods: Option<Vec<RawInstallMethod>>,
}

#[derive(Debug, Deserialize)]
struct RawInstallMethod {
    #[serde(default)]
    dependencies: Vec<RawDependency>,
}

#[derive(Debug, Deserialize)]
struct RawDependency {
    tool: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    version_constraint: Option<String>,
    #[serde(default)]
    platforms: Option<Vec<String>>,
}

struct RequiresEdge {
    constraint_str: String,
    origin: String,
    pointer: String,
}

fn check_dep(
    dep: &RawDependency,
    pointer: &str,
    origin: &str,
    catalog: &Catalog,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Rule 1: unknown tool reference.
    let Some(target) = catalog.get(&dep.tool) else {
        diagnostics.push(Diagnostic {
            origin: origin.to_string(),
            pointer: pointer.to_string(),
            message: format!("references unknown tool id \"{}\"", dep.tool),
        });
        return;
    };

    // Rule 5: version + version_constraint mutually exclusive.
    if dep.version.is_some() && dep.version_constraint.is_some() {
        diagnostics.push(Diagnostic {
            origin: origin.to_string(),
            pointer: pointer.to_string(),
            message: format!(
                "`version` (pin) and `version_constraint` (range) are mutually exclusive on the same dependency edge (target: `{}`)",
                dep.tool,
            ),
        });
    }

    // Rule 2: pin on system_package is forbidden.
    if target.kind == "system_package" && dep.version.is_some() {
        diagnostics.push(Diagnostic {
            origin: origin.to_string(),
            pointer: pointer.to_string(),
            message: format!(
                "pin (`version`) on system_package `{}` is forbidden — apt selects, the catalog only guards via `version_constraint`",
                dep.tool,
            ),
        });
    }

    // Rule 3: platform narrowing must intersect target's platform map.
    if target.kind == "system_package"
        && let (Some(narrowed), Some(declared)) =
            (dep.platforms.as_ref(), target.system_package_platforms.as_ref())
    {
        for distro in narrowed {
            if !declared.contains(distro) {
                let declared_list: Vec<&str> = declared.iter().map(String::as_str).collect();
                let declared_str = if declared_list.is_empty() {
                    "none".to_string()
                } else {
                    declared_list.join(", ")
                };
                diagnostics.push(Diagnostic {
                    origin: origin.to_string(),
                    pointer: pointer.to_string(),
                    message: format!(
                        "dep on system_package `{}` narrows to platform `{}`, but `{}` does not declare that platform (declared: [{}])",
                        dep.tool, distro, dep.tool, declared_str,
                    ),
                });
            }
        }
    }

    // Rule 4: version literal parses against the target's version_style.
    if let Some(v) = &dep.version
        && let Err(e) = Version::parse(v, target.version_style)
    {
        diagnostics.push(Diagnostic {
            origin: origin.to_string(),
            pointer: pointer.to_string(),
            message: format!(
                "unparseable version literal `{v}` (target `{}`, style {:?}): {e}",
                dep.tool, target.version_style,
            ),
        });
    }

    // Rule 4: constraint expression parses against the target's version_style.
    if let Some(c) = &dep.version_constraint
        && let Err(e) = Constraint::parse(c, target.version_style)
    {
        diagnostics.push(Diagnostic {
            origin: origin.to_string(),
            pointer: pointer.to_string(),
            message: format!(
                "unparseable constraint `{c}` (target `{}`, style {:?}): {e}",
                dep.tool, target.version_style,
            ),
        });
    }
}

fn check_requires_intersection(
    catalog: &Catalog,
    requires_index: &BTreeMap<String, Vec<RequiresEdge>>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (tool_id, edges) in requires_index {
        if edges.len() < 2 {
            continue;
        }
        // Use the target's style; if the target is unknown, rule 1
        // already reported each edge individually.
        let Some(target) = catalog.get(tool_id) else {
            continue;
        };
        let style: VersionStyle = target.version_style;

        // Parse each edge's constraint. If any failed to parse, rule 4
        // already reported it; skip to avoid duplicate noise.
        let mut parsed: Vec<(&RequiresEdge, Constraint)> = Vec::with_capacity(edges.len());
        for edge in edges {
            match Constraint::parse(&edge.constraint_str, style) {
                Ok(c) => parsed.push((edge, c)),
                Err(_) => return,
            }
        }

        let mut accumulator = parsed[0].1.clone();
        for (edge, c) in parsed.iter().skip(1) {
            match accumulator.intersect(c) {
                Ok(next) => accumulator = next,
                Err(_) => {
                    let edges_summary = parsed
                        .iter()
                        .map(|(e, _)| format!("{}:{} ({})", e.origin, e.pointer, e.constraint_str))
                        .collect::<Vec<_>>()
                        .join("; ");
                    diagnostics.push(Diagnostic {
                        origin: "[catalog-wide]".to_string(),
                        pointer: String::new(),
                        message: format!(
                            "requires[] intersection on `{tool_id}` is unsatisfiable — {edges_summary}",
                        ),
                    });
                    // Move on to the next tool id; one diagnostic per
                    // tool is enough.
                    let _ = edge;
                    break;
                }
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RulesError {
    #[error("walk error: {0}")]
    Walk(String),

    #[error("failed to read {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}
