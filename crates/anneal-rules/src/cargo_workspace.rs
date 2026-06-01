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
//!     copies it to a stable `test-bin` output. The run action consumes that binary
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
//! Deferred: `RUSTFLAGS`/sanitizer/coverage axes (§13.6), binary/bin-unit test
//! targets, integration multi-binary split, separately-addressable test targets
//! (a query/CLI concern), and dependency vendoring.

use std::path::{Path, PathBuf};

use anneal_core::{AxisValues, Coverage, DebugInfo, Digest, Lto, OptLevel, Sanitizer, ALL_AXES};
use anneal_exec::{Action, ActionBuilder};

use crate::context::RuleContext;
use crate::providers::{Artifact, ArtifactSource, ProviderSet};
use crate::rule::{Analysis, Rule, RuleError};
use crate::schema::{AttrSchema, AttrType};

/// Directories never treated as build inputs.
const IGNORED_DIRS: &[&str] = &["target", ".git", ".mybuild"];

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
        let toolchain_dirs = toolchain_bin_dirs()?;
        let sources = ctx.source_tree(Path::new("."), IGNORED_DIRS)?;
        if sources.is_empty() {
            return Err(RuleError::Message(
                "cargo_workspace: no source files found in the package".to_owned(),
            ));
        }

        let opt_level = ctx.config().axes().opt_level;
        let (profile_dir, release) = match opt_level {
            OptLevel::Debug => ("debug", false),
            OptLevel::Release | OptLevel::ReleaseWithDebugInfo => ("release", true),
        };
        let release_flag = if release { " --release" } else { "" };

        let mut path: Vec<String> = toolchain_dirs
            .iter()
            .map(|d| d.to_string_lossy().into_owned())
            .collect();
        path.push("/usr/bin".to_owned());
        path.push("/bin".to_owned());
        let path_env = path.join(":");

        // cargo_workspace interprets all five axes (§13.6), so it consumes all five —
        // each enters the cache key, and the snapshot key, at its current value.
        let rustflags = rustflags_for(ctx.config().axes());
        let consumed = ALL_AXES.to_vec();

        let crates = workspace_crates(ctx)?;
        let snapshot_key = snapshot_key(&toolchain_dirs, &sources, ctx);
        let snapshot_paths = vec![PathBuf::from("target")];
        let label = ctx.label().clone();

        // Inputs from `data` deps (§14.6, inner-tool-only): every file a dependency
        // provides — generated output or plain source — is materialized into the build
        // tree at its path, as a content-addressed input. Added to every *compiling*
        // action (the test-run action only executes a binary, so it needs nothing here).
        let data: Vec<Artifact> = ctx
            .deps()
            .iter()
            .filter_map(|dep| dep.providers.files.as_ref())
            .flat_map(|file_set| file_set.files.iter().cloned())
            .collect();

        let mut actions = Vec::new();

        // --- coarse build action ---
        let build_cmd = cargo_args("build", None, None, release_flag);
        let mut build = with_data(
            with_sources(
                cargo_builder(format!("cargo_workspace build {label}"), build_cmd, &path_env, &rustflags),
                &sources,
            ),
            &data,
        );
        for c in crates.iter().filter(|c| c.has_lib) {
            let lib = format!("lib{}.rlib", c.name.replace('-', "_"));
            let out = PathBuf::from(format!("target/{profile_dir}/{lib}"));
            build = build.output(out.to_string_lossy().into_owned(), out);
        }
        actions.push(
            build
                .configured(ctx.config().clone(), consumed.clone())
                .snapshot(snapshot_key, snapshot_paths.clone())
                .build(),
        );

        // --- per-(crate, test_type) actions ---
        for c in &crates {
            if c.has_lib {
                // unit: compile/run split.
                let compile_id = format!("cargo_workspace test-compile {label} {} unit", c.name);
                let run_id = format!("cargo_workspace test-run {label} {} unit", c.name);

                let compile = with_data(
                    with_sources(
                        cargo_builder(
                            compile_id.clone(),
                            unit_compile_script(&c.name, release_flag),
                            &path_env,
                            &rustflags,
                        ),
                        &sources,
                    ),
                    &data,
                )
                .output("test-bin", "test-bin")
                .configured(ctx.config().clone(), consumed.clone())
                .snapshot(snapshot_key, snapshot_paths.clone());
                actions.push(compile.build());

                // run depends on the compiled binary (an action-graph edge); its cache
                // key is the binary's content, so it hits when the binary is unchanged.
                // It captures the framework output to `results.txt` and always exits 0
                // so a *test failure* is a recorded result (parsed into structured
                // form by anneal-test), not a lost action error.
                let run_script = "chmod u+x test-bin\n\
                     ./test-bin > results.txt 2>&1; code=$?\n\
                     printf 'ANNEAL_TEST_EXIT=%s\\n' \"$code\" >> results.txt\n";
                let run = Action::builder(
                    run_id,
                    vec!["/bin/sh".to_owned(), "-c".to_owned(), run_script.to_owned()],
                )
                .input_from_output("test-bin", "test-bin", compile_id, "test-bin")
                .output("results.txt", "results.txt")
                .env("PATH", "/usr/bin:/bin")
                .configured(ctx.config().clone(), Vec::new());
                actions.push(run.build());

                // doc: single action (no reusable binary, §12.3).
                let doc = with_data(
                    with_sources(
                        cargo_builder(
                            format!("cargo_workspace test {label} {} doc", c.name),
                            cargo_args("test", Some(&c.name), Some("--doc"), release_flag),
                            &path_env,
                            &rustflags,
                        ),
                        &sources,
                    ),
                    &data,
                )
                .configured(ctx.config().clone(), consumed.clone())
                .snapshot(snapshot_key, snapshot_paths.clone());
                actions.push(doc.build());
            }

            if c.has_tests {
                // integration: single action for now (multi-binary split deferred).
                let integ = with_data(
                    with_sources(
                        cargo_builder(
                            format!("cargo_workspace test {label} {} integration", c.name),
                            cargo_args("test", Some(&c.name), Some("--tests"), release_flag),
                            &path_env,
                            &rustflags,
                        ),
                        &sources,
                    ),
                    &data,
                )
                .configured(ctx.config().clone(), consumed.clone())
                .snapshot(snapshot_key, snapshot_paths.clone());
                actions.push(integ.build());
            }
        }

        Ok(Analysis {
            actions,
            providers: ProviderSet::default(),
        })
    }
}

/// A workspace member crate discovered by introspecting `Cargo.toml`s.
struct CrateInfo {
    name: String,
    has_lib: bool,
    has_tests: bool,
}

/// Build a base `cargo` action builder with the shared environment (toolchain on
/// PATH, deterministic settings, and the axis-derived `RUSTFLAGS`). `command` is the
/// full argv.
fn cargo_builder(name: String, command: Vec<String>, path_env: &str, rustflags: &str) -> ActionBuilder {
    let mut builder = Action::builder(name, command)
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
            ArtifactSource::Output { action, name: output } => {
                builder = builder.input_from_output(name, artifact.path.clone(), action, output);
            }
        }
    }
    builder
}

/// `cargo <sub> [--package <pkg>] [<type_flag>] --offline --locked [--release] [--workspace]`.
fn cargo_args(sub: &str, pkg: Option<&str>, type_flag: Option<&str>, release_flag: &str) -> Vec<String> {
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

/// The shell script for a unit-test **compile** action: compile the test binary
/// without running it, then copy it to the stable output path `test-bin`.
fn unit_compile_script(pkg: &str, release_flag: &str) -> Vec<String> {
    let script = format!(
        "set -eu\n\
         cargo test --package {pkg} --lib --no-run --offline --locked{release_flag} --message-format=json > artifacts.json\n\
         bin=$(grep -o '\"executable\":\"[^\"]*\"' artifacts.json | head -1 | sed 's/^\"executable\":\"//; s/\"$//')\n\
         test -n \"$bin\"\n\
         cp \"$bin\" test-bin\n"
    );
    vec!["/bin/sh".to_owned(), "-c".to_owned(), script]
}

/// Coarse snapshot key: `(toolchain, lockfile, target_triple, all axis values)`
/// (§8.2). Including the axis values gives each configuration its own `target/`
/// snapshot, so a debug, a release, and a coverage build don't thrash one another's.
fn snapshot_key(toolchain_dirs: &[PathBuf], sources: &[Artifact], ctx: &RuleContext) -> Digest {
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
    for (name, value) in ctx.config().axes().consumed(&ALL_AXES) {
        buf.extend_from_slice(name.as_bytes());
        buf.push(b'=');
        buf.extend_from_slice(value.as_bytes());
        buf.push(b';');
    }
    Digest::of(&buf)
}

/// Enumerate workspace member crates, noting whether each has a library target and an
/// integration `tests/` directory.
fn workspace_crates(ctx: &RuleContext) -> Result<Vec<CrateInfo>, RuleError> {
    let root: toml::Value = ctx
        .read_package_file(Path::new("Cargo.toml"))?
        .parse()
        .map_err(|e| RuleError::Message(format!("parsing Cargo.toml: {e}")))?;

    let mut dirs: Vec<PathBuf> = vec![PathBuf::new()]; // the root package, if any
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
                dirs.push(PathBuf::from(s));
            }
        }
    }

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
            crates.push(CrateInfo {
                name: name.to_owned(),
                has_lib: ctx.package_file_exists(&dir.join("src/lib.rs")),
                has_tests: ctx.package_file_exists(&dir.join("tests")),
            });
        }
    }
    Ok(crates)
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
