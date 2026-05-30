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

use std::path::Path;

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
