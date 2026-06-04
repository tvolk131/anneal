//! Fetch mode (§FOD): a workspace with a committed `Cargo.lock` but **no** `vendor/`
//! builds by hash-pinned-fetching its crates.io dependencies and assembling a vendor
//! tree in-sandbox — no pre-vendoring required.
//!
//! Ignored by default: it downloads a crate from static.crates.io. Run explicitly:
//!   cargo test -p anneal-analysis --test cargo_fetch -- --ignored

use anneal_analysis::Analyzer;
use anneal_core::{AxisValues, Configuration, Label, OptLevel, Platform};
use anneal_exec::{Executor, LocalExecutor};
use anneal_loader::load_package;
use anneal_rules::builtin_rules;

#[test]
#[ignore = "network: downloads cfg-if from static.crates.io"]
fn fetch_mode_builds_a_registry_dep_offline() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let ws = root.join("ws");
    std::fs::create_dir_all(ws.join("mylib/src")).unwrap();
    std::fs::write(ws.join("Cargo.toml"), "[workspace]\nmembers = [\"mylib\"]\nresolver = \"2\"\n").unwrap();
    std::fs::write(
        ws.join("mylib/Cargo.toml"),
        "[package]\nname = \"mylib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ncfg-if = \"=1.0.0\"\n",
    )
    .unwrap();
    // Use cfg-if at item level so the dep is genuinely compiled and linked.
    std::fs::write(
        ws.join("mylib/src/lib.rs"),
        "cfg_if::cfg_if! { if #[cfg(unix)] { pub fn platform() -> &'static str { \"unix\" } } \
         else { pub fn platform() -> &'static str { \"other\" } } }\n",
    )
    .unwrap();
    std::fs::write(ws.join("BUILD"), "cargo_workspace(name = \"ws\")\n").unwrap();
    // Committed-lockfile case: generate Cargo.lock, and deliberately do NOT vendor.
    let ok = std::process::Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(&ws)
        .status()
        .expect("cargo available")
        .success();
    assert!(ok, "cargo generate-lockfile failed");
    assert!(!ws.join("vendor").exists(), "this exercises the no-vendor fetch path");

    let registry = builtin_rules();
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();
    let cfg = Configuration::new(
        Platform::new("host", "host"),
        AxisValues { opt_level: OptLevel::Debug, ..Default::default() },
    );
    let graph = load_package(root, "ws", &registry).unwrap();
    let g = Analyzer::new(&graph, &registry, &cfg, root, exec.cas())
        .analyze(&Label::parse("//ws:ws").unwrap())
        .unwrap();
    let actions: Vec<_> = g.actions().cloned().collect();

    // A fixed-output fetch action must have been emitted for cfg-if.
    assert!(
        actions.iter().any(|a| a.name() == "cargo_workspace fetch cfg-if-1.0.0"),
        "expected a fetch action for cfg-if; got {:?}",
        actions.iter().map(|a| a.name()).collect::<Vec<_>>()
    );

    // Run the whole graph: fetch (network, hash-verified) → vendor-assemble → build offline.
    // (`results` is index-aligned with `actions`.)
    let results = exec.execute_graph(&actions).unwrap();
    assert!(
        results.iter().all(|r| r.success()),
        "every action (fetch + build) should succeed"
    );

    // The build produced the member's rlib (proving the offline build linked the
    // hash-pinned-fetched dependency).
    let build_idx = actions
        .iter()
        .position(|a| a.name().starts_with("cargo_workspace build"))
        .unwrap();
    assert!(
        results[build_idx].outputs.keys().any(|k| k.contains("libmylib.rlib")),
        "build should declare+capture libmylib.rlib; got {:?}",
        results[build_idx].outputs.keys().collect::<Vec<_>>()
    );
}
