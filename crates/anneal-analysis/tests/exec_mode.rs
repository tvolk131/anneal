//! ExecMode through the rule layer (DESIGN.md §4.1/§4.4, increment A): the same
//! cargo target analyzed under `Incremental` takes a `mutate_state` grant on its
//! warm `target/`; under `Hermetic` it emits the same actions with **no state
//! grant** — cold, deterministic, promotable under full enforcement. Under axis-mapping
//! (§13.6) exec_mode is NOT in cargo's consumed axes (it maps to no flag and is
//! correctness-neutral), so it no longer enters the key *as an axis value*. The two
//! variants' action keys still differ — but via their **execution contract**
//! (`cache_policy` + `snapshot_paths`: SnapshotBased-with-`target/` vs Deterministic),
//! not via exec_mode. The snapshot's *identity* key stays out of the action key (§8.2).

use anneal_analysis::{ActionGraph, Analyzer};
use anneal_core::{AxisValues, Configuration, ExecMode, Label, Platform};
use anneal_exec::{action_digest, Action, LocalExecutor};
use anneal_loader::load_package;
use anneal_rules::builtin_rules;

fn config(exec_mode: ExecMode) -> Configuration {
    Configuration::new(
        Platform::new("host", "host"),
        AxisValues {
            exec_mode,
            ..Default::default()
        },
    )
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
        .expect("cargo available");
    assert!(status.success());
    tmp
}

fn build_action_for(root: &std::path::Path, mode: ExecMode, exec: &LocalExecutor) -> Action {
    let registry = builtin_rules();
    let cfg = config(mode);
    let graph = load_package(root, "ws", &registry).unwrap();
    build_action(
        &Analyzer::new(&graph, &registry, &cfg, root, exec.cas())
            .analyze(&Label::parse("//ws:ws").unwrap())
            .unwrap(),
    )
}

#[test]
fn hermetic_arm_takes_no_state_grant() {
    let tmp = fixture();
    let root = tmp.path();
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();

    let incremental = build_action_for(root, ExecMode::Incremental, &exec);
    assert!(
        incremental.snapshot_key().is_some(),
        "incremental cargo mutates warm target/ state"
    );

    let hermetic = build_action_for(root, ExecMode::Hermetic, &exec);
    assert!(
        hermetic.snapshot_key().is_none(),
        "hermetic cargo takes no state grant — cold and promotable"
    );

    // exec_mode is no longer in `consumed_axes` (it maps to no flag — §13.6), so it does
    // NOT enter the key as an axis value. The two variants' keys still differ, but for a
    // different, legitimate reason: the Incremental arm is `SnapshotBased` with a `target/`
    // snapshot path and the Hermetic arm is `Deterministic` with none — that execution-
    // contract difference (`cache_policy` + `snapshot_paths`) is part of the action key,
    // even though the snapshot's *identity* key is not (§8.2). So warm and cold builds
    // remain distinct actions; dropping exec_mode from `consumed` does not, by itself,
    // collapse them.
    assert_ne!(action_digest(&incremental), action_digest(&hermetic));
}
