//! Phase 4 / `pnpm_workspace` script layer: declared `scripts` become per-script
//! actions that run against the restored `node_modules` (`docs/pnpm-workspace.md` §2–5).
//! A `test`-kind script captures its run and always exits 0 (failure is recorded data);
//! a `build`-kind script declares outputs and exposes them as a provider. Both are
//! `SnapshotAccelerated`: they restore `install`'s snapshot read-only and never cache.

use anneal_analysis::Analyzer;
use anneal_core::{AxisValues, Configuration, Label, OptLevel, Platform};
use anneal_exec::{Action, LocalExecutor};
use anneal_loader::load_package;
use anneal_rules::builtin_rules;

fn debug() -> Configuration {
    Configuration::new(
        Platform::new("host", "host"),
        AxisValues {
            opt_level: OptLevel::Debug,
            ..Default::default()
        },
    )
}

/// A zero-dependency package with a `test` script (asserts + prints a marker) and a
/// `build` script (writes a declared output). The lockfile is generated without
/// installing; the install action builds `node_modules` in the sandbox.
fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let app = tmp.path().join("app");
    std::fs::create_dir_all(&app).unwrap();

    std::fs::write(
        app.join("package.json"),
        "{\n  \"name\": \"app\",\n  \"version\": \"0.0.0\",\n  \"private\": true,\n  \
         \"scripts\": { \"test\": \"node test.js\", \"build\": \"node build.js\" }\n}\n",
    )
    .unwrap();
    std::fs::write(
        app.join("test.js"),
        "const assert = require('node:assert');\n\
         assert.strictEqual(1 + 1, 2);\n\
         console.log('PNPM_TEST_RAN');\n",
    )
    .unwrap();
    std::fs::write(
        app.join("build.js"),
        "require('node:fs').writeFileSync('dist.txt', 'built\\n');\n",
    )
    .unwrap();
    std::fs::write(
        app.join("BUILD"),
        "pnpm_workspace(\n    name = \"app\",\n    scripts = {\n        \
         \"test\": { \"kind\": \"test\" },\n        \
         \"build\": { \"kind\": \"build\", \"outputs\": [\"dist.txt\"] },\n    },\n)\n",
    )
    .unwrap();

    let status = std::process::Command::new("pnpm")
        .args(["install", "--lockfile-only", "--ignore-scripts"])
        .current_dir(&app)
        .status()
        .expect("pnpm available");
    assert!(status.success(), "pnpm install --lockfile-only failed");
    tmp
}

fn idx(actions: &[Action], needle: &str) -> usize {
    actions
        .iter()
        .position(|a| a.name().starts_with(needle))
        .unwrap_or_else(|| panic!("no action matching {needle:?}"))
}

#[test]
fn declared_scripts_run_against_the_installed_workspace() {
    let tmp = fixture();
    let root = tmp.path();
    let registry = builtin_rules();
    let cfg = debug();
    let exec = LocalExecutor::new(root.join(".mybuild")).unwrap();
    let label = Label::parse("//app:app").unwrap();

    let graph = load_package(root, "app", &registry).unwrap();
    let g = Analyzer::new(&graph, &registry, &cfg, root, exec.cas())
        .analyze(&label)
        .unwrap();
    let actions: Vec<Action> = g.actions().cloned().collect();
    // install + one test + one build.
    assert_eq!(actions.len(), 3, "install + test + build");

    let results = exec.execute_graph(&actions).unwrap();

    // install succeeded (so node_modules was saved to the snapshot).
    let install = &results[idx(&actions, "pnpm_workspace install")];
    assert!(install.success(), "install failed (exit {})", install.exit_code);

    // The test script ran against the restored node_modules; it always exits 0, and the
    // captured output proves the script body actually executed and passed.
    let test = &results[idx(&actions, "pnpm_workspace test")];
    assert!(test.success(), "test action wrapper should always exit 0");
    let results_txt = String::from_utf8(
        exec.cas()
            .get(test.outputs.get("results.txt").expect("results.txt captured"))
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert!(
        results_txt.contains("PNPM_TEST_RAN"),
        "test script did not run; results:\n{results_txt}"
    );
    assert!(
        results_txt.contains("ANNEAL_TEST_EXIT=0"),
        "test should have passed (exit 0); results:\n{results_txt}"
    );

    // The build script produced its declared output, content-addressed in the CAS.
    let build = &results[idx(&actions, "pnpm_workspace build")];
    assert!(build.success(), "build failed (exit {})", build.exit_code);
    let dist = String::from_utf8(
        exec.cas()
            .get(build.outputs.get("dist.txt").expect("dist.txt captured"))
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(dist, "built\n");

    // The build output is exposed as a provider for downstream consumers.
    let files = g
        .providers(&label)
        .expect("providers for the analyzed target")
        .files
        .as_ref()
        .expect("a file provider");
    assert!(
        files.files.iter().any(|a| a.path == std::path::Path::new("dist.txt")),
        "build output should be provided"
    );
}
