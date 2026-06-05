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
//!   convention). Each script action is **`SnapshotConsuming`**: it shares `install`'s
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
//! Deferred (see `docs/pnpm-workspace.md` §8): the `data` routing (plain-path for M1 — a
//! generated file as a direct relative-path input to the consumer; name-resolution is a
//! gated enhancement, §4); the axis mapping (scripts consume no axes yet); a script's *own*
//! build-incremental snapshot (`.tsbuildinfo`, a second snapshot under a different key — one
//! `snapshot_key` per action today); the sealed+reproducibility-gated cache opt-in.

use std::path::{Path, PathBuf};

use anneal_core::Digest;
use anneal_exec::{Action, ActionBuilder, Toolchain};

use crate::attrs::AttrValue;
use crate::context::RuleContext;
use crate::providers::{Artifact, ArtifactSource, FileSet, ProviderSet};
use crate::rule::{Analysis, Rule, RuleError};
use crate::schema::{AttrSchema, AttrType};
use crate::toolchain::{nix_base_runtime, nix_store_toolchain, toolchain_path_env};

/// `scripts` is an optional table; the rule validates its structure. `data` routes other
/// targets' file outputs into the workspace — plain-path (§4 of `docs/pnpm-workspace.md`):
/// `{ "//pkg:t": "dest/path" }` materializes the dep's file at `dest/path`, a direct input
/// to the consuming scripts, which read it by relative path.
const SCHEMA: &[AttrSchema] = &[
    AttrSchema::optional("scripts", AttrType::Dict),
    AttrSchema::optional("data", AttrType::LabelKeyedStringDict),
];

/// The sandbox-relative pnpm store directory, kept inside the sandbox (rather than a
/// scrubbed `$HOME`) so the install is hermetic and the store can be snapshotted.
const STORE_DIR: &str = ".pnpm-store";

/// Directories never treated as build inputs.
const IGNORED_DIRS: &[&str] = &["node_modules", ".git", ".anneal"];

pub struct PnpmWorkspace;

impl Rule for PnpmWorkspace {
    fn kind(&self) -> &'static str {
        "pnpm_workspace"
    }

    fn schema(&self) -> &'static [AttrSchema] {
        SCHEMA
    }

    fn analyze(&self, ctx: &RuleContext) -> Result<Analysis, RuleError> {
        let toolchain = nix_store_toolchain("node", &["pnpm", "node"])?;
        let runtime = nix_base_runtime()?;
        let path_env = toolchain_path_env(&[&toolchain, &runtime]);

        // --- install inputs: manifests + lockfile only (not the source tree) ---
        if !ctx.package_file_exists(Path::new("package.json")) {
            return Err(RuleError::Message(
                "pnpm_workspace: no package.json found in the package".to_owned(),
            ));
        }
        let lockfile = ctx
            .source_artifact(Path::new("pnpm-lock.yaml"))
            .map_err(|_| {
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
        let snapshot_key = snapshot_key(&toolchain, &runtime, source_digest(&lockfile), ctx);
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
            &toolchain,
            &runtime,
        );
        // pnpm may atomically refresh the lockfile even under `--frozen-lockfile`
        // (tempfile + rename with identical content). Give it a private writable copy
        // while keeping the lockfile digest in the action key.
        let install = add_writable_source(add_source(install, &package_json), &lockfile)
            .configured(ctx.config().clone(), Vec::new())
            .snapshot(snapshot_key, snapshot_paths.clone())
            .try_build()?;
        actions.push(install);

        // --- per-script actions: SnapshotConsuming (restore node_modules read-only) ---
        let mut provided: Vec<Artifact> = Vec::new();
        if let Some(scripts) = ctx.attrs().dict_opt("scripts")? {
            // Scripts need the actual source to run; install does not.
            let sources = ctx.source_tree(Path::new("."), IGNORED_DIRS)?;
            // Plain-path `data` routing (§4): each routed file is a **direct input to the
            // consuming scripts** at its per-edge destination — not routed through install.
            let routed = resolve_data(ctx)?;

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
                        let action = with_routed(
                            with_sources(
                                with_env(
                                    Action::builder(
                                        format!("pnpm_workspace test {label} {name}"),
                                        test_command(name),
                                    ),
                                    &path_env,
                                    &toolchain,
                                    &runtime,
                                ),
                                &sources,
                            ),
                            &routed,
                        )
                        .output("results.txt", "results.txt")
                        .configured(ctx.config().clone(), Vec::new())
                        .snapshot_restore(snapshot_key, snapshot_paths.clone())
                        .try_build()?;
                        actions.push(action);
                    }
                    "build" => {
                        let outs: Vec<String> = spec
                            .get("outputs")
                            .and_then(AttrValue::as_string_list)
                            .map(<[String]>::to_vec)
                            .unwrap_or_default();
                        let action_id = format!("pnpm_workspace build {label} {name}");
                        let mut builder = with_routed(
                            with_sources(
                                with_env(
                                    Action::builder(
                                        action_id.clone(),
                                        vec!["pnpm".to_owned(), "run".to_owned(), name.clone()],
                                    ),
                                    &path_env,
                                    &toolchain,
                                    &runtime,
                                ),
                                &sources,
                            ),
                            &routed,
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
                            .try_build()?;
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
    vec!["sh".to_owned(), "-c".to_owned(), body]
}

/// Apply the shared environment: toolchain on PATH, an in-sandbox pnpm store, and the
/// update notifier disabled (no network reach).
fn with_env(
    builder: ActionBuilder,
    path_env: &str,
    toolchain: &Toolchain,
    runtime: &Toolchain,
) -> ActionBuilder {
    builder
        .toolchain(toolchain.clone())
        .toolchain(runtime.clone())
        .env("PATH", path_env)
        .env("npm_config_store_dir", STORE_DIR)
        .env("npm_config_update_notifier", "false")
}

/// Add every resolved source artifact as a content-addressed input at its own path.
fn with_sources(mut builder: ActionBuilder, sources: &[Artifact]) -> ActionBuilder {
    for artifact in sources {
        builder = if is_pnpm_lockfile(&artifact.path) {
            add_writable_source(builder, artifact)
        } else {
            add_source(builder, artifact)
        };
    }
    builder
}

/// Resolve the `data` routing (plain-path, §4): each `{ "//dep": "dest" }` entry becomes the
/// dep's single provided file re-pathed to `dest`. The result is attached as a **direct input
/// to the consuming scripts** (not install). M1 routes a single file per edge; a multi-file
/// provider is a deferred (tree-artifact) case.
fn resolve_data(ctx: &RuleContext) -> Result<Vec<Artifact>, RuleError> {
    let mut routed = Vec::new();
    for (dep_label, dest) in ctx.attrs().label_keyed_strings_opt("data")? {
        let dep = ctx
            .deps()
            .iter()
            .find(|d| &d.label == dep_label)
            .ok_or_else(|| {
                RuleError::Message(format!(
                    "pnpm_workspace: data dependency {dep_label} was not resolved"
                ))
            })?;
        let files = dep
            .providers
            .files
            .as_ref()
            .map(|fs| fs.files.as_slice())
            .unwrap_or(&[]);
        match files {
            [one] => routed.push(Artifact {
                path: PathBuf::from(dest),
                source: one.source.clone(),
            }),
            [] => {
                return Err(RuleError::Message(format!(
                    "pnpm_workspace: data dependency {dep_label} provides no files to route"
                )))
            }
            many => {
                return Err(RuleError::Message(format!(
                    "pnpm_workspace: data dependency {dep_label} provides {} files; plain-path \
                     routing needs exactly one (multi-file routing is deferred)",
                    many.len()
                )))
            }
        }
    }
    Ok(routed)
}

/// Add routed `data` artifacts as inputs at their per-edge destination. A resolved source
/// flows in as a blob; a produced output as an action-graph edge resolved at execution.
fn with_routed(mut builder: ActionBuilder, routed: &[Artifact]) -> ActionBuilder {
    for artifact in routed {
        let name = artifact.path.to_string_lossy().into_owned();
        match &artifact.source {
            ArtifactSource::Source(digest) => {
                builder = builder.input(name, artifact.path.clone(), *digest);
            }
            ArtifactSource::Output {
                action,
                name: output,
            } => {
                builder = builder.input_from_output(name, artifact.path.clone(), action, output);
            }
        }
    }
    builder
}

/// Add a single resolved source artifact as a content-addressed input at its own path.
fn add_source(builder: ActionBuilder, artifact: &Artifact) -> ActionBuilder {
    let name = artifact.path.to_string_lossy().into_owned();
    builder.input(name, artifact.path.clone(), source_digest(artifact))
}

/// Add a source artifact as a private writable input while preserving its digest identity.
fn add_writable_source(builder: ActionBuilder, artifact: &Artifact) -> ActionBuilder {
    let name = artifact.path.to_string_lossy().into_owned();
    builder.writable_input(name, artifact.path.clone(), source_digest(artifact))
}

fn is_pnpm_lockfile(path: &Path) -> bool {
    path == Path::new("pnpm-lock.yaml")
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
/// The toolchain term is the canonical `/nix/store/...` identity for pnpm/node. **Node
/// version is intentionally not a separate term**: with `--ignore-scripts` there is no
/// install-time native compilation, so `node_modules` content does not depend on it.
fn snapshot_key(
    toolchain: &Toolchain,
    runtime: &Toolchain,
    lockfile: Digest,
    ctx: &RuleContext,
) -> Digest {
    let mut buf = Vec::new();
    buf.extend_from_slice(toolchain.identity().as_bytes());
    buf.push(0);
    buf.extend_from_slice(runtime.identity().as_bytes());
    buf.push(0);
    buf.extend_from_slice(lockfile.as_bytes());
    buf.push(0);
    buf.extend_from_slice(ctx.config().platform().target_triple().as_bytes());
    Digest::of(&buf)
}
