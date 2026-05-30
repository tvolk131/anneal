//! Phase 3 (§12.2–12.3): per-`(crate, test_type)` test targets with the compile/run
//! split, and the payoff — an unrelated edit busts the *compile* cache (coarse
//! whole-tree inputs) but the *run* still hits because the test binary is unchanged.

use anneal_analysis::Analyzer;
use anneal_core::{AxisValues, Configuration, Label, OptLevel, Platform};
use anneal_exec::{Action, ActionResult, LocalExecutor};
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

/// Single-crate workspace with a unit test, plus an unrelated `README.txt`.
fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(ws.join("mathlib/src")).unwrap();
    std::fs::write(
        ws.join("Cargo.toml"),
        "[workspace]\nmembers = [\"mathlib\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("mathlib/Cargo.toml"),
        "[package]\nname = \"mathlib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("mathlib/src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n\
         #[cfg(test)]\nmod tests {\n    #[test]\n    fn adds() { assert_eq!(super::add(2, 2), 4); }\n}\n",
    )
    .unwrap();
    std::fs::write(ws.join("README.txt"), "version 1\n").unwrap();
    std::fs::write(ws.join("BUILD"), "cargo_workspace(name = \"ws\")\n").unwrap();

    let status = std::process::Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(&ws)
        .status()
        .expect("cargo available");
    assert!(status.success());
    tmp
}

/// The result of the action whose name matches `pat`, paired by position.
fn result_for<'a>(actions: &[Action], results: &'a [ActionResult], pat: &str) -> &'a ActionResult {
    let idx = actions
        .iter()
        .position(|a| a.name().contains(pat))
        .unwrap_or_else(|| panic!("no action matching {pat:?}"));
    &results[idx]
}

#[test]
fn unit_test_compile_run_split_with_run_cache_hit() {
    let tmp = fixture();
    let root = tmp.path();
    let registry = builtin_rules();
    let cfg = debug_config();
    let exec = LocalExecutor::new(root.join(".mybuild")).unwrap();
    let label = Label::parse("//ws:ws").unwrap();

    let analyze = || -> Vec<Action> {
        let graph = load_package(root, "ws", &registry).unwrap();
        Analyzer::new(&graph, &registry, &cfg, root, exec.cas())
            .analyze(&label)
            .unwrap()
            .actions()
            .cloned()
            .collect()
    };

    // The rule emits a unit compile + run (the split) and a doc action for the crate.
    let actions = analyze();
    assert!(actions.iter().any(|a| a.name().contains("test-compile") && a.name().contains("unit")));
    assert!(actions.iter().any(|a| a.name().contains("test-run") && a.name().contains("unit")));

    // First run of the whole graph: the unit test actually executes and passes.
    let first = exec.execute_graph(&actions).unwrap();
    let run1 = result_for(&actions, &first, "test-run");
    assert!(run1.success(), "unit tests should run and pass (exit {})", run1.exit_code);
    assert!(!run1.cache_hit, "first run executes the test binary");

    // Edit an UNRELATED file (the README). Cargo never reads it, but it is part of the
    // coarse whole-tree inputs, so the compile action's key changes.
    std::fs::write(root.join("ws/README.txt"), "version 2 — unrelated change\n").unwrap();

    let actions2 = analyze();
    let second = exec.execute_graph(&actions2).unwrap();
    let compile2 = result_for(&actions2, &second, "test-compile");
    let run2 = result_for(&actions2, &second, "test-run");

    assert!(
        !compile2.cache_hit,
        "the unrelated edit changes the coarse inputs, so compile re-runs"
    );
    assert!(
        run2.cache_hit,
        "but the test binary is byte-identical, so the run is a cache hit (tests not re-run)"
    );
}
