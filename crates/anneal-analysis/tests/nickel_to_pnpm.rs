//! Phase 4 / §14.3 (plain-path): a `nickel_eval` generates a JSON that a `pnpm_workspace`
//! routes — by **plain-path** (`docs/pnpm-workspace.md` §4) — into a consuming test script,
//! which reads it by relative path. This is the cross-language boundary (Nickel → TS) with
//! composing caches: editing the Nickel source propagates the new value to the consumer, while
//! editing only the consumer leaves the generator cached.
//!
//! The routed `config.json` is a **direct input to the script** (a §14.6 Level-1 clean edge);
//! `install` stays config-agnostic, so a Nickel edit does not trigger a reinstall.

use anneal_analysis::Analyzer;
use anneal_core::{AxisValues, Configuration, Label, OptLevel, Platform};
use anneal_exec::{Action, ActionResult, LocalExecutor};
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

/// A pnpm workspace whose `test` script reads a Nickel-generated `gen/config.json` by
/// relative path. `greeting` is the value Nickel exports; the test threads it through.
fn fixture(greeting: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let app = tmp.path().join("app");
    std::fs::create_dir_all(&app).unwrap();

    write_config(&app, greeting);
    std::fs::write(
        app.join("package.json"),
        "{\n  \"name\": \"app\",\n  \"version\": \"0.0.0\",\n  \"private\": true,\n  \
         \"scripts\": { \"test\": \"node test.js\" }\n}\n",
    )
    .unwrap();
    // The consumer reads the routed file by relative path and echoes the value.
    std::fs::write(
        app.join("test.js"),
        "const cfg = require('./gen/config.json');\n\
         const assert = require('node:assert');\n\
         assert.ok(typeof cfg.greeting === 'string', 'greeting present');\n\
         console.log('GREETING=' + cfg.greeting);\n",
    )
    .unwrap();
    std::fs::write(
        app.join("BUILD"),
        "nickel_eval(name = \"cfg\", src = \"config.ncl\", out = \"config.json\")\n\
         pnpm_workspace(\n    name = \"app\",\n    \
         data = { \"//app:cfg\": \"gen/config.json\" },\n    \
         scripts = { \"test\": { \"kind\": \"test\" } },\n)\n",
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

fn write_config(app: &std::path::Path, greeting: &str) {
    std::fs::write(
        app.join("config.ncl"),
        format!("{{ greeting = \"{greeting}\", n = 1 }}\n"),
    )
    .unwrap();
}

fn idx(actions: &[Action], needle: &str) -> usize {
    actions
        .iter()
        .position(|a| a.name().starts_with(needle))
        .unwrap_or_else(|| panic!("no action matching {needle:?}"))
}

/// Load → analyze → execute the whole graph; return the actions and their aligned results.
fn run(
    exec: &LocalExecutor,
    root: &std::path::Path,
    cfg: &Configuration,
) -> (Vec<Action>, Vec<ActionResult>) {
    let registry = builtin_rules();
    let label = Label::parse("//app:app").unwrap();
    let graph = load_package(root, "app", &registry).unwrap();
    let g = Analyzer::new(&graph, &registry, cfg, root, exec.cas())
        .analyze(&label)
        .unwrap();
    let actions: Vec<Action> = g.actions().cloned().collect();
    let results = exec.execute_graph(&actions).unwrap();
    (actions, results)
}

fn results_txt(exec: &LocalExecutor, actions: &[Action], results: &[ActionResult]) -> String {
    let test = &results[idx(actions, "pnpm_workspace test")];
    String::from_utf8(
        exec.cas()
            .get(
                test.outputs
                    .get("results.txt")
                    .expect("results.txt captured"),
            )
            .unwrap()
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn pnpm_test_reads_the_nickel_generated_json() {
    let tmp = fixture("hello");
    let root = tmp.path();
    let cfg = debug();
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();

    let (actions, results) = run(&exec, root, &cfg);

    // The Nickel producer is a dependency of the workspace, analyzed (and run) before it.
    assert!(
        results[idx(&actions, "nickel_eval")].success(),
        "nickel failed"
    );
    assert!(
        results[idx(&actions, "pnpm_workspace install")].success(),
        "install failed"
    );

    let out = results_txt(&exec, &actions, &results);
    // Routing worked: the script read the materialized JSON by relative path, and the
    // assertion passed (exit 0 ⇒ the value was present and well-typed).
    assert!(
        out.contains("GREETING=hello"),
        "routed value not seen; results:\n{out}"
    );
    assert!(
        out.contains("ANNEAL_TEST_EXIT=0"),
        "test should have passed; results:\n{out}"
    );
}

#[test]
fn composing_caches_across_the_boundary() {
    let tmp = fixture("hello");
    let root = tmp.path();
    let cfg = debug();
    let exec = LocalExecutor::new(root.join(".anneal")).unwrap();

    // Cold build.
    let (a1, r1) = run(&exec, root, &cfg);
    assert!(
        !r1[idx(&a1, "nickel_eval")].cache_hit,
        "first nickel run is a miss"
    );
    assert!(results_txt(&exec, &a1, &r1).contains("GREETING=hello"));

    // Edit only the consumer is unnecessary to show the cached direction — an identical
    // re-run already proves the generator is cached when nothing it depends on changed.
    let (a2, r2) = run(&exec, root, &cfg);
    assert!(
        r2[idx(&a2, "nickel_eval")].cache_hit,
        "unchanged re-run: generator stays cached"
    );

    // Edit the Nickel source → the new value must propagate through the routing edge.
    write_config(&root.join("app"), "howdy");
    let (a3, r3) = run(&exec, root, &cfg);
    assert!(
        !r3[idx(&a3, "nickel_eval")].cache_hit,
        "editing .ncl rebuilds the generator"
    );
    // install is config-agnostic under plain-path: a Nickel edit does not reinstall.
    assert!(
        r3[idx(&a3, "pnpm_workspace install")].cache_hit,
        "config edit must not reinstall"
    );
    let out = results_txt(&exec, &a3, &r3);
    assert!(
        out.contains("GREETING=howdy"),
        "new Nickel value did not reach the consumer:\n{out}"
    );
}
