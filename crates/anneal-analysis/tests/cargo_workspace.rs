//! Phase 3, increment 1: a `cargo_workspace` builds hermetically through the
//! sandbox and is content-addressed on its sources + toolchain + profile.
//!
//! Uses a dependency-free Cargo workspace so `cargo build --offline` needs no
//! network (Milestone 1 is scoped to public-/no-dependency workflows; vendoring is
//! a later increment).

use anneal_analysis::Analyzer;
use anneal_core::{AxisValues, Configuration, OptLevel, Platform};
use anneal_exec::{Executor, LocalExecutor};
use anneal_loader::load_package;
use anneal_rules::builtin_rules;

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
    std::fs::write(ws.join("BUILD"), "cargo_workspace(name = \"ws\")\n").unwrap();

    let status = std::process::Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(&ws)
        .status()
        .expect("cargo must be available to set up the fixture");
    assert!(status.success(), "cargo generate-lockfile failed");
    tmp
}

#[test]
fn cargo_workspace_builds_hermetically_and_caches() {
    let tmp = cargo_fixture();
    let root = tmp.path();
    let registry = builtin_rules();
    let graph = load_package(root, "ws", &registry).unwrap();
    let cfg = config(OptLevel::Debug);
    let exec = LocalExecutor::new(root.join(".mybuild")).unwrap();

    let analyzer = Analyzer::new(&graph, &registry, &cfg, root, exec.cas());
    let label = anneal_core::Label::parse("//ws:ws").unwrap();
    let g = analyzer.analyze(&label).unwrap();
    assert_eq!(g.action_count(), 1, "one coarse build action");

    let action = g.actions().next().unwrap().clone();

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
    assert!(second.cache_hit, "identical workspace should hit the action cache");
}

#[test]
fn editing_a_source_busts_the_cache() {
    let tmp = cargo_fixture();
    let root = tmp.path();
    let registry = builtin_rules();
    let cfg = config(OptLevel::Debug);
    let exec = LocalExecutor::new(root.join(".mybuild")).unwrap();
    let label = anneal_core::Label::parse("//ws:ws").unwrap();

    // Build once.
    let g1 = {
        let graph = load_package(root, "ws", &registry).unwrap();
        Analyzer::new(&graph, &registry, &cfg, root, exec.cas())
            .analyze(&label)
            .unwrap()
    };
    let first = exec.execute(g1.actions().next().unwrap()).unwrap();
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
    let after_edit = exec.execute(g2.actions().next().unwrap()).unwrap();
    assert!(after_edit.success());
    assert!(!after_edit.cache_hit, "a source edit must bust the build cache");
}

#[test]
fn profile_axis_changes_the_build() {
    // Debug vs release are distinct configured builds (different cache keys).
    let tmp = cargo_fixture();
    let root = tmp.path();
    let registry = builtin_rules();
    let exec = LocalExecutor::new(root.join(".mybuild")).unwrap();
    let label = anneal_core::Label::parse("//ws:ws").unwrap();
    let graph = load_package(root, "ws", &registry).unwrap();

    let debug_action = Analyzer::new(&graph, &registry, &config(OptLevel::Debug), root, exec.cas())
        .analyze(&label)
        .unwrap()
        .actions()
        .next()
        .unwrap()
        .clone();
    let release_action =
        Analyzer::new(&graph, &registry, &config(OptLevel::Release), root, exec.cas())
            .analyze(&label)
            .unwrap()
            .actions()
            .next()
            .unwrap()
            .clone();

    assert!(exec.execute(&debug_action).unwrap().success());
    // Release is a different action; first run is a miss, not served from debug's cache.
    let release = exec.execute(&release_action).unwrap();
    assert!(release.success());
    assert!(!release.cache_hit, "release build must not reuse the debug cache entry");
}
