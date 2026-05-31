//! Phase 4 (§13.1, §6.3, §5.6): nickel_eval evaluates a Nickel file to a chosen
//! format, is configuration-invariant, validates the format against capability, and
//! exposes the output as a routable provider.

use anneal_analysis::{ActionGraph, AnalysisError, Analyzer};
use anneal_core::{AxisValues, Configuration, Label, OptLevel, Platform};
use anneal_exec::{action_digest, Action, Executor, LocalExecutor};
use anneal_loader::load_package;
use anneal_rules::{builtin_rules, ArtifactSource};

const CONFIG_NCL: &str = "{ name = \"demo\", port = 8080, urls = [\"a\", \"b\"] }\n";

/// A package `cfg/` containing `config.ncl` and the given `BUILD` body.
fn workspace(build: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = tmp.path().join("cfg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(pkg.join("config.ncl"), CONFIG_NCL).unwrap();
    std::fs::write(pkg.join("BUILD"), build).unwrap();
    tmp
}

fn analyze(
    root: &std::path::Path,
    target: &str,
    config: &Configuration,
    exec: &LocalExecutor,
) -> Result<ActionGraph, AnalysisError> {
    let registry = builtin_rules();
    let graph = load_package(root, "cfg", &registry).unwrap();
    Analyzer::new(&graph, &registry, config, root, exec.cas()).analyze(&Label::parse(target).unwrap())
}

fn host() -> Configuration {
    Configuration::new(Platform::new("host", "host"), AxisValues::default())
}

#[test]
fn evaluates_to_json_by_default_and_exposes_it() {
    let tmp = workspace("nickel_eval(name = \"config\", src = \"config.ncl\", out = \"config.json\")\n");
    let root = tmp.path();
    let exec = LocalExecutor::new(root.join(".mybuild")).unwrap();

    let g = analyze(root, "//cfg:config", &host(), &exec).unwrap();
    assert_eq!(g.action_count(), 1);

    // Provider exposes the produced output as a routable Output artifact.
    let files = g
        .providers(&Label::parse("//cfg:config").unwrap())
        .unwrap()
        .files
        .as_ref()
        .unwrap();
    assert!(matches!(files.files[0].source, ArtifactSource::Output { .. }));

    let action = g.actions().next().unwrap().clone();
    let result = exec.execute(&action).unwrap();
    assert!(result.success(), "nickel export failed (exit {})", result.exit_code);
    let json =
        String::from_utf8(exec.cas().get(result.outputs.get("config.json").unwrap()).unwrap().unwrap())
            .unwrap();
    assert!(json.contains("\"demo\"") && json.contains("8080"), "got: {json}");
}

#[test]
fn produces_toml_when_requested() {
    let tmp = workspace(
        "nickel_eval(name = \"config\", src = \"config.ncl\", format = \"toml\", out = \"config.toml\")\n",
    );
    let root = tmp.path();
    let exec = LocalExecutor::new(root.join(".mybuild")).unwrap();

    let g = analyze(root, "//cfg:config", &host(), &exec).unwrap();
    let result = exec.execute(&g.actions().next().unwrap().clone()).unwrap();
    assert!(result.success());
    let toml =
        String::from_utf8(exec.cas().get(result.outputs.get("config.toml").unwrap()).unwrap().unwrap())
            .unwrap();
    // TOML rendering of the same Nickel value.
    assert!(toml.contains("name = \"demo\""), "expected TOML, got: {toml}");
    assert!(toml.contains("port = 8080"));
}

#[test]
fn format_changes_the_cache_key() {
    let tmp = workspace(
        "nickel_eval(name = \"j\", src = \"config.ncl\", format = \"json\", out = \"c.json\")\n\
         nickel_eval(name = \"t\", src = \"config.ncl\", format = \"toml\", out = \"c.toml\")\n",
    );
    let root = tmp.path();
    let exec = LocalExecutor::new(root.join(".mybuild")).unwrap();

    let pick = |target: &str| -> Action {
        analyze(root, target, &host(), &exec)
            .unwrap()
            .actions()
            .next()
            .unwrap()
            .clone()
    };
    // Same source, different format ⇒ different command ⇒ different cache key.
    assert_ne!(action_digest(&pick("//cfg:j")), action_digest(&pick("//cfg:t")));
}

#[test]
fn is_configuration_invariant() {
    let tmp = workspace("nickel_eval(name = \"config\", src = \"config.ncl\", out = \"config.json\")\n");
    let root = tmp.path();
    let exec = LocalExecutor::new(root.join(".mybuild")).unwrap();

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
    let a = analyze(root, "//cfg:config", &cfg_a, &exec).unwrap().actions().next().unwrap().clone();
    let b = analyze(root, "//cfg:config", &cfg_b, &exec).unwrap().actions().next().unwrap().clone();
    // No axes consumed and platform-independent ⇒ identical key across configurations.
    assert_eq!(action_digest(&a), action_digest(&b));
}

#[test]
fn unsupported_format_is_rejected_at_the_rule_boundary() {
    let tmp = workspace("nickel_eval(name = \"bad\", src = \"config.ncl\", format = \"xml\")\n");
    let root = tmp.path();
    let exec = LocalExecutor::new(root.join(".mybuild")).unwrap();

    let err = analyze(root, "//cfg:bad", &host(), &exec).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("format") && msg.contains("xml"), "got: {msg}");
}
