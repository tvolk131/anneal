//! The `pnpm_workspace` rule — wraps a pnpm workspace as a coarse outer target
//! (§13.4). Like `cargo_workspace`, pnpm remains the **opaque inner engine** (§3.2).
//! Full design and the decisions behind this rule live in `docs/pnpm-workspace.md`.
//!
//! # Two layers
//!
//! pnpm is a *package manager + script runner*, not a toolchain, so the rule splits
//! into a layer it **infers** and a layer the user **declares** (`docs/rules.md` §3):
//!
//! * **`install`** — the deterministic, inferred core: `pnpm install --offline
//!   --frozen-lockfile --ignore-scripts`, **sealed**, **`SnapshotBased`** (it owns and
//!   saves the `node_modules` + store snapshot, keyed `(platform, toolchain, lockfile)`;
//!   Node version deliberately absent — §6 of the doc). Declares only the manifests and
//!   lockfile as inputs, so editing application source does not reinstall.
//!
//! * **per-script actions** — the open-ended layer the user declares via the `scripts`
//!   attribute: `scripts = { "test": { "kind": "test" }, "build": { "kind": "build",
//!   "outputs": ["dist"] } }`. The rule *discovers* nothing automatically — the user
//!   names which scripts become actions and of what `kind` (explicit; no name
//!   convention). Each script action is **`SnapshotAccelerated`**: it shares `install`'s
//!   snapshot key to **restore `node_modules` read-only** so the script can run, but is
//!   **never action-cached** (an opaque script's output is not trusted reproducible —
//!   `docs/rules.md` §4–5). There is **no `cacheable` knob**: graduation to a real cache
//!   is a system action after verification, never a consumer assertion.
//!
//! Two `kind`s:
//! * **`test`** — captures the run to `results.txt` and **always exits 0**, so a test
//!   *failure* is recorded data, not a lost action (the `cargo_workspace` test-run idiom).
//! * **`build`** — runs the script, declares its `outputs`, and exposes them as a
//!   provider for downstream consumers.
//!
//! Deferred (see `docs/pnpm-workspace.md` §8): the axis mapping (scripts consume no axes
//! yet); a script's *own* build-incremental snapshot (`.tsbuildinfo`, a second snapshot
//! under a different key — one `snapshot_key` per action today); the `file:` data routing;
//! the sealed+reproducibility-gated cache opt-in.

use std::path::{Path, PathBuf};

use anneal_core::Digest;
use anneal_exec::{Action, ActionBuilder};

use crate::attrs::AttrValue;
use crate::context::RuleContext;
use crate::providers::{Artifact, ArtifactSource, FileSet, ProviderSet};
use crate::rule::{Analysis, Rule, RuleError};
use crate::schema::{AttrSchema, AttrType};

/// `scripts` is an optional table; the rule (not the schema) validates its structure.
const SCHEMA: &[AttrSchema] = &[AttrSchema::optional("scripts", AttrType::Dict)];

/// The sandbox-relative pnpm store directory, kept inside the sandbox (rather than a
/// scrubbed `$HOME`) so the install is hermetic and the store can be snapshotted.
const STORE_DIR: &str = ".pnpm-store";

/// Directories never treated as build inputs.
const IGNORED_DIRS: &[&str] = &["node_modules", ".git", ".mybuild"];

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
        let path_env = path_env(&toolchain_dirs);

        // --- install inputs: manifests + lockfile only (not the source tree) ---
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

        let label = ctx.label().clone();
        // The same snapshot key is shared by install (which saves) and every script
        // (which restores) — the concrete form of "the edge carries the install-snapshot
        // identity" (§6 of the pnpm doc).
        let snapshot_key = snapshot_key(&toolchain_dirs, source_digest(&lockfile), ctx);
        let snapshot_paths = vec![PathBuf::from("node_modules"), PathBuf::from(STORE_DIR)];

        let mut actions = Vec::new();

        // --- install action: SnapshotBased (owns + saves the node_modules snapshot) ---
        let install = with_env(
            Action::builder(
                format!("pnpm_workspace install {label}"),
                vec![
                    "pnpm".to_owned(),
                    "install".to_owned(),
                    "--offline".to_owned(),
                    "--frozen-lockfile".to_owned(),
                    "--ignore-scripts".to_owned(),
                    format!("--store-dir={STORE_DIR}"),
                ],
            ),
            &path_env,
        );
        let install = add_source(add_source(install, &package_json), &lockfile)
            .configured(ctx.config().clone(), Vec::new())
            .snapshot(snapshot_key, snapshot_paths.clone())
            .build();
        actions.push(install);

        // --- per-script actions: SnapshotAccelerated (restore node_modules read-only) ---
        let mut provided: Vec<Artifact> = Vec::new();
        if let Some(scripts) = ctx.attrs().dict_opt("scripts")? {
            // Scripts need the actual source to run; install does not.
            let sources = ctx.source_tree(Path::new("."), IGNORED_DIRS)?;

            for (name, spec) in scripts {
                let spec = spec.as_dict().ok_or_else(|| {
                    RuleError::Message(format!("pnpm_workspace: scripts[{name:?}] must be a table"))
                })?;
                let kind = spec.get("kind").and_then(AttrValue::as_str).ok_or_else(|| {
                    RuleError::Message(format!(
                        "pnpm_workspace: scripts[{name:?}] requires a string `kind` (\"test\" or \"build\")"
                    ))
                })?;

                match kind {
                    "test" => {
                        let action = with_sources(
                            with_env(
                                Action::builder(
                                    format!("pnpm_workspace test {label} {name}"),
                                    test_command(name),
                                ),
                                &path_env,
                            ),
                            &sources,
                        )
                        .output("results.txt", "results.txt")
                        .configured(ctx.config().clone(), Vec::new())
                        .snapshot_restore(snapshot_key, snapshot_paths.clone())
                        .build();
                        actions.push(action);
                    }
                    "build" => {
                        let outs: Vec<String> = spec
                            .get("outputs")
                            .and_then(AttrValue::as_string_list)
                            .map(<[String]>::to_vec)
                            .unwrap_or_default();
                        let action_id = format!("pnpm_workspace build {label} {name}");
                        let mut builder = with_sources(
                            with_env(
                                Action::builder(
                                    action_id.clone(),
                                    vec!["pnpm".to_owned(), "run".to_owned(), name.clone()],
                                ),
                                &path_env,
                            ),
                            &sources,
                        );
                        for out in &outs {
                            builder = builder.output(out.clone(), PathBuf::from(out));
                            provided.push(Artifact {
                                path: PathBuf::from(out),
                                source: ArtifactSource::Output {
                                    action: action_id.clone(),
                                    name: out.clone(),
                                },
                            });
                        }
                        let action = builder
                            .configured(ctx.config().clone(), Vec::new())
                            .snapshot_restore(snapshot_key, snapshot_paths.clone())
                            .build();
                        actions.push(action);
                    }
                    other => {
                        return Err(RuleError::Message(format!(
                            "pnpm_workspace: scripts[{name:?}] has unknown kind {other:?}; \
                             expected \"test\" or \"build\""
                        )));
                    }
                }
            }
        }

        let providers = if provided.is_empty() {
            ProviderSet::default()
        } else {
            ProviderSet {
                files: Some(FileSet { files: provided }),
            }
        };

        Ok(Analysis { actions, providers })
    }
}

/// The shell command for a `test`-kind script: run it, capture stdout+stderr to
/// `results.txt`, record the inner exit code, and **always exit 0** so a test failure is
/// recorded data (parsed by `anneal-test`), not a lost action error.
fn test_command(script: &str) -> Vec<String> {
    let body = format!(
        "pnpm run {script} > results.txt 2>&1; code=$?\n\
         printf 'ANNEAL_TEST_EXIT=%s\\n' \"$code\" >> results.txt\n"
    );
    vec!["/bin/sh".to_owned(), "-c".to_owned(), body]
}

/// Apply the shared environment: toolchain on PATH, an in-sandbox pnpm store, and the
/// update notifier disabled (no network reach).
fn with_env(builder: ActionBuilder, path_env: &str) -> ActionBuilder {
    builder
        .env("PATH", path_env)
        .env("npm_config_store_dir", STORE_DIR)
        .env("npm_config_update_notifier", "false")
}

/// PATH = toolchain bin dirs, then the system locations.
fn path_env(toolchain_dirs: &[PathBuf]) -> String {
    let mut path: Vec<String> = toolchain_dirs
        .iter()
        .map(|d| d.to_string_lossy().into_owned())
        .collect();
    path.push("/usr/bin".to_owned());
    path.push("/bin".to_owned());
    path.join(":")
}

/// Add every resolved source artifact as a content-addressed input at its own path.
fn with_sources(mut builder: ActionBuilder, sources: &[Artifact]) -> ActionBuilder {
    for artifact in sources {
        builder = add_source(builder, artifact);
    }
    builder
}

/// Add a single resolved source artifact as a content-addressed input at its own path.
fn add_source(builder: ActionBuilder, artifact: &Artifact) -> ActionBuilder {
    let name = artifact.path.to_string_lossy().into_owned();
    builder.input(name, artifact.path.clone(), source_digest(artifact))
}

/// Extract the CAS digest of a resolved source artifact. `source_artifact`/`source_tree`
/// always yield [`ArtifactSource::Source`], so the `Output` arm is unreachable.
fn source_digest(artifact: &Artifact) -> Digest {
    match &artifact.source {
        ArtifactSource::Source(digest) => *digest,
        ArtifactSource::Output { .. } => {
            unreachable!("source artifacts are always a Source")
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
