//! The `cargo_workspace` rule — wraps a Cargo workspace as a coarse outer target
//! (§13.4). Cargo remains the **opaque inner engine** (§3.2): this rule does not
//! model rustc invocations; it emits a handful of coarse actions and lets Cargo own
//! the inner loop.
//!
//! # Milestone 1 increment 1 (this file)
//!
//! One coarse `build` action: `cargo build` over the whole workspace, run hermetically
//! (sealed, network-denied, `--offline --locked`) with the source tree materialized
//! as inputs and the toolchain provided on `PATH`. The result is content-addressed
//! on the sources + toolchain + profile, so an identical workspace hits cache and a
//! source edit rebuilds.
//!
//! Deliberately **not yet** here (subsequent increments, §12.2–12.3, §8.2):
//! per-`(crate, test_type)` test targets with the compile/run split, the conservative
//! `target/` snapshot, `RUSTFLAGS`/sanitizer/coverage axis mapping (§13.6), and
//! dependency vendoring for non-trivial workspaces.

use std::path::{Path, PathBuf};

use anneal_core::{Axis, OptLevel};
use anneal_exec::Action;

use crate::context::RuleContext;
use crate::providers::{ArtifactSource, ProviderSet};
use crate::rule::{Analysis, Rule, RuleError};
use crate::schema::AttrSchema;

/// Directories never treated as build inputs.
const IGNORED_DIRS: &[&str] = &["target", ".git", ".mybuild"];

pub struct CargoWorkspace;

impl Rule for CargoWorkspace {
    fn kind(&self) -> &'static str {
        "cargo_workspace"
    }

    fn schema(&self) -> &'static [AttrSchema] {
        // Milestone 1 increment 1 wraps the whole package directory; no attributes
        // beyond the implicit `name` yet.
        &[]
    }

    fn analyze(&self, ctx: &RuleContext) -> Result<Analysis, RuleError> {
        let toolchain_dirs = toolchain_bin_dirs()?;
        let sources = ctx.source_tree(Path::new("."), IGNORED_DIRS)?;
        if sources.is_empty() {
            return Err(RuleError::Message(
                "cargo_workspace: no source files found in the package".to_owned(),
            ));
        }

        // opt_level → Cargo profile (§13.6). Other axes are deferred.
        let mut command = vec![
            "cargo".to_owned(),
            "build".to_owned(),
            "--offline".to_owned(),
            "--locked".to_owned(),
            "--workspace".to_owned(),
        ];
        match ctx.config().axes().opt_level {
            OptLevel::Release | OptLevel::ReleaseWithDebugInfo => command.push("--release".to_owned()),
            OptLevel::Debug => {}
        }

        // The toolchain is provided via PATH. Under Nix these are content-addressed
        // store paths, so the toolchain version is captured in the cache key. (A
        // principled `register_toolchain` provider, §19.5, is the later replacement.)
        let mut path: Vec<String> = toolchain_dirs
            .iter()
            .map(|d| d.to_string_lossy().into_owned())
            .collect();
        path.push("/usr/bin".to_owned());
        path.push("/bin".to_owned());

        let mut builder = Action::builder(format!("cargo_workspace build {}", ctx.label()), command)
            .env("PATH", path.join(":"))
            .env("CARGO_TERM_COLOR", "never");

        for artifact in &sources {
            if let ArtifactSource::Source(digest) = &artifact.source {
                let name = artifact.path.to_string_lossy().into_owned();
                builder = builder.input(name, artifact.path.clone(), *digest);
            }
        }

        // Consumes opt_level (it picks the profile); other axes are trimmed out.
        builder = builder.configured(ctx.config().clone(), vec![Axis::OptLevel]);

        Ok(Analysis {
            actions: vec![builder.build()],
            providers: ProviderSet::default(),
        })
    }
}

/// The bin directories containing `cargo` and `rustc`, discovered on the ambient
/// `PATH` at analysis time.
fn toolchain_bin_dirs() -> Result<Vec<PathBuf>, RuleError> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for tool in ["cargo", "rustc"] {
        let dir = which_dir(tool).ok_or_else(|| {
            RuleError::Message(format!(
                "`{tool}` not found on PATH; cargo_workspace requires a Rust toolchain"
            ))
        })?;
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    Ok(dirs)
}

/// The directory on `PATH` containing `tool`, if any.
fn which_dir(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find(|dir| dir.join(tool).is_file())
}
