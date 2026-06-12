//! The `cargo_workspace` rule — wraps a Cargo workspace as a coarse outer target
//! (§13.4). Cargo remains the **opaque inner engine** (§3.2): this rule does not
//! model rustc invocations; it emits a handful of coarse actions and lets Cargo own
//! the inner loop.
//!
//! # Actions emitted
//!
//! * One coarse **`build`** action (`cargo build --workspace`), declaring each
//!   member library's `.rlib` as an output.
//! * Per `(crate, test_type)` (§12.2), for the types a crate has:
//!   * **unit** (`--lib`): a **compile/run split** (§12.3). The compile action runs
//!     `cargo test --no-run --message-format=json`, extracts the test binary, and
//!     copies it to a stable logical `test-bin` output. The run action consumes that binary
//!     (an action-graph edge) and executes it. Because the run action's cache key is
//!     the *content* of the binary, an unrelated edit busts the compile cache but the
//!     run still **hits** — tests don't re-run when the binary is unchanged.
//!   * **doc** (`--doc`): a single action. Doc tests produce no reusable binary
//!     (§12.3), so they are not split.
//!   * **integration** (`--tests`): a single action for now (multi-binary split
//!     deferred).
//!
//! All cargo actions are **snapshot based** (§8.2): Cargo's `target/` is restored
//! before and saved after, so compilation is incremental across runs. They share one
//! snapshot key (toolchain, lockfile, triple, profile). `CARGO_INCREMENTAL=0` keeps
//! builds reproducible (rustc incremental codegen is not bit-stable), a precondition
//! for the §1.4 correctness-neutral invariant.
//!
//! # Dependency acquisition: pre-vendored, or hash-pinned fetch (§FOD)
//!
//! Two ways the workspace's crates.io dependencies reach the sealed `--offline` build:
//!
//! * **Pre-vendored** — a committed `vendor/` + `.cargo/config.toml` (from `cargo
//!   vendor`); `source_tree` materializes them and cargo reads them offline. No fetch.
//! * **Fetch mode** — no committed `vendor/`, but a `Cargo.lock` lists crates.io deps.
//!   Each crate becomes a **fixed-output fetch** (`CachePolicy::FixedOutput`) pinned to
//!   its lockfile checksum, and the compiling actions assemble a vendor tree from the
//!   fetched `.crate` blobs in-sandbox before building. The build is keyed on the
//!   workspace sources + the `.crate` digests (= the lockfile checksums), **not** on
//!   thousands of individual vendored files — so a committed `vendor/` isn't required and
//!   the per-build input-handling cost is O(workspace sources), not O(vendored files).
//!
//! Deferred: `RUSTFLAGS`/sanitizer/coverage axes (§13.6), binary/bin-unit test targets,
//! integration multi-binary split, separately-addressable test targets; in fetch mode,
//! git/`path`-registry deps and non-crates.io registries (vendor those), and a generated
//! lockfile (needs the staged-graph pass).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anneal_core::{
    AxisValues, Coverage, DebugInfo, Digest, ExecMode, Lto, OptLevel, Sanitizer, ALL_AXES,
};
use anneal_exec::{Action, ActionBuilder, Toolchain};

use crate::context::RuleContext;
use crate::diagnostics;
use crate::providers::{Artifact, ArtifactSource, ProviderSet};
use crate::rule::{Analysis, Rule, RuleError};
use crate::schema::{AttrSchema, AttrType};
use crate::state::{Attestation, Concurrency, PersistentStateDecl, StateActionExt, StateKind};
use crate::toolchain::{nix_base_runtime, nix_store_toolchain, toolchain_path_env};

/// Directories never treated as build inputs.
const IGNORED_DIRS: &[&str] = &["target", ".git", ".anneal"];

/// In fetch mode we also ignore a committed `.cargo/` — the rule generates a fresh
/// `.cargo/config.toml` (the vendored-source replacement) in the sandbox, and a
/// materialized read-only copy would block that write.
const FETCH_IGNORED_DIRS: &[&str] = &["target", ".git", ".anneal", ".cargo"];

/// The crates.io registry source string as it appears in `Cargo.lock`.
const CRATES_IO_SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";

/// `data` consumes other targets' file outputs — a `nickel_eval`'s generated JSON, or
/// a `filegroup`'s plain source files — materializing them into the workspace tree as
/// package-local inputs the Rust code reads at build time (`include_str!`, `build.rs`,
/// …; §14.1, §14.6). This is the inner-tool-only case: Cargo/rustc read the content at
/// execution; Anneal never introspects it at analysis. (A generated `Cargo.toml`, which
/// the rule *would* parse at analysis, is the §14.6 staged-pass case, not an edge.)
const SCHEMA: &[AttrSchema] = &[AttrSchema::optional("data", AttrType::LabelList)];

pub struct CargoWorkspace;

impl Rule for CargoWorkspace {
    fn kind(&self) -> &'static str {
        "cargo_workspace"
    }

    fn schema(&self) -> &'static [AttrSchema] {
        SCHEMA
    }

    fn analyze(&self, ctx: &RuleContext) -> Result<Analysis, RuleError> {
        let toolchain = diagnostics::time("cargo_workspace.toolchain.rust", rust_toolchain)?;
        let runtime = diagnostics::time("cargo_workspace.toolchain.runtime", nix_base_runtime)?;

        // Fetch mode (§FOD): no committed vendor/, but a lockfile with crates.io deps →
        // hash-pin-fetch each crate and assemble a vendor tree in-sandbox. Decided first
        // because it changes which directories count as build inputs.
        let fetch_deps = diagnostics::time("cargo_workspace.fetch_plan", || fetch_plan(ctx))?;
        let ignored = if fetch_deps.is_some() {
            FETCH_IGNORED_DIRS
        } else {
            IGNORED_DIRS
        };
        let sources = diagnostics::time("cargo_workspace.source_tree", || {
            ctx.source_tree(Path::new("."), ignored)
        })?;
        if sources.is_empty() {
            return Err(RuleError::Message(
                "cargo_workspace: no source files found in the package".to_owned(),
            ));
        }
        // The in-sandbox vendor-assembly prelude, prepended to every compiling action's
        // shell command in fetch mode (`None` when pre-vendored / dependency-free).
        let prelude = fetch_deps.as_ref().map(|d| assembly_prelude(d));
        let no_deps: Vec<LockDep> = Vec::new();
        let crate_deps: &[LockDep] = fetch_deps.as_deref().unwrap_or(&no_deps);

        let opt_level = ctx.config().axes().opt_level;
        let (profile_dir, release) = match opt_level {
            OptLevel::Debug => ("debug", false),
            OptLevel::Release | OptLevel::ReleaseWithDebugInfo => ("release", true),
        };
        let release_flag = if release { " --release" } else { "" };

        let path_env = diagnostics::time("cargo_workspace.path_env", || {
            toolchain_path_env(&[&toolchain, &runtime])
        });

        // cargo_workspace interprets all five axes (§13.6), so it consumes all five —
        // each enters the cache key, and the snapshot key, at its current value.
        let rustflags = rustflags_for(ctx.config().axes());
        let consumed = ALL_AXES.to_vec();

        let crates =
            diagnostics::time("cargo_workspace.workspace_crates", || workspace_crates(ctx))?;
        // The typed form of the old coarse snapshot key (DESIGN.md §2.1 /
        // Appendix A ruling 4): `target/` is **interleaved** state — mutated by
        // the very actions that read it — so declaring it carries the
        // attestation, and the epoch folds into the state key so a discovered
        // cargo-soundness bug revokes every warm tree derived under it.
        // Declared only under `ExecMode::Incremental` (§4.4): the Hermetic arm
        // of this rule emits the same actions with no state grant — cold,
        // deterministic, promotable under full enforcement. That asymmetry is
        // the §2.4 dev-loop/shared-cache reconciliation, per-rule-interpreted.
        let incremental = ctx.config().axes().exec_mode == ExecMode::Incremental;
        let target_state = if !incremental {
            None
        } else {
            Some(ctx.declare_state(PersistentStateDecl {
                namespace: "cargo-target",
                shard: diagnostics::time("cargo_workspace.state_shard", || {
                    target_state_shard(&toolchain, &runtime, &sources, ctx)
                }),
                kind: StateKind::Interleaved {
                    concurrency: Concurrency::Exclusive,
                    attestation: Attestation {
                        epoch: 1,
                        rationale: "cargo fingerprint reuse is sound under --locked, a \
                                pinned toolchain, CARGO_INCREMENTAL=0, and warm-reuse \
                                content sync; epoch bumped on cargo soundness-class \
                                advisories (cf. the 1.52.0 incremental emergency)",
                    },
                },
                paths: vec![PathBuf::from("target")],
            })?)
        };
        let label = ctx.label().clone();

        // Inputs from `data` deps (§14.6, inner-tool-only): every file a dependency
        // provides — generated output or plain source — is materialized into the build
        // tree at its path, as a content-addressed input. Added to every *compiling*
        // action (the test-run action only executes a binary, so it needs nothing here).
        let data: Vec<Artifact> = diagnostics::time("cargo_workspace.data_deps", || {
            ctx.deps()
                .iter()
                .filter_map(|dep| dep.providers.files.as_ref())
                .flat_map(|file_set| file_set.files.iter().cloned())
                .collect()
        });

        let mut actions = Vec::new();

        // --- fixed-output fetch actions (fetch mode): one per crates.io dependency,
        // pinned to its lockfile checksum (§FOD). The compiling actions depend on these. ---
        diagnostics::time(
            "cargo_workspace.fetch_actions",
            || -> Result<(), RuleError> {
                if let Some(deps) = &fetch_deps {
                    for dep in deps {
                        actions.push(fetch_action(dep, &runtime)?);
                    }
                }
                Ok(())
            },
        )?;

        // --- coarse build action ---
        diagnostics::time(
            "cargo_workspace.build_action",
            || -> Result<(), RuleError> {
                let build_cmd = cargo_command(
                    prelude.as_deref(),
                    cargo_args("build", None, None, release_flag),
                );
                let mut build = with_crates(
                    with_data(
                        with_sources(
                            cargo_builder(
                                format!("cargo_workspace build {label}"),
                                build_cmd,
                                &path_env,
                                &rustflags,
                                &toolchain,
                                &runtime,
                            ),
                            &sources,
                        ),
                        &data,
                    ),
                    crate_deps,
                );
                for c in crates.iter().filter(|c| c.is_normal_lib()) {
                    let lib = format!("lib{}.rlib", c.name.replace('-', "_"));
                    let out = PathBuf::from(format!("target/{profile_dir}/{lib}"));
                    build = build.output(out.to_string_lossy().into_owned(), out);
                }
                actions.push(
                    build
                        .configured(ctx.config().clone(), consumed.clone())
                        .mutate_state_opt(target_state.as_ref())?
                        .try_build()?,
                );
                Ok(())
            },
        )?;

        // --- per-(crate, test_type) actions ---
        diagnostics::time(
            "cargo_workspace.test_actions",
            || -> Result<(), RuleError> {
                for c in &crates {
                    if c.is_normal_lib() {
                        // unit: compile/run split.
                        let compile_id =
                            format!("cargo_workspace test-compile {label} {} unit", c.name);
                        let run_id = format!("cargo_workspace test-run {label} {} unit", c.name);

                        let test_bin_path =
                            PathBuf::from(format!("target/anneal-tests/{}/test-bin", c.name));
                        let compile = with_crates(
                            with_data(
                                with_sources(
                                    cargo_builder(
                                        compile_id.clone(),
                                        shell_cmd(
                                            prelude.as_deref(),
                                            &unit_compile_body(
                                                &c.name,
                                                release_flag,
                                                &test_bin_path,
                                            ),
                                        ),
                                        &path_env,
                                        &rustflags,
                                        &toolchain,
                                        &runtime,
                                    ),
                                    &sources,
                                ),
                                &data,
                            ),
                            crate_deps,
                        )
                        .output("test-bin", test_bin_path)
                        .configured(ctx.config().clone(), consumed.clone())
                        .mutate_state_opt(target_state.as_ref())?;
                        actions.push(compile.try_build()?);

                        // run depends on the compiled binary (an action-graph edge); its cache
                        // key is the binary's content, so it hits when the binary is unchanged.
                        // It captures the framework output to `results.txt` and always exits 0
                        // so a *test failure* is a recorded result (parsed into structured
                        // form by anneal-test), not a lost action error.
                        // `test-bin` is a declared input and Linux sealed mode mounts inputs
                        // read-only, so copy it before restoring the executable bit.
                        let results_path =
                            PathBuf::from(format!("target/anneal-tests/{}/results.txt", c.name));
                        let run_script = format!(
                            "cp test-bin test-bin.run\n\
                     chmod u+x test-bin.run\n\
                     ./test-bin.run > {} 2>&1; code=$?\n\
                     printf 'ANNEAL_TEST_EXIT=%s\\n' \"$code\" >> {}\n",
                            results_path.display(),
                            results_path.display()
                        );
                        let run = Action::builder(
                            run_id,
                            vec!["sh".to_owned(), "-c".to_owned(), run_script],
                        )
                        .input_from_output("test-bin", "test-bin", compile_id, "test-bin")
                        .output("results.txt", results_path)
                        .toolchain(toolchain.clone())
                        .toolchain(runtime.clone())
                        .env("PATH", &path_env)
                        .configured(ctx.config().clone(), Vec::new());
                        actions.push(run.try_build()?);

                        // doc: single action (no reusable binary, §12.3).
                        let doc = with_crates(
                            with_data(
                                with_sources(
                                    cargo_builder(
                                        format!("cargo_workspace test {label} {} doc", c.name),
                                        cargo_command(
                                            prelude.as_deref(),
                                            cargo_args(
                                                "test",
                                                Some(&c.name),
                                                Some("--doc"),
                                                release_flag,
                                            ),
                                        ),
                                        &path_env,
                                        &rustflags,
                                        &toolchain,
                                        &runtime,
                                    ),
                                    &sources,
                                ),
                                &data,
                            ),
                            crate_deps,
                        )
                        .configured(ctx.config().clone(), consumed.clone())
                        .mutate_state_opt(target_state.as_ref())?;
                        actions.push(doc.try_build()?);
                    }

                    if c.has_tests {
                        // integration: single action for now (multi-binary split deferred).
                        let integ = with_crates(
                            with_data(
                                with_sources(
                                    cargo_builder(
                                        format!(
                                            "cargo_workspace test {label} {} integration",
                                            c.name
                                        ),
                                        cargo_command(
                                            prelude.as_deref(),
                                            cargo_args(
                                                "test",
                                                Some(&c.name),
                                                Some("--tests"),
                                                release_flag,
                                            ),
                                        ),
                                        &path_env,
                                        &rustflags,
                                        &toolchain,
                                        &runtime,
                                    ),
                                    &sources,
                                ),
                                &data,
                            ),
                            crate_deps,
                        )
                        .configured(ctx.config().clone(), consumed.clone())
                        .mutate_state_opt(target_state.as_ref())?;
                        actions.push(integ.try_build()?);
                    }
                }
                Ok(())
            },
        )?;

        Ok(Analysis {
            actions,
            providers: ProviderSet::default(),
        })
    }
}

fn rust_toolchain() -> Result<Toolchain, RuleError> {
    #[cfg(target_os = "macos")]
    {
        nix_store_toolchain("rust", &["cargo", "rustc", "cc", "xcrun"])
    }

    #[cfg(not(target_os = "macos"))]
    {
        nix_store_toolchain("rust", &["cargo", "rustc", "cc"])
    }
}

/// A workspace member crate discovered by introspecting `Cargo.toml`s.
struct CrateInfo {
    name: String,
    /// `src/lib.rs` exists. NOTE: also true for proc-macro crates — distinguish with
    /// [`CrateInfo::is_normal_lib`].
    has_lib: bool,
    /// `[lib] proc-macro = true` — produces a dylib, **not** an rlib.
    is_proc_macro: bool,
    has_tests: bool,
}

impl CrateInfo {
    /// A normal (rlib-producing) library member: has a lib and isn't a proc-macro. Only
    /// these get a declared `lib<name>.rlib` output and lib/doc test actions; a proc-macro
    /// member produces a dylib, so declaring its rlib would be a spurious `MissingOutput`.
    fn is_normal_lib(&self) -> bool {
        self.has_lib && !self.is_proc_macro
    }
}

/// Build a base `cargo` action builder with the shared environment (toolchain on
/// PATH, deterministic settings, and the axis-derived `RUSTFLAGS`). `command` is the
/// full argv.
fn cargo_builder(
    name: String,
    command: Vec<String>,
    path_env: &str,
    rustflags: &str,
    toolchain: &Toolchain,
    runtime: &Toolchain,
) -> ActionBuilder {
    let mut builder = Action::builder(name, command)
        .toolchain(toolchain.clone())
        .toolchain(runtime.clone())
        .env("PATH", path_env)
        .env("CARGO_TERM_COLOR", "never")
        .env("CARGO_INCREMENTAL", "0");
    // Only set RUSTFLAGS when an axis actually changes a flag, so a default-config
    // build is byte-for-byte what plain `cargo` produces.
    if !rustflags.is_empty() {
        builder = builder.env("RUSTFLAGS", rustflags);
    }
    builder
}

/// Translate the `lto`, `debug_info`, `sanitizer`, and `coverage` axes into a
/// `RUSTFLAGS` string (§13.6). `opt_level` is handled separately as the Cargo
/// profile. Each axis emits a flag only for non-default values, so the default
/// configuration yields an empty string (no override of the profile's own choices).
///
/// `sanitizer` maps to `-Z sanitizer=…`, which requires a nightly toolchain; on
/// stable a sanitized build will fail at compile time. The mapping is still applied
/// so the configuration is honest and enters the cache key.
fn rustflags_for(axes: &AxisValues) -> String {
    let mut flags: Vec<String> = Vec::new();

    match axes.lto {
        Lto::Off => {}
        Lto::Thin => flags.push("-Clto=thin".to_owned()),
        Lto::Full => flags.push("-Clto=fat".to_owned()),
    }
    match axes.debug_info {
        DebugInfo::Full => {} // profile default
        DebugInfo::LineTablesOnly => flags.push("-Cdebuginfo=1".to_owned()),
        DebugInfo::None => flags.push("-Cdebuginfo=0".to_owned()),
    }
    match axes.sanitizer {
        Sanitizer::None => {}
        Sanitizer::Address => flags.push("-Zsanitizer=address".to_owned()),
        Sanitizer::Thread => flags.push("-Zsanitizer=thread".to_owned()),
        Sanitizer::Memory => flags.push("-Zsanitizer=memory".to_owned()),
        Sanitizer::Undefined => flags.push("-Zsanitizer=undefined".to_owned()),
    }
    if axes.coverage == Coverage::On {
        flags.push("-Cinstrument-coverage".to_owned());
    }

    flags.join(" ")
}

/// Add every source file as a content-addressed input.
fn with_sources(mut builder: ActionBuilder, sources: &[Artifact]) -> ActionBuilder {
    for artifact in sources {
        if let ArtifactSource::Source(digest) = &artifact.source {
            let name = artifact.path.to_string_lossy().into_owned();
            builder = builder.input(name, artifact.path.clone(), *digest);
        }
    }
    builder
}

/// Add `data` dependency artifacts as inputs, materialized at their declared paths in
/// the build tree. A resolved source flows in as a blob; a produced output flows in as
/// an action-graph edge resolved at execution.
fn with_data(mut builder: ActionBuilder, data: &[Artifact]) -> ActionBuilder {
    for artifact in data {
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

/// `cargo <sub> [--package <pkg>] [<type_flag>] --offline --locked [--release] [--workspace]`.
fn cargo_args(
    sub: &str,
    pkg: Option<&str>,
    type_flag: Option<&str>,
    release_flag: &str,
) -> Vec<String> {
    let mut s = format!("cargo {sub} --offline --locked");
    match pkg {
        Some(p) => s.push_str(&format!(" --package {p}")),
        None => s.push_str(" --workspace"),
    }
    if let Some(flag) = type_flag {
        s.push(' ');
        s.push_str(flag);
    }
    s.push_str(release_flag);
    // These are simple, space-free tokens.
    s.split(' ').map(str::to_owned).collect()
}

/// The shell **body** for a unit-test **compile** action: compile the test binary
/// without running it, then copy it to the stable declared output path. The `set -eu`
/// and any fetch-mode vendor prelude are added by [`shell_cmd`].
fn unit_compile_body(pkg: &str, release_flag: &str, output_path: &Path) -> String {
    format!(
        "cargo test --package {pkg} --lib --no-run --offline --locked{release_flag} --message-format=json > artifacts.json\n\
         bin=$(grep -o '\"executable\":\"[^\"]*\"' artifacts.json | head -1 | sed 's/^\"executable\":\"//; s/\"$//')\n\
         test -n \"$bin\"\n\
         cp \"$bin\" {}\n",
        output_path.display()
    )
}

/// Wrap a shell `body` as a sandbox command: `set -eu`, then the fetch-mode vendor
/// `prelude` (if any), then the body. Used by every compiling action.
fn shell_cmd(prelude: Option<&str>, body: &str) -> Vec<String> {
    let mut script = String::from("set -eu\n");
    if let Some(p) = prelude {
        script.push_str(p);
    }
    script.push_str(body);
    vec!["sh".to_owned(), "-c".to_owned(), script]
}

/// Choose a compiling action's command: in pre-vendored / dependency-free builds, run
/// the `cargo` argv directly (unchanged); in fetch mode, wrap it in a shell that first
/// runs the vendor-assembly `prelude`.
fn cargo_command(prelude: Option<&str>, argv: Vec<String>) -> Vec<String> {
    match prelude {
        None => argv,
        // argv tokens are simple and space-free, so re-joining to a shell line is safe.
        Some(p) => shell_cmd(Some(p), &argv.join(" ")),
    }
}

/// One crates.io dependency pinned by `Cargo.lock`: its name, version, and the SHA-256
/// of its `.crate` tarball (the lockfile `checksum`, which is also the fixed-output pin).
struct LockDep {
    name: String,
    version: String,
    checksum: String,
}

impl LockDep {
    /// The `<name>-<version>` stem shared by the registry path, the `.crate` filename,
    /// and the extracted vendor directory.
    fn base(&self) -> String {
        format!("{}-{}", self.name, self.version)
    }
}

/// Decide whether to fetch dependencies (§FOD), and which. `Some(deps)` only when there
/// is **no** committed `vendor/` **and** `Cargo.lock` lists crates.io dependencies; then
/// each is hash-pinned-fetched and a vendor tree is assembled in-sandbox. `None` keeps
/// the pre-vendored / dependency-free path untouched.
///
/// Errors on a dependency this increment can't fetch (git, `path`+registry, a non-
/// crates.io registry, or a v1/v2 lockfile lacking inline checksums) — vendor those.
fn fetch_plan(ctx: &RuleContext) -> Result<Option<Vec<LockDep>>, RuleError> {
    if ctx.package_file_exists(Path::new("vendor"))
        || !ctx.package_file_exists(Path::new("Cargo.lock"))
    {
        return Ok(None);
    }
    let lock: toml::Value = ctx
        .read_package_file(Path::new("Cargo.lock"))?
        .parse()
        .map_err(|e| RuleError::Message(format!("parsing Cargo.lock: {e}")))?;
    let packages = match lock.get("package").and_then(|p| p.as_array()) {
        Some(p) => p,
        None => return Ok(None),
    };

    let mut deps = Vec::new();
    for pkg in packages {
        // No `source` ⇒ a local workspace member or path dependency: nothing to fetch.
        let source = match pkg.get("source").and_then(|s| s.as_str()) {
            Some(s) => s,
            None => continue,
        };
        let name = pkg.get("name").and_then(|n| n.as_str()).unwrap_or_default();
        let version = pkg
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if source != CRATES_IO_SOURCE {
            return Err(RuleError::Message(format!(
                "cargo_workspace fetch mode: unsupported dependency source {source:?} for \
                 {name} {version}; commit a vendor/ directory for this workspace instead"
            )));
        }
        let checksum = pkg
            .get("checksum")
            .and_then(|c| c.as_str())
            .ok_or_else(|| {
                RuleError::Message(format!(
                    "cargo_workspace fetch mode: {name} {version} has no inline checksum in \
                 Cargo.lock (regenerate with a current cargo, or vendor the workspace)"
                ))
            })?;
        deps.push(LockDep {
            name: name.to_owned(),
            version: version.to_owned(),
            checksum: checksum.to_owned(),
        });
    }
    Ok((!deps.is_empty()).then_some(deps))
}

/// A fixed-output fetch (§FOD) for one crate: download its `.crate` from static.crates.io
/// into the single declared output `crate`, pinned to the lockfile checksum. Cached by
/// output (a present blob skips the download), verified against the pin (a mismatch fails
/// closed). The graph-unique name lets the compiling actions reference it.
fn fetch_action(dep: &LockDep, runtime: &Toolchain) -> Result<Action, RuleError> {
    let expected = Digest::from_hex(&dep.checksum).map_err(|e| {
        RuleError::Message(format!(
            "{} {}: invalid checksum hex in Cargo.lock: {e}",
            dep.name, dep.version
        ))
    })?;
    let base = dep.base();
    let url = format!("https://static.crates.io/crates/{}/{base}.crate", dep.name);
    let output_path = PathBuf::from(format!(".anneal/fetch/{base}.crate"));
    let script = format!(
        "curl -sSL --fail --retry 3 -o {} '{url}'",
        output_path.display()
    );
    let path_env = toolchain_path_env(&[runtime]);
    Ok(Action::builder(
        format!("cargo_workspace fetch {base}"),
        vec!["sh".to_owned(), "-c".to_owned(), script],
    )
    .toolchain(runtime.clone())
    .env("PATH", path_env)
    .output("crate", output_path)
    .platform_independent()
    .fixed_output(expected)
    .try_build()?)
}

/// Add each fetched `.crate` as an input at `.anneal-crates/<base>.crate` (an action-graph
/// edge to its fetch action). Empty `deps` ⇒ a no-op (pre-vendored / dependency-free).
fn with_crates(mut builder: ActionBuilder, deps: &[LockDep]) -> ActionBuilder {
    for dep in deps {
        let base = dep.base();
        builder = builder.input_from_output(
            format!("crate:{base}"),
            format!(".anneal-crates/{base}.crate"),
            format!("cargo_workspace fetch {base}"),
            "crate",
        );
    }
    builder
}

/// The fetch-mode vendor-assembly prelude: extract each fetched `.crate` into a vendor
/// tree with its pinned-checksum `.cargo-checksum.json`, then write a `.cargo/config.toml`
/// that redirects crates.io to that vendor directory — so `cargo build --offline` reads
/// the deps with no network and no committed `vendor/`. Pure `tar`/`printf`/`cat` (on the
/// sandbox PATH), with the checksum baked in (no `sha256sum`, which isn't on macOS).
///
/// Two deliberate divergences from `cargo vendor`'s own output, both validated on a real
/// transitive tree (`anneal-analysis/tests/cargo_fetch.rs`):
///   * **Directory names.** We name *every* dir `<name>-<version>` (the `.crate` tarball's
///     top-level dir). `cargo vendor` uses a bare `<name>` for single-version crates and
///     only suffixes to disambiguate multiple versions — but cargo's `directory` source
///     reads each crate's identity from its inner `Cargo.toml`/`.cargo-checksum.json`, not
///     the folder name, so the always-suffixed form is accepted (and avoids the bare-vs-
///     suffixed special case).
///   * **Empty `files` map.** `cargo vendor` writes a full per-file checksum map; we write
///     `{"files":{},"package":"<sha>"}`. cargo verifies the `package` checksum against the
///     lockfile and treats the per-file map as optional local-modification tracking, so an
///     empty map builds. (If a future cargo / `--frozen` rejects it, bake the per-file
///     checksums in instead.)
fn assembly_prelude(deps: &[LockDep]) -> String {
    let mut s = String::from("mkdir -p vendor .cargo\n");
    for dep in deps {
        let base = dep.base();
        s.push_str(&format!("tar xzf .anneal-crates/{base}.crate -C vendor\n"));
        s.push_str(&format!(
            "printf '%s' '{{\"files\":{{}},\"package\":\"{}\"}}' > vendor/{base}/.cargo-checksum.json\n",
            dep.checksum
        ));
    }
    s.push_str(
        "cat > .cargo/config.toml <<'CARGOCFG'\n\
         [source.crates-io]\n\
         replace-with = \"vendored-sources\"\n\
         [source.vendored-sources]\n\
         directory = \"vendor\"\n\
         CARGOCFG\n",
    );
    s
}

/// The `cargo-target` state shard: `(toolchain, runtime, lockfile, target_triple,
/// all axis values)` (§8.2). Including the axis values gives each configuration its
/// own `target/` tree, so a debug, a release, and a coverage build don't thrash one
/// another's. Formerly the hand-rolled snapshot-key digest; the typed state layer
/// derives the key from this shard plus rule scope, kind, and attestation epoch.
fn target_state_shard(
    toolchain: &Toolchain,
    runtime: &Toolchain,
    sources: &[Artifact],
    ctx: &RuleContext,
) -> Vec<String> {
    let mut shard = vec![
        toolchain.identity().to_owned(),
        runtime.identity().to_owned(),
    ];
    shard.push(
        match sources
            .iter()
            .find(|a| a.path == Path::new("Cargo.lock"))
            .map(|a| &a.source)
        {
            Some(ArtifactSource::Source(lock)) => lock.to_hex(),
            _ => "no-lockfile".to_owned(),
        },
    );
    shard.push(ctx.config().platform().target_triple().to_owned());
    let all_axes = BTreeSet::from(ALL_AXES);
    for (name, value) in ctx.config().axes().consumed(&all_axes) {
        shard.push(format!("{name}={value}"));
    }
    shard
}

/// Enumerate workspace member crates, noting whether each has a library target and an
/// integration `tests/` directory.
///
/// This is the rule's **workspace-structure** view (members + each member's relevant
/// target kind). It is *hand-parsed* from the manifests today — pure, in-process,
/// non-stale (it reads the live `Cargo.toml`s). It intentionally re-derives a *slice* of
/// `cargo metadata` (members + lib/proc-macro kind), not cargo's full resolution, so it
/// can drift on the long tail (nested workspaces, `default-members`, multi-level globs).
/// The clean upgrade is to let cargo report this authoritatively via the staged-graph /
/// `cargo metadata` path (see TODO); a rule only consumes the returned structure, so the
/// implementation can swap underneath without touching `analyze`.
fn workspace_crates(ctx: &RuleContext) -> Result<Vec<CrateInfo>, RuleError> {
    let root: toml::Value = ctx
        .read_package_file(Path::new("Cargo.toml"))?
        .parse()
        .map_err(|e| RuleError::Message(format!("parsing Cargo.toml: {e}")))?;

    let workspace = root.get("workspace");
    let exclude: Vec<PathBuf> = workspace
        .and_then(|w| w.get("exclude"))
        .and_then(|e| e.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default();

    let mut dirs: Vec<PathBuf> = vec![PathBuf::new()]; // the root package, if any
    if let Some(members) = workspace
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
    {
        for member in members.iter().filter_map(|m| m.as_str()) {
            // Expand a trailing single-level glob (`crates/*`, or a bare `*`) by listing
            // the immediate subdirectories that contain a `Cargo.toml`. Multi-level globs
            // (`**`, `a/*/b`) are not yet supported and are skipped.
            let glob_prefix = if member == "*" {
                Some("")
            } else {
                member.strip_suffix("/*")
            };
            match glob_prefix {
                Some(prefix) => {
                    for entry in ctx.list_dir(Path::new(prefix))? {
                        if ctx.package_file_exists(&entry.join("Cargo.toml")) {
                            dirs.push(entry);
                        }
                    }
                }
                None if member.contains('*') => continue, // unsupported glob shape
                None => dirs.push(PathBuf::from(member)),
            }
        }
    }
    dirs.retain(|d| !exclude.contains(d));

    let mut crates = Vec::new();
    for dir in dirs {
        let manifest = dir.join("Cargo.toml");
        if !ctx.package_file_exists(&manifest) {
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
            let is_proc_macro = parsed
                .get("lib")
                .and_then(|l| l.get("proc-macro"))
                .and_then(toml::Value::as_bool)
                .unwrap_or(false);
            crates.push(CrateInfo {
                name: name.to_owned(),
                has_lib: ctx.package_file_exists(&dir.join("src/lib.rs")),
                is_proc_macro,
                has_tests: ctx.package_file_exists(&dir.join("tests")),
            });
        }
    }
    Ok(crates)
}

#[cfg(test)]
mod tests {
    use super::rustflags_for;
    use anneal_core::{AxisValues, Coverage, DebugInfo, Lto, Sanitizer};

    #[test]
    fn default_config_emits_no_rustflags() {
        assert_eq!(rustflags_for(&AxisValues::default()), "");
    }

    #[test]
    fn lto_debuginfo_and_coverage_map_to_codegen_flags() {
        let axes = AxisValues {
            lto: Lto::Thin,
            debug_info: DebugInfo::None,
            coverage: Coverage::On,
            ..Default::default()
        };
        assert_eq!(
            rustflags_for(&axes),
            "-Clto=thin -Cdebuginfo=0 -Cinstrument-coverage"
        );
    }

    #[test]
    fn line_tables_and_full_lto() {
        let axes = AxisValues {
            lto: Lto::Full,
            debug_info: DebugInfo::LineTablesOnly,
            ..Default::default()
        };
        assert_eq!(rustflags_for(&axes), "-Clto=fat -Cdebuginfo=1");
    }

    #[test]
    fn sanitizer_maps_to_unstable_flag() {
        let axes = AxisValues {
            sanitizer: Sanitizer::Address,
            ..Default::default()
        };
        assert_eq!(rustflags_for(&axes), "-Zsanitizer=address");
    }
}
