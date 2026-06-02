//! Phase 3 (§13.6): axis interpretation for cargo_workspace. A consumed axis changes
//! the build's cache key (configuration matters), and a coverage build — whose flag
//! is stable-supported — actually compiles through the kernel.

use anneal_analysis::{ActionGraph, Analyzer};
use anneal_core::{AxisValues, Configuration, Coverage, DebugInfo, Label, Platform};
use anneal_exec::{action_digest, Action, Executor, LocalExecutor};
use anneal_loader::load_package;
use anneal_rules::builtin_rules;

fn config(axes: AxisValues) -> Configuration {
    Configuration::new(Platform::new("host", "host"), axes)
}

fn build_action(graph: &ActionGraph) -> Action {
    graph
        .actions()
        .find(|a| a.name().starts_with("cargo_workspace build"))
        .expect("a build action")
        .clone()
}

fn fixture() -> tempfile::TempDir {
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
    std::fs::write(ws.join("mylib/src/lib.rs"), "pub fn add(a: i32, b: i32) -> i32 { a + b }\n").unwrap();
    std::fs::write(ws.join("BUILD"), "cargo_workspace(name = \"ws\")\n").unwrap();
    let status = std::process::Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(&ws)
        .status()
        .expect("cargo available");
    assert!(status.success());
    tmp
}

fn build_action_for(root: &std::path::Path, axes: AxisValues, exec: &LocalExecutor) -> Action {
    let registry = builtin_rules();
    let cfg = config(axes);
    let graph = load_package(root, "ws", &registry).unwrap();
    build_action(
        &Analyzer::new(&graph, &registry, &cfg, root, exec.cas())
            .analyze(&Label::parse("//ws:ws").unwrap())
            .unwrap(),
    )
}

#[test]
fn a_consumed_axis_changes_the_build_cache_key() {
    let tmp = fixture();
    let root = tmp.path();
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();

    // debug_info is consumed and maps to RUSTFLAGS, so changing it must change the key.
    let full = build_action_for(root, AxisValues::default(), &exec);
    let none = build_action_for(
        root,
        AxisValues {
            debug_info: DebugInfo::None,
            ..Default::default()
        },
        &exec,
    );
    assert_ne!(
        action_digest(&full),
        action_digest(&none),
        "a debug_info change must change the build's cache key"
    );
}

#[test]
fn coverage_build_compiles_through_the_kernel() {
    let tmp = fixture();
    let root = tmp.path();
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();

    // -Cinstrument-coverage is stable; the instrumented build should succeed.
    let action = build_action_for(
        root,
        AxisValues {
            coverage: Coverage::On,
            ..Default::default()
        },
        &exec,
    );
    let result = exec.execute(&action).unwrap();
    assert!(
        result.success(),
        "coverage-instrumented build should compile (exit {})",
        result.exit_code
    );
}
