//! Phase 4 / §14.6: a `cargo_workspace` consumes a `nickel_eval`'s generated JSON as
//! a package-local input — the Rust crate `include_str!`s it and a unit test asserts
//! the embedded content. This is the inner-tool-only routing case (Cargo/rustc read
//! the file at execution; Anneal never introspects it), and it's the Nickel → Rust
//! direction of generated-native-package routing.

use anneal_analysis::Analyzer;
use anneal_core::{AxisValues, Configuration, Label, OptLevel, Platform};
use anneal_exec::{Action, ActionResult, LocalExecutor};
use anneal_loader::load_package;
use anneal_rules::builtin_rules;
use anneal_test::{TestOutcome, TestResult};

fn debug() -> Configuration {
    Configuration::new(
        Platform::new("host", "host"),
        AxisValues {
            opt_level: OptLevel::Debug,
            ..Default::default()
        },
    )
}

/// A workspace `app/` whose single crate embeds a Nickel-generated `gen/config.json`.
/// `greeting` is the value the Nickel config exports — the test threads it through.
fn fixture(greeting: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let app = tmp.path().join("app");
    std::fs::create_dir_all(app.join("thecrate/src")).unwrap();

    std::fs::write(
        app.join("config.ncl"),
        format!("{{ greeting = \"{greeting}\", count = 3 }}\n"),
    )
    .unwrap();
    std::fs::write(
        app.join("Cargo.toml"),
        "[workspace]\nmembers = [\"thecrate\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    std::fs::write(
        app.join("thecrate/Cargo.toml"),
        "[package]\nname = \"thecrate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    // The crate embeds the generated JSON at compile time; the unit test asserts it.
    std::fs::write(
        app.join("thecrate/src/lib.rs"),
        "pub const CONFIG: &str = include_str!(\"../../gen/config.json\");\n\
         #[cfg(test)]\nmod tests {\n    #[test]\n    fn embeds_generated_config() {\n        \
         assert!(super::CONFIG.contains(\"hello\"), \"embedded: {}\", super::CONFIG);\n    }\n}\n",
    )
    .unwrap();
    std::fs::write(
        app.join("BUILD"),
        "nickel_eval(name = \"cfg\", src = \"config.ncl\", out = \"gen/config.json\")\n\
         cargo_workspace(name = \"app\", data = [\"//app:cfg\"])\n",
    )
    .unwrap();

    let status = std::process::Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(&app)
        .status()
        .expect("cargo available");
    assert!(status.success());
    tmp
}

#[test]
fn cargo_crate_embeds_nickel_generated_json() {
    let tmp = fixture("hello");
    let root = tmp.path();
    let registry = builtin_rules();
    let cfg = debug();
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();
    let label = Label::parse("//app:app").unwrap();

    let graph = load_package(root, "app", &registry).unwrap();
    let analyzer = Analyzer::new(&graph, &registry, &cfg, root, exec.cas());
    let g = analyzer.analyze(&label).unwrap();

    // The Nickel producer is a dependency, analyzed before the workspace.
    assert!(g.order().contains(&Label::parse("//app:cfg").unwrap()));

    let actions: Vec<Action> = g.actions().cloned().collect();
    let results = exec.execute_graph(&actions).unwrap();

    // The build action compiled — which can only happen if the generated JSON was
    // materialized into the tree (else include_str! is a compile error).
    let build_idx = actions
        .iter()
        .position(|a| a.name().starts_with("cargo_workspace build"))
        .unwrap();
    assert!(
        results[build_idx].success(),
        "build failed (exit {}) — generated JSON likely not materialized",
        results[build_idx].exit_code
    );

    // The unit test ran and passed — proving the *content* of the Nickel output flowed
    // through into the compiled crate.
    let run_idx = actions
        .iter()
        .position(|a| a.name().contains("test-run") && a.name().contains("thecrate"))
        .unwrap();
    let run = &results[run_idx];
    let output = String::from_utf8(
        exec.cas()
            .get(run.outputs.get("results.txt").unwrap())
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let result = TestResult::from_libtest(label, cfg, &output);
    assert_eq!(
        result.outcome,
        TestOutcome::Passed,
        "unit test output:\n{output}"
    );
    assert!(
        result
        .cases
        .iter()
            .any(|c| c.name == "tests::embeds_generated_config"
                && c.outcome == TestOutcome::Passed),
        "expected generated-config unit case to pass; parsed cases: {:?}\noutput:\n{output}",
        result.cases
    );
}

#[test]
fn editing_nickel_source_rebuilds_the_consuming_workspace() {
    // Changing the Nickel source regenerates the JSON, whose content is in the build
    // action's cache key — so the consuming Rust workspace rebuilds. (The build action
    // references the Nickel output, so it must be run through the graph, not alone.)
    let tmp = fixture("hello");
    let root = tmp.path();
    let registry = builtin_rules();
    let cfg = debug();
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();
    let label = Label::parse("//app:app").unwrap();

    // Run just the producer + the build action and return the build's result.
    let run_build = |exec: &LocalExecutor| -> ActionResult {
        let graph = load_package(root, "app", &registry).unwrap();
        let g = Analyzer::new(&graph, &registry, &cfg, root, exec.cas())
            .analyze(&label)
            .unwrap();
        let actions: Vec<Action> = g
            .actions()
            .filter(|a| {
                a.name().starts_with("nickel_eval") || a.name().starts_with("cargo_workspace build")
            })
            .cloned()
            .collect();
        let results = exec.execute_graph(&actions).unwrap();
        let idx = actions
            .iter()
            .position(|a| a.name().starts_with("cargo_workspace build"))
            .unwrap();
        results[idx].clone()
    };

    assert!(!run_build(&exec).cache_hit, "first build is a miss");
    assert!(
        run_build(&exec).cache_hit,
        "identical re-run hits the build cache"
    );

    // Edit the Nickel source → regenerated JSON propagates across the routing edge.
    std::fs::write(
        root.join("app/config.ncl"),
        "{ greeting = \"hello there\", count = 4 }\n",
    )
    .unwrap();
    let after = run_build(&exec);
    assert!(
        !after.cache_hit,
        "editing the Nickel source must rebuild the consuming workspace"
    );
    assert!(after.success());
}
