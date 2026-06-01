//! Phase 4 / `pnpm_workspace` step 1: the `install` action. A single-package pnpm
//! workspace analyzes to one coarse install action — `pnpm install --offline
//! --frozen-lockfile --ignore-scripts` — that is sealed, snapshot-based, and cacheable.
//! This is the deterministic, inferred core of the rule (`docs/pnpm-workspace.md` §1, §6);
//! the script layer and the Nickel→TS routing are later steps.

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

/// A single-package pnpm workspace `app/` with no dependencies. We generate the
/// lockfile (without installing) so the hermetic build can run `--frozen-lockfile
/// --offline`; the install action itself produces `node_modules` inside the sandbox.
fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let app = tmp.path().join("app");
    std::fs::create_dir_all(&app).unwrap();

    std::fs::write(
        app.join("package.json"),
        "{ \"name\": \"app\", \"version\": \"0.0.0\", \"private\": true }\n",
    )
    .unwrap();
    std::fs::write(app.join("BUILD"), "pnpm_workspace(name = \"app\")\n").unwrap();

    // Lockfile only — no node_modules in the source tree; the action builds that.
    let status = std::process::Command::new("pnpm")
        .args(["install", "--lockfile-only", "--ignore-scripts"])
        .current_dir(&app)
        .status()
        .expect("pnpm available");
    assert!(status.success(), "pnpm install --lockfile-only failed");
    assert!(app.join("pnpm-lock.yaml").exists(), "lockfile not generated");
    tmp
}

fn install_action(actions: &[Action]) -> usize {
    actions
        .iter()
        .position(|a| a.name().starts_with("pnpm_workspace install"))
        .expect("an install action")
}

#[test]
fn pnpm_workspace_emits_one_installing_action_that_succeeds() {
    let tmp = fixture();
    let root = tmp.path();
    let registry = builtin_rules();
    let cfg = debug();
    let exec = LocalExecutor::new(root.join(".mybuild")).unwrap();
    let label = Label::parse("//app:app").unwrap();

    let graph = load_package(root, "app", &registry).unwrap();
    let analyzer = Analyzer::new(&graph, &registry, &cfg, root, exec.cas());
    let g = analyzer.analyze(&label).unwrap();

    let actions: Vec<Action> = g.actions().cloned().collect();
    assert_eq!(actions.len(), 1, "step 1 emits only the install action");

    let results = exec.execute_graph(&actions).unwrap();
    let idx = install_action(&actions);
    assert!(
        results[idx].success(),
        "pnpm install failed (exit {})",
        results[idx].exit_code
    );
}

#[test]
fn install_is_snapshot_cacheable_across_runs() {
    // install is SnapshotBased + Sealed ⇒ cacheable: a first run is a miss, an
    // identical second run (same lockfile, toolchain, platform) hits the action cache
    // and skips re-running pnpm.
    let tmp = fixture();
    let root = tmp.path();
    let registry = builtin_rules();
    let cfg = debug();
    let exec = LocalExecutor::new(root.join(".mybuild")).unwrap();
    let label = Label::parse("//app:app").unwrap();

    let run = |exec: &LocalExecutor| {
        let graph = load_package(root, "app", &registry).unwrap();
        let g = Analyzer::new(&graph, &registry, &cfg, root, exec.cas())
            .analyze(&label)
            .unwrap();
        let actions: Vec<Action> = g.actions().cloned().collect();
        let results = exec.execute_graph(&actions).unwrap();
        results[install_action(&actions)].clone()
    };

    let first = run(&exec);
    assert!(first.success(), "first install failed (exit {})", first.exit_code);
    assert!(!first.cache_hit, "first install is a cache miss");

    let second = run(&exec);
    assert!(second.cache_hit, "identical re-run hits the install action cache");
}
