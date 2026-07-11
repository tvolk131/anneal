//! Phase 3, increment 1: a `cargo_workspace` builds hermetically through the
//! sandbox and is content-addressed on its sources + toolchain + profile.
//!
//! Uses a dependency-free Cargo workspace so `cargo build --offline` needs no
//! network (Milestone 1 is scoped to public-/no-dependency workflows; vendoring is
//! a later increment).

use anneal_analysis::{ActionGraph, Analyzer};
use anneal_core::{AxisValues, Configuration, OptLevel, Platform};
use anneal_exec::{Action, Executor, LocalExecutor};
use anneal_loader::load_package;
use anneal_rules::builtin_rules;

/// The coarse `build` action of a `cargo_workspace` (now one of several emitted).
fn build_action(graph: &ActionGraph) -> Action {
    graph
        .actions()
        .find(|a| a.name().starts_with("cargo_workspace build"))
        .expect("a build action")
        .clone()
}

fn config(opt: OptLevel) -> Configuration {
    Configuration::new(
        Platform::new("host", "host"),
        AxisValues {
            opt_level: opt,
            ..Default::default()
        },
    )
}

/// Create a dependency-free Cargo workspace under `<tmp>/ws` with a `BUILD` file,
/// and generate its `Cargo.lock` (so `--locked` is satisfied).
fn cargo_fixture() -> tempfile::TempDir {
    cargo_fixture_build("cargo_workspace(name = \"ws\")\n")
}

/// A dependency-free Cargo workspace under `<tmp>/ws` with the given `BUILD` body.
fn cargo_fixture_build(build: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(ws.join("mylib/src")).unwrap();
    std::fs::write(
        ws.join("Cargo.toml"),
        "[workspace]\nmembers = [\"mylib\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("mylib/Cargo.toml"),
        "[package]\nname = \"mylib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("mylib/src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    std::fs::write(ws.join("BUILD"), build).unwrap();

    let status = std::process::Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(&ws)
        .status()
        .expect("cargo must be available to set up the fixture");
    assert!(status.success(), "cargo generate-lockfile failed");
    tmp
}

/// macOS regression for the SDK groundwork: the rust toolchain's declared env
/// (`DEVELOPER_DIR`, the pinned apple-sdk store path) must be threaded onto the
/// cargo **compiling** action — without it, `xcrun`/rustc can't resolve the SDK
/// in the scrubbed sandbox. Analysis-only (no execution), so it needs neither
/// network nor a working linker; it pins that the manifest env reaches the
/// action, the link most likely to regress in a `cargo_builder` refactor.
#[test]
#[cfg(target_os = "macos")]
fn cargo_compiling_actions_carry_the_rust_toolchain_developer_dir() {
    let tmp = cargo_fixture();
    let root = tmp.path();
    let registry = builtin_rules();
    let graph = load_package(root, "ws", &registry).unwrap();
    let cfg = config(OptLevel::Debug);
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();
    let g = Analyzer::new(&graph, &registry, &cfg, root, exec.cas())
        .analyze(&anneal_core::Label::parse("//ws:ws").unwrap())
        .unwrap();

    let action = build_action(&g);
    assert!(
        action
            .env()
            .get("DEVELOPER_DIR")
            .is_some_and(|d| d.starts_with("/nix/store/")),
        "cargo build action must carry DEVELOPER_DIR (a /nix/store SDK path) on macOS; got env {:?}",
        action.env()
    );
}

/// `native_libs = ["zlib"]` attaches the zlib native-lib toolchain (declared in
/// the flake's manifest) to the cargo actions: its `PKG_CONFIG_PATH` env and its
/// bin dir merge onto the *compiling* action, its closure mounts read-only on both
/// the compiling **and** run actions (so a dynamically-linked test binary finds the
/// lib at runtime). Analysis-only (no network, no execution) — pins the attachment
/// wiring, the link most likely to regress. Runs on every platform (zlib is in the
/// manifest unconditionally, unlike the macOS-only DEVELOPER_DIR).
#[test]
fn native_libs_attach_toolchain_roots_and_env_to_cargo_actions() {
    let tmp = cargo_fixture_build("cargo_workspace(name = \"ws\", native_libs = [\"zlib\"])\n");
    let root = tmp.path();
    let registry = builtin_rules();
    let graph = load_package(root, "ws", &registry).unwrap();
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();
    let g = Analyzer::new(
        &graph,
        &registry,
        &config(OptLevel::Debug),
        root,
        exec.cas(),
    )
    .analyze(&anneal_core::Label::parse("//ws:ws").unwrap())
    .unwrap();

    let build = build_action(&g);
    // Env merged in: zlib's PKG_CONFIG_PATH so a `-sys` build script can discover it.
    assert!(
        build
            .env()
            .get("PKG_CONFIG_PATH")
            .is_some_and(|p| p.contains("zlib")),
        "compile action should carry zlib's PKG_CONFIG_PATH; got {:?}",
        build.env().get("PKG_CONFIG_PATH")
    );
    // Roots mounted: the zlib toolchain (its closure) is a declared input.
    let zlib_root_mounted = |action: &anneal_exec::Action| {
        action.toolchains().get("zlib").is_some_and(|t| {
            t.read_only_roots()
                .iter()
                .any(|r| r.to_string_lossy().contains("zlib"))
        })
    };
    assert!(
        zlib_root_mounted(&build),
        "compile action mounts zlib's closure"
    );

    // The run action mounts zlib's roots too (dynamic lib needed at test runtime),
    // even though it carries none of the compile env.
    let run = g
        .actions()
        .find(|a| a.name().starts_with("cargo_workspace test-run"))
        .expect("a unit test-run action");
    assert!(
        zlib_root_mounted(run),
        "run action must mount zlib's closure for a dynamically-linked test binary"
    );
}

#[test]
fn cargo_workspace_builds_hermetically_and_caches() {
    let tmp = cargo_fixture();
    let root = tmp.path();
    let registry = builtin_rules();
    let graph = load_package(root, "ws", &registry).unwrap();
    let cfg = config(OptLevel::Debug);
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();

    let analyzer = Analyzer::new(&graph, &registry, &cfg, root, exec.cas());
    let label = anneal_core::Label::parse("//ws:ws").unwrap();
    let g = analyzer.analyze(&label).unwrap();
    let action = build_action(&g);

    // First build: real, hermetic cargo build through the sandbox.
    let first = exec.execute(&action).unwrap();
    assert!(
        first.success(),
        "cargo build should succeed (exit {})",
        first.exit_code
    );
    assert!(!first.cache_hit);

    // Identical inputs → cache hit, no rebuild.
    let second = exec.execute(&action).unwrap();
    assert!(
        second.cache_hit,
        "identical workspace should hit the action cache"
    );
}

#[test]
fn editing_a_source_busts_the_cache() {
    let tmp = cargo_fixture();
    let root = tmp.path();
    let registry = builtin_rules();
    let cfg = config(OptLevel::Debug);
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();
    let label = anneal_core::Label::parse("//ws:ws").unwrap();

    // Build once.
    let g1 = {
        let graph = load_package(root, "ws", &registry).unwrap();
        Analyzer::new(&graph, &registry, &cfg, root, exec.cas())
            .analyze(&label)
            .unwrap()
    };
    let first = exec.execute(&build_action(&g1)).unwrap();
    assert!(first.success());

    // Edit a source file, re-analyze (new content digest), rebuild.
    std::fs::write(
        root.join("ws/mylib/src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\npub fn sub(a: i32, b: i32) -> i32 { a - b }\n",
    )
    .unwrap();
    let g2 = {
        let graph = load_package(root, "ws", &registry).unwrap();
        Analyzer::new(&graph, &registry, &cfg, root, exec.cas())
            .analyze(&label)
            .unwrap()
    };
    let after_edit = exec.execute(&build_action(&g2)).unwrap();
    assert!(after_edit.success());
    assert!(
        !after_edit.cache_hit,
        "a source edit must bust the build cache"
    );
}

#[test]
fn profile_axis_changes_the_build() {
    // Debug vs release are distinct configured builds (different cache keys).
    let tmp = cargo_fixture();
    let root = tmp.path();
    let registry = builtin_rules();
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();
    let label = anneal_core::Label::parse("//ws:ws").unwrap();
    let graph = load_package(root, "ws", &registry).unwrap();

    let debug_action = build_action(
        &Analyzer::new(
            &graph,
            &registry,
            &config(OptLevel::Debug),
            root,
            exec.cas(),
        )
        .analyze(&label)
        .unwrap(),
    );
    let release_action = build_action(
        &Analyzer::new(
            &graph,
            &registry,
            &config(OptLevel::Release),
            root,
            exec.cas(),
        )
        .analyze(&label)
        .unwrap(),
    );

    assert!(exec.execute(&debug_action).unwrap().success());
    // Release is a different action; first run is a miss, not served from debug's cache.
    let release = exec.execute(&release_action).unwrap();
    assert!(release.success());
    assert!(
        !release.cache_hit,
        "release build must not reuse the debug cache entry"
    );
}

/// Generate `Cargo.lock` for a workspace fixture so `--locked` is satisfied.
fn gen_lockfile(ws: &std::path::Path) {
    let ok = std::process::Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(ws)
        .status()
        .expect("cargo available")
        .success();
    assert!(ok, "cargo generate-lockfile failed");
}

fn build_and_outputs(
    root: &std::path::Path,
) -> std::collections::BTreeMap<String, anneal_core::Digest> {
    let registry = builtin_rules();
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();
    let graph = load_package(root, "ws", &registry).unwrap();
    let action = build_action(
        &Analyzer::new(
            &graph,
            &registry,
            &config(OptLevel::Debug),
            root,
            exec.cas(),
        )
        .analyze(&anneal_core::Label::parse("//ws:ws").unwrap())
        .unwrap(),
    );
    let result = exec.execute(&action).unwrap();
    assert!(result.success(), "build failed (exit {})", result.exit_code);
    result.outputs
}

#[test]
fn proc_macro_member_does_not_break_the_build() {
    // A proc-macro member has `src/lib.rs` but produces a *dylib*, not an rlib. We must
    // detect `[lib] proc-macro = true` and NOT declare its `lib<name>.rlib` output — else
    // the build action fails with a spurious MissingOutput.
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(ws.join("mylib/src")).unwrap();
    std::fs::create_dir_all(ws.join("mymacro/src")).unwrap();
    std::fs::write(
        ws.join("Cargo.toml"),
        "[workspace]\nmembers = [\"mylib\", \"mymacro\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("mylib/Cargo.toml"),
        "[package]\nname = \"mylib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(ws.join("mylib/src/lib.rs"), "pub fn f() -> i32 { 1 }\n").unwrap();
    // A trivial proc-macro — needs only the built-in `proc_macro` crate, so still offline.
    std::fs::write(
        ws.join("mymacro/Cargo.toml"),
        "[package]\nname = \"mymacro\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("mymacro/src/lib.rs"),
        "use proc_macro::TokenStream;\n#[proc_macro]\npub fn noop(_: TokenStream) -> TokenStream { TokenStream::new() }\n",
    )
    .unwrap();
    std::fs::write(ws.join("BUILD"), "cargo_workspace(name = \"ws\")\n").unwrap();
    gen_lockfile(&ws);

    let outputs = build_and_outputs(tmp.path());
    assert!(
        outputs.keys().any(|k| k.contains("libmylib.rlib")),
        "the normal lib's rlib should be declared+captured; got {:?}",
        outputs.keys().collect::<Vec<_>>()
    );
    assert!(
        !outputs.keys().any(|k| k.contains("mymacro")),
        "the proc-macro member must NOT have a declared rlib output; got {:?}",
        outputs.keys().collect::<Vec<_>>()
    );
}

#[test]
fn glob_members_are_expanded() {
    // `members = ["crates/*"]` must enumerate each subcrate (we previously skipped globs).
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::write(
        ws_path(&ws, "Cargo.toml"),
        "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for name in ["alpha", "beta"] {
        std::fs::create_dir_all(ws.join(format!("crates/{name}/src"))).unwrap();
        std::fs::write(
            ws.join(format!("crates/{name}/Cargo.toml")),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        )
        .unwrap();
        std::fs::write(
            ws.join(format!("crates/{name}/src/lib.rs")),
            "pub fn f() {}\n",
        )
        .unwrap();
    }
    std::fs::write(ws.join("BUILD"), "cargo_workspace(name = \"ws\")\n").unwrap();
    gen_lockfile(&ws);

    let outputs = build_and_outputs(tmp.path());
    for name in ["alpha", "beta"] {
        assert!(
            outputs
                .keys()
                .any(|k| k.contains(&format!("lib{name}.rlib"))),
            "glob member {name} should be enumerated and its rlib captured; got {:?}",
            outputs.keys().collect::<Vec<_>>()
        );
    }
}

/// Create `<ws>/<rel>`'s parent and return the full path (small helper for the glob test).
fn ws_path(ws: &std::path::Path, rel: &str) -> std::path::PathBuf {
    std::fs::create_dir_all(ws).unwrap();
    ws.join(rel)
}
