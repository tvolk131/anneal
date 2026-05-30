//! Phase 3 release-blocker gate (§1.4, §22): a snapshot-restored (incremental) build
//! must produce byte-identical outputs to a cold (clean) build.
//!
//! Uses a two-crate workspace (`applib` depends on `corelib`). We prime a snapshot at
//! source state v1, then edit only `applib`. The warm build therefore **reuses**
//! `corelib`'s artifact from the snapshot while recompiling `applib` — exactly the
//! incremental path whose correctness the invariant governs.

use anneal_analysis::Analyzer;
use anneal_core::{AxisValues, Configuration, Label, OptLevel, Platform};
use anneal_exec::{prime_snapshot, verify_correctness_neutral, Action, LocalExecutor};
use anneal_loader::load_package;
use anneal_rules::builtin_rules;

fn debug_config() -> Configuration {
    Configuration::new(
        Platform::new("host", "host"),
        AxisValues {
            opt_level: OptLevel::Debug,
            ..Default::default()
        },
    )
}

/// A two-crate Cargo workspace with a BUILD file and a generated lockfile.
fn two_crate_fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(ws.join("corelib/src")).unwrap();
    std::fs::create_dir_all(ws.join("applib/src")).unwrap();

    std::fs::write(
        ws.join("Cargo.toml"),
        "[workspace]\nmembers = [\"corelib\", \"applib\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("corelib/Cargo.toml"),
        "[package]\nname = \"corelib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("corelib/src/lib.rs"),
        "pub fn base() -> i32 { 41 }\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("applib/Cargo.toml"),
        "[package]\nname = \"applib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\ncorelib = { path = \"../corelib\" }\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("applib/src/lib.rs"),
        "pub fn answer() -> i32 { corelib::base() + 1 }\n",
    )
    .unwrap();
    std::fs::write(ws.join("BUILD"), "cargo_workspace(name = \"ws\")\n").unwrap();

    let status = std::process::Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(&ws)
        .status()
        .expect("cargo must be available for fixture setup");
    assert!(status.success());
    tmp
}

#[test]
fn snapshot_restored_build_is_correctness_neutral() {
    let tmp = two_crate_fixture();
    let root = tmp.path();
    let registry = builtin_rules();
    let cfg = debug_config();
    let exec = LocalExecutor::new(root.join(".mybuild")).unwrap();
    let label = Label::parse("//ws:ws").unwrap();

    // Analyze + return the coarse build action for the current sources.
    let build_action = |exec: &LocalExecutor| -> Action {
        let graph = load_package(root, "ws", &registry).unwrap();
        Analyzer::new(&graph, &registry, &cfg, root, exec.cas())
            .analyze(&label)
            .unwrap()
            .actions()
            .find(|a| a.name().starts_with("cargo_workspace build"))
            .expect("a build action")
            .clone()
    };

    // v1: prime the snapshot (clean build of both crates, snapshot saved).
    let v1 = build_action(&exec);
    let primed = prime_snapshot(&exec, &v1).unwrap();
    assert!(primed.success(), "priming build failed (exit {})", primed.exit_code);
    assert_eq!(primed.outputs.len(), 2, "two library rlibs expected");

    // Edit ONLY applib → v2. corelib is unchanged, so the warm build reuses it.
    std::fs::write(
        root.join("ws/applib/src/lib.rs"),
        "pub fn answer() -> i32 { corelib::base() + 100 }\n",
    )
    .unwrap();

    // The gate: cold (clean) vs warm (snapshot-restored, incremental) must match.
    let v2 = build_action(&exec);
    let report = verify_correctness_neutral(&exec, &v2).unwrap();

    assert!(
        !report.cold.is_empty(),
        "there must be declared outputs to compare"
    );
    assert!(
        report.is_neutral(),
        "snapshot-warm build diverged from cold build on: {:?}\ncold={:?}\nwarm={:?}",
        report.divergences(),
        report.cold,
        report.warm,
    );
}
