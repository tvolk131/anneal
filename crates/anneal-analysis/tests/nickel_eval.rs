//! Phase 4 (§13.1, §6.3): nickel_eval evaluates a Nickel file to JSON, is
//! configuration-invariant, and exposes the JSON as a routable provider.

use anneal_analysis::Analyzer;
use anneal_core::{AxisValues, Configuration, Label, OptLevel, Platform};
use anneal_exec::{action_digest, Executor, LocalExecutor};
use anneal_loader::load_package;
use anneal_rules::{builtin_rules, ArtifactSource};

fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = tmp.path().join("cfg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("config.ncl"),
        "{ name = \"demo\", port = 8080, urls = [\"a\", \"b\"] }\n",
    )
    .unwrap();
    std::fs::write(
        pkg.join("BUILD"),
        "nickel_eval(name = \"config\", src = \"config.ncl\", out = \"config.json\")\n",
    )
    .unwrap();
    tmp
}

fn analyze(root: &std::path::Path, config: &Configuration, exec: &LocalExecutor) -> anneal_analysis::ActionGraph {
    let registry = builtin_rules();
    let graph = load_package(root, "cfg", &registry).unwrap();
    Analyzer::new(&graph, &registry, config, root, exec.cas())
        .analyze(&Label::parse("//cfg:config").unwrap())
        .unwrap()
}

#[test]
fn evaluates_nickel_to_json_and_exposes_it() {
    let tmp = fixture();
    let root = tmp.path();
    let exec = LocalExecutor::new(root.join(".mybuild")).unwrap();
    let cfg = Configuration::new(Platform::new("host", "host"), AxisValues::default());

    let g = analyze(root, &cfg, &exec);
    assert_eq!(g.action_count(), 1);

    // The provider exposes the produced JSON as a routable Output artifact.
    let providers = g.providers(&Label::parse("//cfg:config").unwrap()).unwrap();
    let files = providers.files.as_ref().expect("a FileSet");
    assert_eq!(files.files.len(), 1);
    assert!(matches!(files.files[0].source, ArtifactSource::Output { .. }));

    // Execute: Nickel evaluates the file to JSON.
    let action = g.actions().next().unwrap().clone();
    let result = exec.execute(&action).unwrap();
    assert!(result.success(), "nickel export should succeed (exit {})", result.exit_code);
    let json = String::from_utf8(
        exec.cas().get(result.outputs.get("config.json").unwrap()).unwrap().unwrap(),
    )
    .unwrap();
    assert!(json.contains("\"demo\""), "JSON should contain the evaluated data: {json}");
    assert!(json.contains("8080"));
}

#[test]
fn is_configuration_invariant() {
    let tmp = fixture();
    let root = tmp.path();
    let exec = LocalExecutor::new(root.join(".mybuild")).unwrap();

    // Two genuinely different configurations: different platform AND different axes.
    let cfg_a = Configuration::new(
        Platform::new("linux", "x86_64-unknown-linux-gnu"),
        AxisValues::default(),
    );
    let cfg_b = Configuration::new(
        Platform::new("mac", "aarch64-apple-darwin"),
        AxisValues {
            opt_level: OptLevel::Release,
            ..Default::default()
        },
    );

    let action_a = analyze(root, &cfg_a, &exec).actions().next().unwrap().clone();
    let action_b = analyze(root, &cfg_b, &exec).actions().next().unwrap().clone();

    // No axes consumed and platform-independent ⇒ identical cache key, so one
    // evaluation is shared across every configuration (§6.3).
    assert_eq!(
        action_digest(&action_a),
        action_digest(&action_b),
        "nickel_eval must be configuration-invariant"
    );
}
