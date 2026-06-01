//! The `pnpm_workspace` rule — wraps a pnpm workspace as a coarse outer target
//! (§13.4). Like `cargo_workspace`, pnpm remains the **opaque inner engine** (§3.2).
//! Full design and the decisions behind this rule live in `docs/pnpm-workspace.md`.
//!
//! # Milestone-1 build order
//!
//! This module is built incrementally. **Step 1 (this file's current scope) is the
//! `install` action only** — resolution + install, the deterministic, inferred core
//! (`docs/pnpm-workspace.md` §1, §6). The declared-script layer (`scripts`), the
//! `data` routing edge, and the axis mapping are later steps.
//!
//! # The `install` action
//!
//! `pnpm install --offline --frozen-lockfile --ignore-scripts`, run **sealed**:
//!
//! * **`--offline` + `--frozen-lockfile`** mirror `cargo_workspace`'s `--offline
//!   --locked` posture: the lockfile (`pnpm-lock.yaml`) is the source of truth, and
//!   external-dependency vendoring is deferred (the M1 demo uses no registry deps).
//! * **`--ignore-scripts`** enforces the policy decision (§6, §9 of the doc): pnpm ≥ 10
//!   blocks dependency lifecycle scripts by default; we additionally never run them at
//!   install. A native build that needs a compile step is a separate, explicit
//!   `kind = "build"` action — not an opaque install hook.
//!
//! The action declares **only the manifests and lockfile as inputs** (not the whole
//! source tree), so editing a `.ts` file does not bust the install — install depends on
//! the dependency spec, nothing else. `node_modules` and the store are a **snapshot**
//! (§8.2), not content-addressed outputs: they are re-derivable from the lockfile
//! (`docs/rules.md` §5), keyed coarsely on `(platform, toolchain, lockfile)`. **The Node
//! version is deliberately absent from the key** (§6 of the doc): with no install-time
//! native compilation, `node_modules` content is Node-version-independent.

use std::path::{Path, PathBuf};

use anneal_core::Digest;
use anneal_exec::Action;

use crate::context::RuleContext;
use crate::providers::{Artifact, ArtifactSource, ProviderSet};
use crate::rule::{Analysis, Rule, RuleError};
use crate::schema::AttrSchema;

/// Step 1 takes no attributes beyond the implicit `name`. `scripts` and `data` arrive
/// with the script-layer and routing steps respectively.
const SCHEMA: &[AttrSchema] = &[];

/// The sandbox-relative pnpm store directory, kept inside the sandbox (rather than a
/// scrubbed `$HOME`) so the install is hermetic and the store can be snapshotted.
const STORE_DIR: &str = ".pnpm-store";

pub struct PnpmWorkspace;

impl Rule for PnpmWorkspace {
    fn kind(&self) -> &'static str {
        "pnpm_workspace"
    }

    fn schema(&self) -> &'static [AttrSchema] {
        SCHEMA
    }

    fn analyze(&self, ctx: &RuleContext) -> Result<Analysis, RuleError> {
        let toolchain_dirs = toolchain_bin_dirs()?;

        // Install depends on the dependency *spec*, not the source tree: the manifests
        // and the lockfile. Editing application source must not reinstall.
        if !ctx.package_file_exists(Path::new("package.json")) {
            return Err(RuleError::Message(
                "pnpm_workspace: no package.json found in the package".to_owned(),
            ));
        }
        let lockfile = ctx.source_artifact(Path::new("pnpm-lock.yaml")).map_err(|_| {
            RuleError::Message(
                "pnpm_workspace: pnpm-lock.yaml not found; run `pnpm install` first \
                 (we install with --frozen-lockfile)"
                    .to_owned(),
            )
        })?;
        let package_json = ctx.source_artifact(Path::new("package.json"))?;

        let mut path: Vec<String> = toolchain_dirs
            .iter()
            .map(|d| d.to_string_lossy().into_owned())
            .collect();
        path.push("/usr/bin".to_owned());
        path.push("/bin".to_owned());
        let path_env = path.join(":");

        let label = ctx.label().clone();
        let snapshot_key = snapshot_key(&toolchain_dirs, source_digest(&lockfile), ctx);
        // node_modules (incl. the virtual store node_modules/.pnpm) and the
        // content-addressed store are the install layer's mutable cache (§8.2).
        let snapshot_paths = vec![PathBuf::from("node_modules"), PathBuf::from(STORE_DIR)];

        let mut builder = Action::builder(
            format!("pnpm_workspace install {label}"),
            vec![
                "pnpm".to_owned(),
                "install".to_owned(),
                "--offline".to_owned(),
                "--frozen-lockfile".to_owned(),
                "--ignore-scripts".to_owned(),
                format!("--store-dir={STORE_DIR}"),
            ],
        )
        .env("PATH", path_env)
        // Keep pnpm's store inside the sandbox rather than a scrubbed $HOME.
        .env("npm_config_store_dir", STORE_DIR)
        // No update check — a network reach that has no place in a hermetic action.
        .env("npm_config_update_notifier", "false");

        // Declared inputs: the manifests + lockfile only.
        builder = add_source(builder, &package_json);
        builder = add_source(builder, &lockfile);

        // Install consumes no axes (pure resolution; §6 axis matrix). It IS
        // platform-sensitive — pnpm installs only platform-matching optional deps — so
        // the default `platform_sensitive = true` stands (no `.platform_independent()`).
        let action = builder
            .configured(ctx.config().clone(), Vec::new())
            .snapshot(snapshot_key, snapshot_paths)
            .build();

        Ok(Analysis {
            actions: vec![action],
            // Step 1 exposes no providers; the script layer and routing add them.
            providers: ProviderSet::default(),
        })
    }
}

/// Add a resolved source artifact as a content-addressed input at its own path.
fn add_source(builder: anneal_exec::ActionBuilder, artifact: &Artifact) -> anneal_exec::ActionBuilder {
    let name = artifact.path.to_string_lossy().into_owned();
    builder.input(name, artifact.path.clone(), source_digest(artifact))
}

/// Extract the CAS digest of a resolved source artifact. `source_artifact` always
/// yields a [`ArtifactSource::Source`], so the `Output` arm is unreachable.
fn source_digest(artifact: &Artifact) -> Digest {
    match &artifact.source {
        ArtifactSource::Source(digest) => *digest,
        ArtifactSource::Output { .. } => {
            unreachable!("source_artifact yields a Source artifact")
        }
    }
}

/// Coarse snapshot key — `(platform, toolchain, pnpm-lock.yaml)` (§6 of the pnpm doc).
/// The toolchain bin dirs stand in for the pnpm/node version (ad-hoc PATH discovery, as
/// in `cargo_workspace`, until `register_toolchain`). **Node version is intentionally
/// not a separate term**: with `--ignore-scripts` there is no install-time native
/// compilation, so `node_modules` content does not depend on it.
fn snapshot_key(toolchain_dirs: &[PathBuf], lockfile: Digest, ctx: &RuleContext) -> Digest {
    let mut buf = Vec::new();
    for dir in toolchain_dirs {
        buf.extend_from_slice(dir.to_string_lossy().as_bytes());
        buf.push(b':');
    }
    buf.push(0);
    buf.extend_from_slice(lockfile.as_bytes());
    buf.push(0);
    buf.extend_from_slice(ctx.config().platform().target_triple().as_bytes());
    Digest::of(&buf)
}

/// The bin directories containing `pnpm` and `node`, discovered on `PATH`.
fn toolchain_bin_dirs() -> Result<Vec<PathBuf>, RuleError> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for tool in ["pnpm", "node"] {
        let dir = which_dir(tool).ok_or_else(|| {
            RuleError::Message(format!(
                "`{tool}` not found on PATH; pnpm_workspace requires pnpm (>= 10) and node"
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
