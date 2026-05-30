//! The `cargo_workspace` rule — wraps a Cargo workspace as a coarse outer target
//! (§13.4). Cargo remains the **opaque inner engine** (§3.2): this rule does not
//! model rustc invocations; it emits a handful of coarse actions and lets Cargo own
//! the inner loop.
//!
//! # Milestone 1 increment (this file)
//!
//! One coarse `build` action: `cargo build` over the whole workspace, run
//! hermetically (sealed, network-denied, `--offline --locked`) with the source tree
//! materialized as inputs and the toolchain on `PATH`. The action is **snapshot
//! based** (§8.2): Cargo's `target/` is restored before the build and saved after,
//! so a rebuild after a source edit is incremental rather than cold. Each
//! workspace-member library's `.rlib` is declared as an output so the result is
//! content-addressed and can be compared by the correctness-neutral verifier.
//!
//! `CARGO_INCREMENTAL=0`: rustc's incremental codegen is not guaranteed bit-identical
//! to a clean build, which would make a snapshot-warm build legitimately differ from
//! a cold one. Disabling it keeps builds reproducible — a precondition for the §1.4
//! correctness-neutral invariant to be checkable at all.
//!
//! Deliberately **not yet** here: per-`(crate, test_type)` test targets with the
//! compile/run split (§12.2–12.3), `RUSTFLAGS`/sanitizer/coverage axes (§13.6),
//! binary/test artifact outputs, and dependency vendoring.

use std::path::{Path, PathBuf};

use anneal_core::{Axis, Digest, OptLevel};
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

        // opt_level → Cargo profile (§13.6); other axes deferred.
        let opt_level = ctx.config().axes().opt_level;
        let (profile_dir, release) = match opt_level {
            OptLevel::Debug => ("debug", false),
            OptLevel::Release | OptLevel::ReleaseWithDebugInfo => ("release", true),
        };

        let mut command = vec![
            "cargo".to_owned(),
            "build".to_owned(),
            "--offline".to_owned(),
            "--locked".to_owned(),
            "--workspace".to_owned(),
        ];
        if release {
            command.push("--release".to_owned());
        }

        // Toolchain via PATH (Nix store paths ⇒ toolchain version in the cache key).
        let mut path: Vec<String> = toolchain_dirs
            .iter()
            .map(|d| d.to_string_lossy().into_owned())
            .collect();
        path.push("/usr/bin".to_owned());
        path.push("/bin".to_owned());

        let outputs = library_outputs(ctx, profile_dir)?;
        let snapshot_key = snapshot_key(&toolchain_dirs, &sources, ctx, opt_level);

        let mut builder = Action::builder(format!("cargo_workspace build {}", ctx.label()), command)
            .env("PATH", path.join(":"))
            .env("CARGO_TERM_COLOR", "never")
            .env("CARGO_INCREMENTAL", "0");

        for artifact in &sources {
            if let ArtifactSource::Source(digest) = &artifact.source {
                let name = artifact.path.to_string_lossy().into_owned();
                builder = builder.input(name, artifact.path.clone(), *digest);
            }
        }
        for out in &outputs {
            builder = builder.output(out.to_string_lossy().into_owned(), out.clone());
        }
        builder = builder
            .configured(ctx.config().clone(), vec![Axis::OptLevel])
            .snapshot(snapshot_key, vec![PathBuf::from("target")]);

        Ok(Analysis {
            actions: vec![builder.build()],
            providers: ProviderSet::default(),
        })
    }
}

/// The coarse snapshot key: a hash of `(toolchain, lockfile, target_triple, profile)`
/// (§8.2). Coarse enough that a source edit hits the same snapshot; fine enough that
/// a toolchain or lockfile change invalidates it.
fn snapshot_key(
    toolchain_dirs: &[PathBuf],
    sources: &[crate::providers::Artifact],
    ctx: &RuleContext,
    opt_level: OptLevel,
) -> Digest {
    let mut buf = Vec::new();
    for dir in toolchain_dirs {
        buf.extend_from_slice(dir.to_string_lossy().as_bytes());
        buf.push(b':');
    }
    buf.push(0);
    if let Some(ArtifactSource::Source(lock)) = sources
        .iter()
        .find(|a| a.path == Path::new("Cargo.lock"))
        .map(|a| &a.source)
    {
        buf.extend_from_slice(lock.as_bytes());
    }
    buf.push(0);
    buf.extend_from_slice(ctx.config().platform().target_triple().as_bytes());
    buf.push(0);
    buf.extend_from_slice(opt_level.as_str().as_bytes());
    Digest::of(&buf)
}

/// The `.rlib` output path of each workspace-member library crate, by introspecting
/// `Cargo.toml`s. (Binaries, tests, and glob members are deferred increments.)
fn library_outputs(ctx: &RuleContext, profile_dir: &str) -> Result<Vec<PathBuf>, RuleError> {
    let root: toml::Value = ctx
        .read_package_file(Path::new("Cargo.toml"))?
        .parse()
        .map_err(|e| RuleError::Message(format!("parsing Cargo.toml: {e}")))?;

    let mut member_dirs: Vec<PathBuf> = vec![PathBuf::new()]; // the root package, if any
    if let Some(members) = root
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
    {
        for member in members {
            if let Some(s) = member.as_str() {
                if s.contains('*') {
                    continue; // glob members deferred
                }
                member_dirs.push(PathBuf::from(s));
            }
        }
    }

    let mut outputs = Vec::new();
    for dir in member_dirs {
        let manifest = dir.join("Cargo.toml");
        if !ctx.package_file_exists(&manifest) || !ctx.package_file_exists(&dir.join("src/lib.rs"))
        {
            continue;
        }
        let parsed: toml::Value = ctx
            .read_package_file(&manifest)?
            .parse()
            .map_err(|e| RuleError::Message(format!("parsing {}: {e}", manifest.display())))?;
        if let Some(name) = parsed
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
        {
            let lib = format!("lib{}.rlib", name.replace('-', "_"));
            outputs.push(PathBuf::from(format!("target/{profile_dir}/{lib}")));
        }
    }
    Ok(outputs)
}

/// The bin directories containing `cargo` and `rustc`, discovered on `PATH`.
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

fn which_dir(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find(|dir| dir.join(tool).is_file())
}
