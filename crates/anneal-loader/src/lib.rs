//! `anneal-loader` — turns `BUILD` files into a [`TargetGraph`] (§4).
//!
//! This crate **hides starlark-rust entirely**. Nothing in its public API mentions a
//! Starlark type: callers get a [`TargetGraph`] of [`TargetDecl`]s on success and a
//! structured [`LoadError`] (with a source location) on failure. The Starlark
//! evaluator, the rule-primitive globals, and value coercion are all internal.
//!
//! Loading one package is three steps:
//! 1. **Evaluate** the `BUILD` file. Rule primitives (`genrule`, `filegroup`,
//!    `alias`) are registered as globals; calling one records a raw target
//!    declaration plus its source location.
//! 2. **Validate** each raw declaration against the rule's [`AttrSchema`] (§4.3):
//!    required attributes present, no unknown attributes, values of the right type;
//!    string-typed label attributes are coerced into [`Label`]s.
//! 3. **Build** the [`TargetDecl`]s (label, kind, typed [`Attrs`], extracted
//!    dependency labels) and collect them into a [`TargetGraph`].
//!
//! [`AttrSchema`]: anneal_rules::AttrSchema
//! [`Attrs`]: anneal_rules::Attrs
//! [`Label`]: anneal_core::Label
//!
//! ## Milestone 1 scope
//!
//! Loads a single package's `BUILD` file. The restricted user-facing subset linting
//! (§4.2), `load()` of `*.bzl` libraries, and multi-package workspace walking are
//! additive and deferred. Rule primitives are the three first-party kinds.

mod error;
mod eval;
mod graph;
mod validate;

use std::collections::BTreeSet;
use std::path::Path;

use anneal_core::Label;
use anneal_rules::RuleRegistry;

pub use error::LoadError;
pub use graph::{TargetDecl, TargetGraph};

/// Load a package's `BUILD` file from a source string into a [`TargetGraph`].
///
/// `package` is the package path (e.g. `"crates/my_lib"`) used to form target
/// labels; `filename` is what appears in diagnostics.
pub fn load_package_str(
    package: &str,
    filename: &str,
    source: &str,
    registry: &RuleRegistry,
) -> Result<TargetGraph, LoadError> {
    let raw = eval::evaluate(filename, package, source)?;
    let mut graph = TargetGraph::default();
    for raw_target in raw {
        graph.insert(validate::build_target(package, raw_target, registry)?);
    }
    Ok(graph)
}

/// Load a package's `BUILD` file from disk: reads `<workspace_root>/<package>/BUILD`.
pub fn load_package(
    workspace_root: &Path,
    package: &str,
    registry: &RuleRegistry,
) -> Result<TargetGraph, LoadError> {
    let path = workspace_root.join(package).join("BUILD");
    let source = std::fs::read_to_string(&path)
        .map_err(|e| LoadError::io(format!("reading {}: {e}", path.display())))?;
    let filename = format!("{package}/BUILD");
    load_package_str(package, &filename, &source, registry)
}

/// Load the transitive **package closure** needed to analyze `target`.
///
/// Starts from `target`'s package, then loads any package introduced by a
/// cross-package dependency edge, repeating until the closure is covered, merging
/// every package's targets into one [`TargetGraph`]. Only **reachable** packages are
/// loaded — building one target in a large monorepo does not parse every `BUILD` file
/// (§4: cross-package on-demand loading is a loader concern; analysis stays a
/// single-graph consumer). The requested target's existence is the analyzer's check.
pub fn load_closure(
    workspace_root: &Path,
    target: &Label,
    registry: &RuleRegistry,
) -> Result<TargetGraph, LoadError> {
    let mut combined = TargetGraph::default();
    let mut loaded: BTreeSet<String> = BTreeSet::new();
    let mut pending: Vec<String> = vec![target.package().to_owned()];

    while let Some(package) = pending.pop() {
        if !loaded.insert(package.clone()) {
            continue; // already merged (a package reached by more than one edge)
        }
        let package_graph = load_package(workspace_root, &package, registry)?;
        for decl in package_graph.into_decls() {
            for dep in &decl.deps {
                if !loaded.contains(dep.package()) {
                    pending.push(dep.package().to_owned());
                }
            }
            combined.insert(decl);
        }
    }
    Ok(combined)
}

/// Load **every** package in the workspace into one [`TargetGraph`] by walking the tree
/// for `BUILD` files.
///
/// Unlike [`load_closure`] (on-demand from a single target), this enumerates the *whole*
/// workspace — required for reverse-dependency queries like `affected`, which must know
/// every target that *could* depend on a changed one (§11.3). Directories like `.git`,
/// the `.anneal` store, `target/`, and `node_modules` are skipped.
pub fn load_workspace(
    workspace_root: &Path,
    registry: &RuleRegistry,
) -> Result<TargetGraph, LoadError> {
    let mut packages = Vec::new();
    collect_packages(workspace_root, Path::new(""), &mut packages)?;

    let mut combined = TargetGraph::default();
    for package in packages {
        for decl in load_package(workspace_root, &package, registry)?.into_decls() {
            combined.insert(decl);
        }
    }
    Ok(combined)
}

/// Directories never descended into while scanning for `BUILD` files.
const WORKSPACE_IGNORED_DIRS: &[&str] =
    &[".git", ".hg", ".svn", ".anneal", "target", "node_modules"];

/// Recursively collect package paths (directories containing a `BUILD` file), relative to
/// the workspace root and `/`-separated. Symlinked directories are not followed.
fn collect_packages(root: &Path, rel: &Path, out: &mut Vec<String>) -> Result<(), LoadError> {
    let dir = root.join(rel);
    if dir.join("BUILD").is_file() {
        out.push(package_path(rel));
    }
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| LoadError::io(format!("scanning {}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| LoadError::io(format!("scanning {}: {e}", dir.display())))?;
        let file_type = entry
            .file_type()
            .map_err(|e| LoadError::io(format!("scanning {}: {e}", dir.display())))?;
        // `file_type` does not follow symlinks, so a symlinked dir reports as a symlink
        // (not a dir) and is skipped — avoiding symlink cycles.
        if file_type.is_dir() {
            let name = entry.file_name();
            if WORKSPACE_IGNORED_DIRS
                .iter()
                .any(|ig| std::ffi::OsStr::new(ig) == name)
            {
                continue;
            }
            collect_packages(root, &rel.join(name), out)?;
        }
    }
    Ok(())
}

/// A relative path as a `/`-separated package path (empty string for the root package).
fn package_path(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
