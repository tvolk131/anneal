//! Phase 3 (§12.4, §19.2): the unit-test run action captures the framework output,
//! and `anneal-test` translates it into structured per-case results — including for a
//! failing test, which is a *recorded result*, not a lost action error.

use anneal_analysis::Analyzer;
use anneal_core::{AxisValues, Configuration, Label, OptLevel, Platform};
use anneal_exec::LocalExecutor;
use anneal_loader::load_package;
use anneal_rules::builtin_rules;
use anneal_test::{TestOutcome, TestResult};

fn debug_config() -> Configuration {
    Configuration::new(
        Platform::new("host", "host"),
        AxisValues {
            opt_level: OptLevel::Debug,
            ..Default::default()
        },
    )
}

/// A single-crate workspace whose unit tests are `lib_rs` (the test bodies).
fn fixture(lib_rs: &str) -> tempfile::TempDir {
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
    std::fs::write(ws.join("mathlib/src/lib.rs"), lib_rs).unwrap();
    std::fs::write(ws.join("BUILD"), "cargo_workspace(name = \"ws\")\n").unwrap();
    let status = std::process::Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(&ws)
        .status()
        .expect("cargo available");
    assert!(status.success());
    tmp
}

/// Run the graph and translate the unit-test run's captured output into a TestResult.
fn run_and_collect(lib_rs: &str) -> TestResult {
    let tmp = fixture(lib_rs);
    let root = tmp.path();
    let registry = builtin_rules();
    let cfg = debug_config();
    let exec = LocalExecutor::new(root.join(".mybuild")).unwrap();
    let label = Label::parse("//ws:ws").unwrap();

    let graph = load_package(root, "ws", &registry).unwrap();
    let actions: Vec<_> = Analyzer::new(&graph, &registry, &cfg, root, exec.cas())
        .analyze(&label)
        .unwrap()
        .actions()
        .cloned()
        .collect();

    let results = exec.execute_graph(&actions).unwrap();
    let run_idx = actions
        .iter()
        .position(|a| a.name().contains("test-run") && a.name().contains("unit"))
        .expect("a unit test-run action");
    let run = &results[run_idx];
    assert!(run.success(), "run action records results even on test failure");

    let output_digest = run.outputs.get("results.txt").expect("captured results.txt");
    let output = String::from_utf8(exec.cas().get(output_digest).unwrap().unwrap()).unwrap();
    TestResult::from_libtest(label, cfg, &output)
}

#[test]
fn passing_tests_produce_structured_passes() {
    let result = run_and_collect(
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n\
         #[cfg(test)]\nmod tests {\n\
            #[test] fn adds() { assert_eq!(super::add(2, 2), 4); }\n\
            #[test] fn more() { assert!(super::add(1, 1) == 2); }\n\
         }\n",
    );

    assert_eq!(result.outcome, TestOutcome::Passed);
    assert_eq!(result.count(TestOutcome::Passed), 2);
    assert_eq!(result.count(TestOutcome::Failed), 0);
    // Per-case identity is stable and target-qualified.
    let adds = result.cases.iter().find(|c| c.name == "tests::adds").unwrap();
    assert_eq!(result.case_id(adds), "//ws:ws#tests::adds");
}

#[test]
fn a_failing_test_is_a_recorded_result_not_an_error() {
    let result = run_and_collect(
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n\
         #[cfg(test)]\nmod tests {\n\
            #[test] fn adds() { assert_eq!(super::add(2, 2), 4); }\n\
            #[test] fn broken() { assert_eq!(super::add(2, 2), 5); }\n\
         }\n",
    );

    assert_eq!(result.outcome, TestOutcome::Failed);
    assert_eq!(result.count(TestOutcome::Passed), 1);
    assert_eq!(result.count(TestOutcome::Failed), 1);

    let broken = result.cases.iter().find(|c| c.name == "tests::broken").unwrap();
    assert_eq!(broken.outcome, TestOutcome::Failed);
    assert!(
        broken.failure_message.as_deref().unwrap_or("").contains("assertion"),
        "the failure detail should be captured: {:?}",
        broken.failure_message
    );
}
